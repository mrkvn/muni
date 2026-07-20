//! Parakeet local-ASR sidecar client.
//!
//! Talks to the `parakeet-sidecar` binary (see `crates/parakeet-sidecar`) over
//! its stdin/stdout, because Parakeet's `ort` 2.0.0-rc.12 cannot be linked into
//! this binary (the VAD pins `ort` =2.0.0-rc.10 and ort-sys uses
//! `links = "onnxruntime"`). The sidecar loads the model once and stays warm;
//! this client spawns it at boot, waits for its `READY` handshake, and sends
//! one full-clip PCM request per press.
//!
//! Failure philosophy: every failure here is recoverable by falling back to
//! Deepgram. `spawn` failing → `parakeet: None` at boot (English stays on
//! Deepgram). `transcribe` failing → the English arm of `finalize_auto_detect`
//! finalizes Deepgram instead. The sidecar is lazily respawned on the next
//! press after a crash, so a transient failure self-heals.
//!
//! The wire protocol is mirrored byte-for-byte in `crates/parakeet-sidecar`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Manager, Runtime};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex as TokioMutex;
use tokio::time::timeout;

use crate::error::MuniError;

/// `MUNI_ASR_BACKEND=parakeet` selects Parakeet for the English path.
pub const ASR_BACKEND_ENV: &str = "MUNI_ASR_BACKEND";
/// Overrides the model directory (must contain `model.onnx` + tokenizer).
pub const MODEL_DIR_ENV: &str = "MUNI_PARAKEET_MODEL_DIR";
/// Overrides the sidecar binary path (escape hatch for non-standard layouts).
pub const BIN_ENV: &str = "MUNI_PARAKEET_BIN";
/// Selects the inference engine: `onnx` (Rust/CPU, default) or `ane`
/// (Swift/CoreML on the Apple Neural Engine — faster, Apple Silicon only).
pub const ENGINE_ENV: &str = "MUNI_PARAKEET_ENGINE";

/// Sidecar executable names per engine.
const SIDECAR_BIN_NAME: &str = "parakeet-sidecar";
const ANE_SIDECAR_BIN_NAME: &str = "parakeet-ane-sidecar";

/// READY-handshake ceiling. Warm load is ~0.6-2 s, but a COLD first run of the
/// ANE engine pays a one-time CoreML→ANE compilation (~85 s observed) on top of
/// the model download, so the ceiling is generous. Onnx warm is ~2 s.
const READY_TIMEOUT: Duration = Duration::from_secs(150);
/// A single dictation clip transcribes in ~0.1-0.7 s; this only guards against
/// a wedged sidecar so the release path can't hang.
const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum accepted frame payload size: 64 MiB. A length prefix above this is
/// protocol corruption from a wedged/hostile sidecar; we refuse it (rather than
/// allocate a multi-gigabyte buffer) and let the error route into the existing
/// respawn path (task 18). MUST stay in sync with the sidecar crates
/// (`crates/parakeet-sidecar`, `crates/parakeet-ane-sidecar`).
const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// Which on-device inference engine backs Parakeet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Engine {
    /// Rust ONNX Runtime on CPU (`crates/parakeet-sidecar`). Default.
    Onnx,
    /// Swift FluidAudio CoreML on the Apple Neural Engine
    /// (`crates/parakeet-ane-sidecar`). Self-manages its model.
    Ane,
}

/// True when the user selected Parakeet via `MUNI_ASR_BACKEND=parakeet`
/// (case-insensitive, trimmed). Default backend is Deepgram.
pub fn is_selected() -> bool {
    std::env::var(ASR_BACKEND_ENV)
        .map(|v| v.trim().eq_ignore_ascii_case("parakeet"))
        .unwrap_or(false)
}

/// Resolve the engine from `MUNI_PARAKEET_ENGINE` (default `onnx`).
pub fn resolve_engine() -> Engine {
    match std::env::var(ENGINE_ENV) {
        Ok(v) if v.trim().eq_ignore_ascii_case("ane") => Engine::Ane,
        _ => Engine::Onnx,
    }
}

// ───────────────────────── wire protocol (pure, testable) ─────────────────────────

/// Encode a request payload: raw `i16` PCM little-endian. The length prefix is
/// added by [`write_frame`]; this is just the body. Delegates to the shared
/// streaming-ASR PCM encoder so the little-endian layout is defined once.
fn encode_pcm_payload(samples: &[i16]) -> Vec<u8> {
    crate::asr_stream::i16_slice_to_le_bytes(samples)
}

