// Parakeet ANE sidecar.
//
// A long-lived process that loads the Parakeet v2 (English) CoreML model via
// FluidAudio — which runs on the Apple Neural Engine — and serves transcription
// to the Muni app over stdin/stdout. It is byte-compatible with the Rust ONNX
// sidecar (crates/parakeet-sidecar) and the app-side client
// (apps/desktop/src-tauri/src/parakeet.rs), so the app can spawn either binary.
//
// ## Wire protocol (length-prefixed binary, little-endian)
// One frame = [u32 len][len bytes payload], same in both directions.
// - Handshake (sidecar→app, once after model load): payload = "READY".
// - Request (app→sidecar): payload = raw i16 PCM little-endian, 16 kHz mono.
// - Response (sidecar→app): payload = [u32 infer_ms][utf8 transcript]; on a
//   failure the body is infer_ms=0 + "ERR:<message>".
//
// FluidAudio manages the model itself (auto-downloads to its own cache on first
// use), so this binary ignores argv — there is no model-dir argument.
//
// ## Frame-size cap
// The u32 length prefix is bounded by `maxFrameBytes` (64 MiB). A prefix above
// the cap is treated as protocol corruption: the sidecar replies with an "ERR:"
// protocol-error frame and exits cleanly instead of blocking forever
// accumulating gigabytes. Mirrors the Rust ONNX sidecar and the app client.

import FluidAudio
import Foundation

// Ignore SIGPIPE so a write to a dead parent's pipe surfaces as a throwing
// error we can handle (see `writeFrame`) instead of killing this process with a
// signal — which would otherwise show up as crash-reporter noise.
signal(SIGPIPE, SIG_IGN)

// ── framing ─────────────────────────────────────────────────────────────

/// Maximum accepted frame payload size: 64 MiB (≈30+ min of 16 kHz i16 PCM).
/// A length prefix above this is protocol corruption, not a real request. MUST
/// stay in sync with the Rust sidecar (`crates/parakeet-sidecar`) and the app
/// client (`apps/desktop/src-tauri/src/parakeet.rs`).
let maxFrameBytes: UInt32 = 64 * 1024 * 1024

/// Outcome of reading the next length-prefixed frame from the peer.
enum FrameRead {
    /// A payload within the size cap, ready to process.
    case payload(Data)
    /// The peer closed the stream cleanly (EOF on the length prefix).
    case eof
    /// The length prefix exceeded `maxFrameBytes`; carries the declared length.
    /// The body is deliberately NOT read — that is the point (no unbounded
    /// accumulation).
    case oversized(UInt32)
}

/// Read exactly `count` bytes, or nil on EOF / error (pipe closed → exit loop).
func readExact(_ handle: FileHandle, _ count: Int) -> Data? {
    var out = Data()
    out.reserveCapacity(count)
    while out.count < count {
        guard let chunk = try? handle.read(upToCount: count - out.count), !chunk.isEmpty else {
            return nil
        }
        out.append(chunk)
    }
    return out
}

/// Read one length-prefixed frame. Never accumulates the body for a prefix above
/// `maxFrameBytes` — that case short-circuits to `.oversized` before any read.
func readFrame(_ handle: FileHandle) -> FrameRead {
    guard let header = readExact(handle, 4) else { return .eof }
    let b = [UInt8](header)
    let len = UInt32(b[0]) | (UInt32(b[1]) << 8) | (UInt32(b[2]) << 16) | (UInt32(b[3]) << 24)
    if len > maxFrameBytes { return .oversized(len) }
    if len == 0 { return .payload(Data()) }
    guard let body = readExact(handle, Int(len)) else { return .eof }
    return .payload(body)
}

