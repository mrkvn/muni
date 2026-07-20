//! Parakeet ASR sidecar.
//!
//! A long-lived process that loads the Parakeet ONNX model ONCE (warm-resident)
//! and serves transcription requests from the Muni app over stdin/stdout. It
//! exists as a separate binary because `parakeet-rs` pulls `ort` 2.0.0-rc.12,
//! which cannot be linked into the app (the app's VAD pins `ort` =2.0.0-rc.10
//! and ort-sys uses `links = "onnxruntime"`, so Cargo forbids both in one
//! binary). See docs/research/09-nvidia-parakeet-evaluation.md.
//!
//! ## Wire protocol (length-prefixed binary, little-endian)
//!
//! One frame = `[u32 len][len bytes payload]`. Symmetric in both directions.
//! The app-side counterpart lives in `apps/desktop/src-tauri/src/parakeet.rs`
//! and MUST stay byte-compatible with this file.
//!
//! - Handshake (sidecar → app, once, after the model finishes loading):
//!   payload = ASCII `"READY"`. The app blocks (with a timeout) for this before
//!   marking the sidecar usable.
//! - Request (app → sidecar): payload = raw `i16` PCM little-endian, 16 kHz
//!   mono — exactly the shape the app already has buffered.
//! - Response (sidecar → app): payload = `[u32 infer_ms][utf8 transcript]`.
//!   On a transcription error the body is `infer_ms = 0` followed by
//!   `ERR:<message>`; the app treats any text starting with `ERR:` as failure
//!   and falls back to Deepgram for that press.
//!
//! Exactly one request is in flight at a time (the app serialises dictation),
//! so no request IDs are needed. When the app closes stdin (EOF), the loop
//! ends and the process exits cleanly.
//!
//! ## Frame-size cap
//!
//! The `u32` length prefix is bounded by [`MAX_FRAME_BYTES`] (64 MiB). A prefix
//! above the cap is treated as protocol corruption (a wedged or hostile peer):
//! the sidecar replies with an `ERR:` protocol-error frame and exits cleanly
//! instead of allocating a multi-gigabyte buffer. The app-side client mirrors
//! the same cap and treats the exit as a sidecar restart.

use std::io::{self, Read, Write};

use anyhow::{Context, Result};
use parakeet_rs::{Parakeet, TimestampMode, Transcriber};

/// Parakeet operates on 16 kHz mono audio; the app captures at this rate.
const SAMPLE_RATE: u32 = 16_000;
const CHANNELS: u16 = 1;

/// Maximum accepted frame payload size: 64 MiB (≈30+ min of 16 kHz i16 PCM).
/// A length prefix above this is protocol corruption, not a real request — we
/// refuse it rather than allocate a buffer that could exhaust memory. MUST stay
/// in sync with the app client (`apps/desktop/src-tauri/src/parakeet.rs`) and
/// the Swift ANE sidecar (`crates/parakeet-ane-sidecar`).
const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// Outcome of reading the next length-prefixed frame from the peer.
#[derive(Debug)]
enum FrameRead {
    /// A payload within the size cap, ready to process.
    Payload(Vec<u8>),
    /// The peer closed the stream cleanly (EOF on the length prefix).
    Eof,
    /// The length prefix exceeded [`MAX_FRAME_BYTES`]; carries the declared
    /// length for logging. The body is deliberately NOT read (that is the whole
    /// point — no oversized allocation).
    Oversized(u32),
}

/// Read one length-prefixed frame. Never allocates the payload buffer for a
/// prefix above [`MAX_FRAME_BYTES`] — that case short-circuits to
/// [`FrameRead::Oversized`] before any allocation.
fn read_frame(reader: &mut impl Read) -> io::Result<FrameRead> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(FrameRead::Eof),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Ok(FrameRead::Oversized(len));
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload)?;
    Ok(FrameRead::Payload(payload))
}