/// Decode a response payload `[u32 infer_ms][utf8 transcript]` into
/// `(text, infer_ms)`. A body starting with `ERR:` is a sidecar-side failure.
fn decode_response(payload: &[u8]) -> Result<(String, u32), MuniError> {
    if payload.len() < 4 {
        return Err(MuniError::ParakeetTranscribeFailed {
            reason: format!("short response frame ({} bytes)", payload.len()),
        });
    }
    let infer_ms = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let text = String::from_utf8_lossy(&payload[4..]).into_owned();
    if let Some(msg) = text.strip_prefix("ERR:") {
        return Err(MuniError::ParakeetTranscribeFailed {
            reason: msg.to_string(),
        });
    }
    Ok((text, infer_ms))
}

/// Read one length-prefixed frame. `Ok(None)` means the peer closed the stream
/// (EOF on the length prefix) — i.e. the sidecar exited.
async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        // A sane sidecar never sends a frame this large; treat an oversized
        // prefix as protocol corruption and surface an error instead of
        // allocating gigabytes. `roundtrip` maps this to a transcribe failure,
        // which drops the sidecar and kicks a respawn (task 18).
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds {MAX_FRAME_BYTES} byte cap"),
        ));
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

/// Write one length-prefixed frame and flush.
async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> std::io::Result<()> {
    writer
        .write_all(&(payload.len() as u32).to_le_bytes())
        .await?;
    writer.write_all(payload).await?;
    writer.flush().await
}

// ───────────────────────── live sidecar process ─────────────────────────