/// Write one length-prefixed frame. EPIPE-safe: if the parent (Muni) closed the
/// pipe, the throwing write surfaces the error and we exit cleanly rather than
/// letting FileHandle raise an ObjC exception (crash-reporter noise).
func writeFrame(_ handle: FileHandle, _ payload: Data) {
    let len = UInt32(payload.count)
    var frame = Data([
        UInt8(len & 0xff),
        UInt8((len >> 8) & 0xff),
        UInt8((len >> 16) & 0xff),
        UInt8((len >> 24) & 0xff),
    ])
    frame.append(payload)
    do {
        try handle.write(contentsOf: frame)
    } catch {
        // Parent gone (EPIPE) or pipe otherwise unwritable: nothing left to
        // serve, so exit cleanly. Use exit(0) — this is an expected shutdown,
        // not a crash.
        logErr("[parakeet-ane-sidecar] write failed (parent gone?): \(error); exiting")
        exit(0)
    }
}

/// Decode an i16 little-endian PCM payload into normalised f32 samples.
func pcmToFloat(_ data: Data) -> [Float] {
    let bytes = [UInt8](data)
    var samples = [Float]()
    samples.reserveCapacity(bytes.count / 2)
    var i = 0
    while i + 1 < bytes.count {
        let u = UInt16(bytes[i]) | (UInt16(bytes[i + 1]) << 8)
        samples.append(Float(Int16(bitPattern: u)) / 32768.0)
        i += 2
    }
    return samples
}

func okResponse(_ inferMs: UInt32, _ text: String) -> Data {
    var d = Data([
        UInt8(inferMs & 0xff),
        UInt8((inferMs >> 8) & 0xff),
        UInt8((inferMs >> 16) & 0xff),
        UInt8((inferMs >> 24) & 0xff),
    ])
    d.append(text.data(using: .utf8) ?? Data())
    return d
}

func errResponse(_ message: String) -> Data {
    okResponse(0, "ERR:\(message)")
}

func logErr(_ s: String) {
    guard let data = (s + "\n").data(using: .utf8) else { return }
    // Best-effort: if stderr is closed (parent gone) don't let a write raise —
    // swallow it, we're likely on our way out anyway.
    try? FileHandle.standardError.write(contentsOf: data)
}

// ── serve ───────────────────────────────────────────────────────────────

logErr("[parakeet-ane-sidecar] loading FluidAudio v2 (English) on the ANE…")
let loadStart = Date()
let asr: AsrManager
do {
    let models = try await AsrModels.downloadAndLoad(version: .v2)
    asr = AsrManager(config: .default)
    try await asr.loadModels(models)
} catch {
    logErr("[parakeet-ane-sidecar] model load failed: \(error)")
    exit(1)
}
logErr("[parakeet-ane-sidecar] model loaded in \(Int(Date().timeIntervalSince(loadStart) * 1000)) ms")

let stdinHandle = FileHandle.standardInput
let stdoutHandle = FileHandle.standardOutput

// Handshake: model is warm, ready to serve.
writeFrame(stdoutHandle, "READY".data(using: .utf8)!)

serveLoop: while true {
    switch readFrame(stdinHandle) {
    case .eof:
        break serveLoop
    case .oversized(let len):
        logErr(
            "[parakeet-ane-sidecar] oversized frame prefix: \(len) bytes exceeds "
                + "\(maxFrameBytes) cap; sending protocol error and exiting")
        writeFrame(
            stdoutHandle,
            errResponse("frame too large: \(len) bytes exceeds \(maxFrameBytes) cap"))
        break serveLoop
    case .payload(let payload):
        do {
            let samples = pcmToFloat(payload)
            var decoderState = try TdtDecoderState()
            let inferStart = Date()
            let result = try await asr.transcribe(samples, decoderState: &decoderState)
            let inferMs = UInt32(max(0, Date().timeIntervalSince(inferStart) * 1000))
            let text = result.text.trimmingCharacters(in: .whitespacesAndNewlines)
            writeFrame(stdoutHandle, okResponse(inferMs, text))
        } catch {
            logErr("[parakeet-ane-sidecar] transcribe error: \(error)")
            writeFrame(stdoutHandle, errResponse("\(error)"))
        }
    }
}