/// Write one length-prefixed frame and flush so the app sees it immediately.
fn write_frame(writer: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    writer.write_all(&(payload.len() as u32).to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()
}

/// Decode an `i16` little-endian PCM request payload into normalised f32 samples
/// in [-1.0, 1.0], which is what `transcribe_samples` expects. A trailing odd
/// byte (malformed frame) is ignored rather than panicking.
fn pcm_i16_to_f32(payload: &[u8]) -> Vec<f32> {
    payload
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect()
}

/// Build a success response payload: `[u32 infer_ms][utf8 transcript]`.
fn encode_ok(infer_ms: u32, text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + text.len());
    out.extend_from_slice(&infer_ms.to_le_bytes());
    out.extend_from_slice(text.as_bytes());
    out
}

/// Build an error response payload: `[u32 0]ERR:<message>`.
fn encode_err(message: &str) -> Vec<u8> {
    let body = format!("ERR:{message}");
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(body.as_bytes());
    out
}

/// Protocol-error message for an oversized length prefix.
fn oversized_message(len: u32) -> String {
    format!("frame too large: {len} bytes exceeds {MAX_FRAME_BYTES} cap")
}

/// Serve requests until the peer closes stdin (EOF) or sends an oversized frame.
///
/// `transcribe` maps a raw request payload to `(infer_ms, transcript)` or an
/// error string; it is injected so the loop is testable without the ONNX model.
/// The framing/cap/protocol-error policy lives here (not in `main`) so every
/// branch — including the oversized-frame exit — is exercised by unit tests.
fn serve<R, W, F>(reader: &mut R, writer: &mut W, mut transcribe: F) -> Result<()>
where
    R: Read,
    W: Write,
    F: FnMut(&[u8]) -> Result<(u32, String), String>,
{
    loop {
        match read_frame(reader).context("read request frame")? {
            FrameRead::Eof => break,
            FrameRead::Oversized(len) => {
                eprintln!(
                    "[parakeet-sidecar] oversized frame prefix: {len} bytes exceeds \
                     {MAX_FRAME_BYTES} cap; sending protocol error and exiting"
                );
                // Best-effort: the peer is misbehaving and we exit regardless,
                // so a write failure here does not change the outcome.
                let _ = write_frame(writer, &encode_err(&oversized_message(len)));
                break;
            }
            FrameRead::Payload(payload) => {
                let response = match transcribe(&payload) {
                    Ok((infer_ms, text)) => encode_ok(infer_ms, text.trim()),
                    Err(e) => {
                        eprintln!("[parakeet-sidecar] transcribe error: {e}");
                        encode_err(&e)
                    }
                };
                write_frame(writer, &response).context("write response frame")?;
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let model_dir = std::env::args()
        .nth(1)
        .context("usage: parakeet-sidecar <model_dir>")?;

    eprintln!("[parakeet-sidecar] loading model from {model_dir}");
    let load_start = std::time::Instant::now();
    let mut parakeet = Parakeet::from_pretrained(&model_dir, None)
        .with_context(|| format!("load Parakeet model from {model_dir}"))?;
    eprintln!(
        "[parakeet-sidecar] model loaded in {} ms",
        load_start.elapsed().as_millis()
    );

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    // Handshake: tell the app the model is warm and we're ready to serve.
    write_frame(&mut writer, b"READY").context("write READY handshake")?;

    // Serve requests until the app closes stdin (or sends an oversized frame).
    serve(&mut reader, &mut writer, |payload| {
        let audio = pcm_i16_to_f32(payload);
        let infer_start = std::time::Instant::now();
        // `Words` mode regroups SentencePiece subword tokens into whole words;
        // the default (`None` → `Tokens`) emits fragmented pieces like
        // "par a ke et", so we must request word grouping for readable text.
        match parakeet.transcribe_samples(audio, SAMPLE_RATE, CHANNELS, Some(TimestampMode::Words))
        {
            Ok(result) => Ok((infer_start.elapsed().as_millis() as u32, result.text)),
            Err(e) => Err(e.to_string()),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A 4-byte prefix declaring more than the cap, with NO body bytes, must
    /// short-circuit to `Oversized` — never attempt to allocate/read the body.
    /// (If it fell through to `read_exact` on the body it would hit EOF and
    /// return `Err`, so `Ok(Oversized)` proves the allocation was skipped.)
    #[test]
    fn read_frame_rejects_oversized_prefix_without_allocating() {
        let len = MAX_FRAME_BYTES + 1;
        let mut cursor = Cursor::new(len.to_le_bytes().to_vec());
        match read_frame(&mut cursor).expect("no io error for a bare oversized prefix") {
            FrameRead::Oversized(got) => assert_eq!(got, len),
            other => panic!("expected Oversized, got {other:?}"),
        }
    }

    #[test]
    fn read_frame_accepts_in_cap_payload() {
        // A normal in-cap frame round-trips to its payload (the reject boundary
        // is `len > cap`, so real requests are unaffected).
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hi").unwrap();
        let mut cursor = Cursor::new(buf);
        match read_frame(&mut cursor).unwrap() {
            FrameRead::Payload(p) => assert_eq!(p, b"hi"),
            other => panic!("expected Payload, got {other:?}"),
        }
    }

    #[test]
    fn read_frame_reports_eof_on_clean_close() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        assert!(matches!(read_frame(&mut cursor).unwrap(), FrameRead::Eof));
    }

    /// The end-to-end protocol contract for task 68: a bogus/oversized prefix
    /// makes `serve` reply with a single `ERR:` protocol-error frame and exit
    /// cleanly, without ever invoking the transcriber or allocating the body.
    #[test]
    fn serve_replies_protocol_error_and_exits_on_oversized_prefix() {
        let len = MAX_FRAME_BYTES + 4096;
        let mut reader = Cursor::new(len.to_le_bytes().to_vec());
        let mut writer: Vec<u8> = Vec::new();
        let mut transcribe_called = false;

        serve(&mut reader, &mut writer, |_payload| {
            transcribe_called = true;
            Ok((0, String::new()))
        })
        .expect("serve exits cleanly (Ok) on an oversized frame");

        assert!(
            !transcribe_called,
            "transcriber must never run for an oversized frame"
        );

        // Exactly one frame was written, and it is an ERR protocol error.
        let mut resp = Cursor::new(writer);
        let payload = match read_frame(&mut resp).unwrap() {
            FrameRead::Payload(p) => p,
            other => panic!("expected an error response frame, got {other:?}"),
        };
        assert_eq!(
            &payload[..4],
            &0u32.to_le_bytes(),
            "error frame carries infer_ms == 0"
        );
        let body = String::from_utf8(payload[4..].to_vec()).unwrap();
        assert!(
            body.starts_with("ERR:"),
            "body must be an ERR frame: {body}"
        );
        assert!(
            body.contains("too large") && body.contains("exceeds"),
            "message should describe the cap breach: {body}"
        );
        // Nothing more was written after the single error frame.
        assert!(matches!(read_frame(&mut resp).unwrap(), FrameRead::Eof));
    }

    #[test]
    fn serve_transcribes_in_cap_frames_then_exits_on_eof() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &[0x02, 0x01]).unwrap(); // one i16 sample
        let mut reader = Cursor::new(buf);
        let mut writer: Vec<u8> = Vec::new();

        serve(&mut reader, &mut writer, |payload| {
            Ok((7, format!("got {} bytes", payload.len())))
        })
        .unwrap();

        let mut resp = Cursor::new(writer);
        match read_frame(&mut resp).unwrap() {
            FrameRead::Payload(frame) => {
                assert_eq!(&frame[..4], &7u32.to_le_bytes());
                assert_eq!(&frame[4..], b"got 2 bytes");
            }
            other => panic!("expected a response payload, got {other:?}"),
        }
    }

    #[test]
    fn serve_forwards_transcribe_errors_as_err_frames() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &[0x00, 0x00]).unwrap();
        let mut reader = Cursor::new(buf);
        let mut writer: Vec<u8> = Vec::new();

        serve(&mut reader, &mut writer, |_payload| Err("boom".to_string())).unwrap();

        let mut resp = Cursor::new(writer);
        let payload = match read_frame(&mut resp).unwrap() {
            FrameRead::Payload(p) => p,
            other => panic!("{other:?}"),
        };
        let body = String::from_utf8(payload[4..].to_vec()).unwrap();
        assert_eq!(body, "ERR:boom");
    }
}