/// A running sidecar: the child plus its piped stdin/stdout. Dropping it kills
/// the process (`kill_on_drop`), so replacing it on crash never leaks a zombie.
struct Sidecar {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Sidecar {
    /// Spawn the sidecar and block until it reports `READY` (model loaded) or
    /// the boot timeout elapses. stderr is inherited so model-load logs/errors
    /// land in the app log.
    async fn start(bin: &Path, model_dir: Option<&Path>) -> Result<Self, MuniError> {
        // The ONNX sidecar takes the model dir as argv[1]; the ANE sidecar
        // self-manages its model (FluidAudio cache), so it gets no argument.
        let mut command = Command::new(bin);
        if let Some(dir) = model_dir {
            command.arg(dir);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| MuniError::ParakeetSidecarUnavailable {
                reason: format!("spawn {}: {e}", bin.display()),
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| MuniError::ParakeetSidecarUnavailable {
                reason: "child stdin pipe missing".to_string(),
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| MuniError::ParakeetSidecarUnavailable {
                reason: "child stdout pipe missing".to_string(),
            })?;
        let mut sidecar = Sidecar {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };

        let frame = timeout(READY_TIMEOUT, read_frame(&mut sidecar.stdout))
            .await
            .map_err(|_| MuniError::ParakeetSidecarUnavailable {
                reason: format!("no READY within {}s", READY_TIMEOUT.as_secs()),
            })?
            .map_err(|e| MuniError::ParakeetSidecarUnavailable {
                reason: format!("reading READY: {e}"),
            })?;

        match frame.as_deref() {
            Some(b"READY") => Ok(sidecar),
            other => Err(MuniError::ParakeetSidecarUnavailable {
                reason: format!("unexpected handshake: {other:?}"),
            }),
        }
    }

    /// Send one PCM request and read the transcript response, bounded by a
    /// timeout so a wedged sidecar can't stall the release path.
    async fn roundtrip(&mut self, request: &[u8]) -> Result<(String, u32), MuniError> {
        let exchange = async {
            write_frame(&mut self.stdin, request).await.map_err(|e| {
                MuniError::ParakeetTranscribeFailed {
                    reason: format!("write request: {e}"),
                }
            })?;
            let frame = read_frame(&mut self.stdout)
                .await
                .map_err(|e| MuniError::ParakeetTranscribeFailed {
                    reason: format!("read response: {e}"),
                })?
                .ok_or_else(|| MuniError::ParakeetTranscribeFailed {
                    reason: "sidecar closed pipe".to_string(),
                })?;
            decode_response(&frame)
        };

        timeout(TRANSCRIBE_TIMEOUT, exchange).await.map_err(|_| {
            MuniError::ParakeetTranscribeFailed {
                reason: format!("no response within {}s", TRANSCRIBE_TIMEOUT.as_secs()),
            }
        })?
    }
}

// ───────────────────────── public client ─────────────────────────

/// Clears the respawn-in-progress flag on drop so a panic or early return in
/// the background respawn task can never wedge the flag `true` and permanently
/// block all future respawns. (Learned 013 drop-guard pattern applied to a
/// spawned task that can be aborted or panic.)
struct RespawnFlagGuard(Arc<AtomicBool>);

impl Drop for RespawnFlagGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Warm Parakeet sidecar handle. Construct once at boot via [`spawn`]; share as
/// `Arc<ParakeetClient>` in `SessionDeps`.
pub struct ParakeetClient {
    bin: PathBuf,
    /// `Some` for the ONNX engine (passed as argv[1]); `None` for the ANE
    /// engine, which self-manages its model via FluidAudio's cache.
    model_dir: Option<PathBuf>,
    pid: Option<u32>,
    /// `Arc` so a background respawn task (see [`ParakeetClient::kick_respawn`])
    /// can own a handle to swap a fresh sidecar back in without blocking the
    /// press path.
    proc: Arc<TokioMutex<Option<Sidecar>>>,
    /// `true` while a background respawn is in flight — a CAS gate so
    /// concurrent presses can't stack multiple respawns of the same sidecar.
    respawning: Arc<AtomicBool>,
    /// Count of background respawns kicked (observability + tests).
    respawn_kicks: Arc<AtomicUsize>,
}

impl ParakeetClient {
    /// Spawn the sidecar and wait for READY. Returns `Err` (mapped to
    /// `parakeet: None` by the caller) if the binary is missing, a required
    /// model dir is missing, or the sidecar never reports READY. Pass
    /// `model_dir = None` for the ANE engine (no model-dir argument).
    pub async fn spawn(bin: PathBuf, model_dir: Option<PathBuf>) -> Result<Self, MuniError> {
        if !bin.exists() {
            return Err(MuniError::ParakeetSidecarUnavailable {
                reason: format!("sidecar binary not found: {}", bin.display()),
            });
        }
        if let Some(dir) = &model_dir {
            if !dir.is_dir() {
                return Err(MuniError::ParakeetSidecarUnavailable {
                    reason: format!("model dir not found: {}", dir.display()),
                });
            }
        }
        let sidecar = Sidecar::start(&bin, model_dir.as_deref()).await?;
        let pid = sidecar.child.id();
        Ok(ParakeetClient {
            bin,
            model_dir,
            pid,
            proc: Arc::new(TokioMutex::new(Some(sidecar))),
            respawning: Arc::new(AtomicBool::new(false)),
            respawn_kicks: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Sidecar process id (for boot logging). `None` once it has exited.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Transcribe a full-clip 16 kHz mono PCM buffer.
    ///
    /// Fail-fast on the press path (plan 039 task 18): if the sidecar is not
    /// running — either it died on a previous press or a background respawn is
    /// still in flight — this returns a typed error **immediately** and kicks a
    /// background respawn, rather than reloading the model inline. A cold ANE
    /// load can take tens of seconds; blocking the release path on it would
    /// stall the paste. The caller (`finalize_auto_detect`'s English arm) treats
    /// the `Err` as "fall back to the live Deepgram stream for this press", and
    /// the *next* press finds a warm sidecar.
    pub async fn transcribe(&self, samples: &[i16]) -> Result<(String, u32), MuniError> {
        let request = encode_pcm_payload(samples);
        let mut guard = self.proc.lock().await;

        if guard.is_none() {
            drop(guard);
            self.kick_respawn();
            return Err(MuniError::ParakeetSidecarUnavailable {
                reason: "sidecar not running — background respawn kicked".to_string(),
            });
        }

        let sidecar = guard.as_mut().expect("sidecar present (checked is_some)");
        match sidecar.roundtrip(&request).await {
            Ok(result) => Ok(result),
            Err(e) => {
                // Drop the broken sidecar (kill_on_drop reaps it), then respawn
                // in the background so this press's caller isn't blocked on the
                // reload — it falls back to Deepgram while the next press warms.
                *guard = None;
                drop(guard);
                self.kick_respawn();
                Err(e)
            }
        }
    }

    /// Kick a background respawn unless one is already running.
    ///
    /// Idempotent under concurrent presses via the `respawning` CAS gate; the
    /// spawned task clears the gate on completion — including panic or abort —
    /// through [`RespawnFlagGuard`], so a failed respawn can never wedge the
    /// gate and starve future attempts.
    fn kick_respawn(&self) {
        if self.respawning.swap(true, Ordering::SeqCst) {
            // Already respawning — don't stack a second reload.
            return;
        }
        self.respawn_kicks.fetch_add(1, Ordering::SeqCst);
        let proc = self.proc.clone();
        let bin = self.bin.clone();
        let model_dir = self.model_dir.clone();
        let flag = RespawnFlagGuard(self.respawning.clone());
        tauri::async_runtime::spawn(async move {
            // Held for the whole task so the gate clears on every exit path.
            let _flag = flag;
            match Sidecar::start(&bin, model_dir.as_deref()).await {
                Ok(sidecar) => {
                    *proc.lock().await = Some(sidecar);
                    log::info!(target: "parakeet", "sidecar respawned after failure");
                }
                Err(e) => {
                    log::warn!(
                        target: "parakeet",
                        "background sidecar respawn failed: {} — next press retries",
                        e.user_message()
                    );
                }
            }
        });
    }

    /// Number of background respawns kicked over this client's lifetime.
    /// Exposed for boot/runtime observability and the fail-fast test.
    pub fn respawn_kicks(&self) -> usize {
        self.respawn_kicks.load(Ordering::SeqCst)
    }
}

// ───────────────────────── path resolution (used by lib.rs setup) ─────────────────────────

/// Resolve the sidecar binary: `MUNI_PARAKEET_BIN` → bundled next to resources
/// → dev source-tree target. Returns the first existing candidate, else the
/// last one tried (so the caller's `spawn` surfaces a clear "not found").
pub fn resolve_sidecar_bin<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    if let Ok(explicit) = std::env::var(BIN_ENV) {
        let p = PathBuf::from(explicit.trim());
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    if let Ok(rd) = app.path().resource_dir() {
        let bundled = rd.join(SIDECAR_BIN_NAME);
        if bundled.exists() {
            return bundled;
        }
    }
    #[cfg(debug_assertions)]
    {
        // <repo>/crates/parakeet-sidecar/target/release/parakeet-sidecar
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../crates/parakeet-sidecar/target/release")
            .join(SIDECAR_BIN_NAME)
    }
    #[cfg(not(debug_assertions))]
    {
        app.path()
            .resource_dir()
            .map(|rd| rd.join(SIDECAR_BIN_NAME))
            .unwrap_or_else(|_| PathBuf::from(SIDECAR_BIN_NAME))
    }
}

/// Resolve the ANE (Swift/FluidAudio) sidecar binary: `MUNI_PARAKEET_BIN` →
/// bundled next to resources → dev source-tree SwiftPM build output.
pub fn resolve_ane_sidecar_bin<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    if let Ok(explicit) = std::env::var(BIN_ENV) {
        let p = PathBuf::from(explicit.trim());
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    if let Ok(rd) = app.path().resource_dir() {
        let bundled = rd.join(ANE_SIDECAR_BIN_NAME);
        if bundled.exists() {
            return bundled;
        }
    }
    #[cfg(debug_assertions)]
    {
        // <repo>/crates/parakeet-ane-sidecar/.build/release/parakeet-ane-sidecar
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../crates/parakeet-ane-sidecar/.build/release")
            .join(ANE_SIDECAR_BIN_NAME)
    }
    #[cfg(not(debug_assertions))]
    {
        app.path()
            .resource_dir()
            .map(|rd| rd.join(ANE_SIDECAR_BIN_NAME))
            .unwrap_or_else(|_| PathBuf::from(ANE_SIDECAR_BIN_NAME))
    }
}

/// Resolve the model directory: `MUNI_PARAKEET_MODEL_DIR` → bundled
/// `resources/parakeet` → dev fallback to the spike's downloaded model.
pub fn resolve_model_dir<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    if let Ok(explicit) = std::env::var(MODEL_DIR_ENV) {
        let p = PathBuf::from(explicit.trim());
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    if let Ok(rd) = app.path().resource_dir() {
        let bundled = rd.join("resources").join("parakeet");
        if bundled.is_dir() {
            return bundled;
        }
    }
    #[cfg(debug_assertions)]
    {
        // <repo>/scripts/parakeet_spike/models/ctc-en (already downloaded by the spike)
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../scripts/parakeet_spike/models/ctc-en")
    }
    #[cfg(not(debug_assertions))]
    {
        app.path()
            .resource_dir()
            .map(|rd| rd.join("resources").join("parakeet"))
            .unwrap_or_else(|_| PathBuf::from("parakeet"))
    }
}

#[cfg(test)]
impl ParakeetClient {
    /// Build a client whose sidecar is already dead (`proc = None`) pointing at
    /// a bogus binary, so `transcribe` exercises the fail-fast + respawn-kick
    /// path without spawning a real sidecar process.
    fn dead_for_test(bin: PathBuf) -> Self {
        ParakeetClient {
            bin,
            model_dir: None,
            pid: None,
            proc: Arc::new(TokioMutex::new(None)),
            respawning: Arc::new(AtomicBool::new(false)),
            respawn_kicks: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transcribe_fails_fast_and_kicks_respawn_when_sidecar_dead() {
        let client = ParakeetClient::dead_for_test(PathBuf::from(
            "/nonexistent/muni-parakeet-sidecar-should-not-exist",
        ));
        let start = std::time::Instant::now();
        let err = client
            .transcribe(&[0i16; 16])
            .await
            .expect_err("dead sidecar must not transcribe");
        let elapsed = start.elapsed();
        // The whole point of task 18: no inline model reload on the press path.
        assert!(
            elapsed < Duration::from_millis(100),
            "transcribe should fail fast without an inline reload, took {elapsed:?}"
        );
        assert!(
            matches!(err, MuniError::ParakeetSidecarUnavailable { .. }),
            "expected ParakeetSidecarUnavailable, got {err:?}"
        );
        assert_eq!(
            client.respawn_kicks(),
            1,
            "a background respawn must be kicked exactly once"
        );
    }

    #[test]
    fn encode_pcm_payload_is_little_endian() {
        // 0x0102 → [0x02, 0x01]; -1 (0xFFFF) → [0xFF, 0xFF].
        let payload = encode_pcm_payload(&[0x0102, -1, 0]);
        assert_eq!(payload, vec![0x02, 0x01, 0xFF, 0xFF, 0x00, 0x00]);
    }

    #[test]
    fn decode_response_splits_infer_ms_and_text() {
        let mut frame = 437u32.to_le_bytes().to_vec();
        frame.extend_from_slice(b"hello world");
        let (text, infer_ms) = decode_response(&frame).expect("ok response");
        assert_eq!(infer_ms, 437);
        assert_eq!(text, "hello world");
    }

    #[test]
    fn decode_response_treats_err_prefix_as_failure() {
        let mut frame = 0u32.to_le_bytes().to_vec();
        frame.extend_from_slice(b"ERR:model exploded");
        let err = decode_response(&frame).expect_err("ERR body is a failure");
        match err {
            MuniError::ParakeetTranscribeFailed { reason } => assert_eq!(reason, "model exploded"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn decode_response_rejects_short_frame() {
        assert!(decode_response(&[0x01, 0x02]).is_err());
    }

    #[tokio::test]
    async fn frame_write_then_read_roundtrips() {
        // Write two frames into an in-memory buffer, read them back in order.
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, b"READY").await.unwrap();
        write_frame(&mut buf, b"second").await.unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(
            read_frame(&mut cursor).await.unwrap().as_deref(),
            Some(&b"READY"[..])
        );
        assert_eq!(
            read_frame(&mut cursor).await.unwrap().as_deref(),
            Some(&b"second"[..])
        );
        // EOF → None.
        assert_eq!(read_frame(&mut cursor).await.unwrap(), None);
    }

    #[tokio::test]
    async fn read_frame_rejects_oversized_prefix() {
        // A prefix above the cap, with no body, must error out immediately
        // (routing into the respawn path) rather than allocate the body.
        let len = MAX_FRAME_BYTES + 1;
        let mut cursor = std::io::Cursor::new(len.to_le_bytes().to_vec());
        let err = read_frame(&mut cursor)
            .await
            .expect_err("oversized prefix must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn is_selected_respects_env(/* serialized via distinct var values */) {
        // Note: reads process env; kept minimal to avoid cross-test races.
        std::env::remove_var(ASR_BACKEND_ENV);
        assert!(!is_selected());
    }
}
