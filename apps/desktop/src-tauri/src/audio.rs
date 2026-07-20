//! Microphone capture and 16 kHz / mono / int16 PCM normalization.
//!
//! Mirrors Swift v1's `AudioCapture` (AVAudioEngine + AVAudioConverter):
//! resampled int16 chunks for Deepgram (Phase 3), RMS amplitude in [0, 1] for
//! the HUD waveform (Phase 7).
//!
//! Architecture:
//! - cpal owns a `!Send` realtime `Stream`, so a dedicated OS thread (named
//!   `muni-audio`) holds it and processes start/stop control messages.
//! - The realtime callback runs on cpal's audio thread. It performs only
//!   bounded, lock-rare work (mono mixdown → linear resample → i16 clamp →
//!   RMS) and ferries `(Vec<i16>, f32)` through a `crossbeam-channel` to a
//!   bridge thread.
//! - The bridge thread (`muni-audio-bridge`) re-publishes amplitude into a
//!   `tokio::sync::watch`, fans chunks out via `tokio::sync::broadcast`, and
//!   emits the `audio://amplitude` Tauri event.
//!
//! When `MUNI_DEBUG_AUDIO=1`, the active capture is tee'd to
//! `<app_local_data_dir>/last_capture.wav` for offline playback verification
//! (see plan §002 Phase 2 acceptance).
//!
//! When `MUNI_DUMP_AUDIO_DIR=/some/path` is set (dev-only spike flag for
//! evaluating local audio-LID candidates against the production capture path),
//! each press is written to its own timestamped WAV at
//! `<dir>/press_<UTC-ISO8601>.wav`. Takes precedence over `MUNI_DEBUG_AUDIO`
//! when both are set, since per-press files are strictly more useful than a
//! single overwrite. The directory is auto-created.

use std::env;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, StreamConfig};
use crossbeam_channel::{unbounded, Receiver as XReceiver, Sender as XSender};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_store::StoreExt;
use tokio::sync::{broadcast, watch};

use crate::error::MuniError;
use crate::settings::{self, SELECTED_MIC_SYSTEM_DEFAULT};

/// Output sample rate. Deepgram Nova-3 expects 16 kHz mono linear16.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Tauri event name carrying per-chunk RMS amplitude in `[0.0, 1.0]`.
pub const EVENT_AMPLITUDE: &str = "audio://amplitude";

/// Env var that enables tee-ing the active capture to a `.wav` file in the
/// app's local data directory.
pub const DEBUG_ENV_VAR: &str = "MUNI_DEBUG_AUDIO";

/// Dev-only spike flag: when set to a directory path, every press dumps its
/// resampled 16 kHz / mono / int16 PCM to `<dir>/press_<UTC-ISO8601>.wav`.
/// Takes precedence over [`DEBUG_ENV_VAR`] when both are set. Used to build
/// per-press WAV corpora for offline audio-LID model evaluation (see
/// backlog 0015). Not surfaced in the UI; not documented in user-facing docs.
pub const DUMP_DIR_ENV_VAR: &str = "MUNI_DUMP_AUDIO_DIR";

/// Broadcast capacity for resampled chunks. Sized to absorb the forwarder's
/// worst-case *fast-abort window* on a wedged half-open socket without dropping
/// audio on the realtime side — the rescue buffer (`forward_and_buffer_until_
/// release`) is the source of truth for the press, so a `RecvError::Lagged`
/// drop there permanently loses mid-press audio.
///
/// Budget: while a wedged socket blocks each send for `deepgram::SEND_TIMEOUT`
/// (1 s), the forwarder keeps receiving but can't drain fast enough, so chunks
/// queue here. It abandons sending after `session::MAX_CONSECUTIVE_SEND_TIMEOUTS`
/// (2) consecutive timeouts, i.e. ~2 s, then drains the backlog with no further
/// blocking. At cpal's ~60 Hz that's ~120 queued chunks; 192 (~3.2 s) clears
/// that window plus the post-release drain and a brief Deepgram reconnect with
/// headroom. Keep this ≥ `MAX_CONSECUTIVE_SEND_TIMEOUTS × SEND_TIMEOUT × 60 Hz`
/// or the slow-sink guarantee ("never punches holes in the local buffer")
/// regresses.
pub(crate) const CHUNK_BROADCAST_CAPACITY: usize = 192;

/// Microphone capture handle.
///
/// Construction starts the audio host + bridge threads but does NOT open the
/// input stream — that happens on the first [`AudioCapture::start`] call so
/// the app can boot before microphone permission has been granted.
pub struct AudioCapture {
    ctl_tx: XSender<Ctl>,
    chunk_tx: broadcast::Sender<Vec<i16>>,
    amp_tx: watch::Sender<f32>,
    /// Held so `start()` can resolve the live
    /// [`settings::KEY_SELECTED_MICROPHONE`] value on every press
    /// without threading the AppHandle through the orchestrator's
    /// driver loop. Reads are cheap (the Tauri Store keeps the file
    /// hot in memory) and reading per-press lets a tray-menu mic
    /// change take effect on the very next press.
    app: AppHandle,
}

#[derive(Debug)]
enum Ctl {
    Start {
        debug_path: Option<PathBuf>,
        /// User-selected device id (cpal device name) or the
        /// [`SELECTED_MIC_SYSTEM_DEFAULT`] sentinel. `None` is treated
        /// the same as the sentinel — open whatever the OS reports as
        /// the current default at the moment of the press.
        device_id: Option<String>,
    },
    Stop,
    Shutdown,
}

/// IPC-shaped descriptor of a single input device returned by
/// [`enumerate_input_devices`]. `id` is the value the tray menu and
/// settings UI persist; `name` is the label shown to the user;
/// `is_default` tags the entry the OS currently considers the system
/// default so the UI can mark it visually.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InputDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// Snapshot the available input devices via cpal's default host. The
/// enumeration is synchronous and uncached — callers should treat the
/// result as a per-call probe, not a long-lived list (the macOS audio
/// system mutates this set on USB plug/unplug).
///
/// Devices whose names can't be read are skipped: cpal returns an
/// `Err(DeviceNameError)` for transient I/O failures and re-enumerating
/// them under a synthetic name would create a stable id we can't
/// resolve later.
pub fn enumerate_input_devices() -> Vec<InputDevice> {
    let host = cpal::default_host();
    let default_name = host
        .default_input_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_default();
    let devices = match host.input_devices() {
        Ok(d) => d,
        Err(err) => {
            log::warn!(target: "audio", "enumerate_input_devices: cpal failed: {err}");
            return Vec::new();
        }
    };
    devices
        .filter_map(|device| match device.name() {
            Ok(name) => {
                let is_default = !default_name.is_empty() && name == default_name;
                Some(InputDevice {
                    id: name.clone(),
                    name,
                    is_default,
                })
            }
            Err(err) => {
                log::debug!(target: "audio", "skipping device with unreadable name: {err}");
                None
            }
        })
        .collect()
}

impl AudioCapture {
    /// Wire the pipeline.
    pub fn new(app: AppHandle) -> Self {
        let (chunk_tx, _) = broadcast::channel(CHUNK_BROADCAST_CAPACITY);
        let (amp_tx, _) = watch::channel(0.0_f32);
        let (sample_tx, sample_rx) = unbounded::<(Vec<i16>, f32)>();
        let (ctl_tx, ctl_rx) = unbounded::<Ctl>();

        spawn_bridge(sample_rx, chunk_tx.clone(), amp_tx.clone(), app.clone());
        std::thread::Builder::new()
            .name("muni-audio".into())
            .spawn(move || run_audio_host(ctl_rx, sample_tx))
            .expect("spawn muni-audio host thread");

        Self {
            ctl_tx,
            chunk_tx,
            amp_tx,
            app,
        }
    }

    /// Begin capturing.
    ///
    /// Reads [`settings::KEY_SELECTED_MICROPHONE`] from the Tauri Store
    /// per-press so a tray-menu mic change takes effect on the next
    /// press without a restart. The [`SELECTED_MIC_SYSTEM_DEFAULT`]
    /// sentinel (or a missing entry) opens whatever the OS reports as
    /// the current default; any other value is matched against cpal's
    /// enumeration by name. A selected device that can't be found
    /// falls back to the system default for that press — see
    /// [`build_stream`] for the resolver — so a stale or unplugged
    /// selection never blocks dictation.
    ///
    /// Synchronously surfaces:
    /// - [`MuniError::MicrophoneDenied`] when macOS reports the user has
    ///   already declined microphone access (cpal otherwise returns a silent
    ///   stream — zero-filled buffers — which we'd misreport as "working").
    /// - [`MuniError::NoInputDevice`] when no default input is configured.
    ///
    /// The two probe errors above are surfaced to the caller (the session
    /// driver routes them through `present_capture_start_error` — plan 039
    /// task 32). Stream-build failures *past* the probe (cpal device errors on
    /// the realtime thread, after this call has already returned `Ok`) remain
    /// log-only: the control thread has no synchronous channel back to this
    /// return value, so those are reported via logs rather than the typed
    /// error path.
    pub fn start(&self, debug_dir: Option<PathBuf>) -> Result<(), MuniError> {
        permission::check_microphone()?;
        if cpal::default_host().default_input_device().is_none() {
            return Err(MuniError::NoInputDevice);
        }
        let debug_path = resolve_dump_dir_from_env()
            .map(|dir| dir.join(per_press_wav_filename(time::OffsetDateTime::now_utc())))
            .or_else(|| {
                debug_dir.and_then(|dir| {
                    if env::var(DEBUG_ENV_VAR).map(|v| v == "1").unwrap_or(false) {
                        Some(dir.join("last_capture.wav"))
                    } else {
                        None
                    }
                })
            });
        let device_id = self.resolve_selected_device_id();
        self.ctl_tx
            .send(Ctl::Start {
                debug_path,
                device_id,
            })
            .map_err(|_| MuniError::AudioStreamFailed {
                reason: "audio host thread terminated".into(),
            })?;
        Ok(())
    }

    /// Pull the current `KEY_SELECTED_MICROPHONE` value from the Tauri
    /// Store. Any failure (store closed, key absent, non-string value)
    /// collapses to `None`, which `build_stream` interprets as "open
    /// the system default" — the safest fallback when the persisted
    /// selection isn't readable.
    fn resolve_selected_device_id(&self) -> Option<String> {
        let store = self.app.store(settings::SETTINGS_FILE).ok()?;
        let value = store.get(settings::KEY_SELECTED_MICROPHONE)?;
        let id = value.as_str()?.to_string();
        if id.is_empty() || id == SELECTED_MIC_SYSTEM_DEFAULT {
            None
        } else {
            Some(id)
        }
    }

    /// Stop the active stream. Idempotent; cheap (just sends a control msg).
    pub fn stop(&self) {
        let _ = self.ctl_tx.send(Ctl::Stop);
    }

    /// Subscribe to per-chunk 16 kHz mono int16 PCM. First consumer is the
    /// Deepgram client added in Phase 3.
    #[allow(dead_code)]
    pub fn subscribe_chunks(&self) -> broadcast::Receiver<Vec<i16>> {
        self.chunk_tx.subscribe()
    }

    /// Subscribe to per-chunk RMS amplitude in `[0, 1]`. First consumer is
    /// the HUD waveform added in Phase 7; the React debug panel listens via
    /// the `audio://amplitude` Tauri event during Phase 2 verification.
    #[allow(dead_code)]
    pub fn subscribe_amplitude(&self) -> watch::Receiver<f32> {
        self.amp_tx.subscribe()
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        let _ = self.ctl_tx.send(Ctl::Shutdown);
    }
}

// -- audio host thread ------------------------------------------------------

/// Per-stream stats updated by the cpal realtime callback and read by
/// the audio host thread at Stop time. Atomics so the callback never
/// has to take a lock on the hot path.
///
/// A **fresh** instance is allocated on every `Ctl::Start` (plan 039 task 15).
/// Previously a single instance was reset per press; making it per-press means
/// the detached post-Stop zombie-verification thread and the next press's
/// stream can never share a counter, so a `Ctl::Start` landing inside the
/// settle window cannot race the verification's re-snapshot.
#[derive(Default)]
struct StreamStats {
    chunks: AtomicUsize,
    samples: AtomicUsize,
    peak_abs: AtomicI32,
}

impl StreamStats {
    fn record(&self, chunk: &[i16]) {
        self.chunks.fetch_add(1, Ordering::Relaxed);
        self.samples.fetch_add(chunk.len(), Ordering::Relaxed);
        let local_peak: i16 = chunk.iter().map(|s| s.saturating_abs()).max().unwrap_or(0);
        let cur = self.peak_abs.load(Ordering::Relaxed);
        if (local_peak as i32) > cur {
            self.peak_abs.store(local_peak as i32, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> (usize, usize, i16) {
        (
            self.chunks.load(Ordering::Relaxed),
            self.samples.load(Ordering::Relaxed),
            self.peak_abs.load(Ordering::Relaxed) as i16,
        )
    }
}

fn run_audio_host(ctl_rx: XReceiver<Ctl>, sample_tx: XSender<(Vec<i16>, f32)>) {
    // The active stream lives only between Start/Stop. The WAV writer mirrors
    // that lifecycle. We hold the writer behind Arc<Mutex<>> because the cpal
    // callback also writes into it; the lock is uncontended in practice (only
    // touched by the realtime thread plus the host thread on Stop).
    let mut active: Option<cpal::Stream> = None;
    let wav: Arc<Mutex<Option<hound::WavWriter<BufWriter<File>>>>> = Arc::new(Mutex::new(None));
    // Per-press stats (plan 039 task 15): a fresh `StreamStats` is allocated on
    // each `Ctl::Start` and moved into the detached verify thread on `Ctl::Stop`,
    // so back-to-back presses never share a zombie-detection counter.
    let mut current_stats: Option<Arc<StreamStats>> = None;
    let mut started_at: Option<Instant> = None;

    while let Ok(ctl) = ctl_rx.recv() {
        match ctl {
            Ctl::Start {
                debug_path,
                device_id,
            } => {
                if active.is_some() {
                    log::debug!(target: "audio", "Start ignored — capture already active");
                    continue;
                }
                let stats: Arc<StreamStats> = Arc::new(StreamStats::default());
                started_at = Some(Instant::now());
                if let Some(path) = debug_path.as_deref() {
                    match open_wav(path) {
                        Ok(writer) => {
                            *wav.lock().expect("audio wav mutex poisoned") = Some(writer);
                            log::info!(target: "audio", "WAV tee enabled at {}", path.display());
                        }
                        Err(err) => {
                            log::warn!(
                                target: "audio",
                                "WAV tee disabled — could not open {}: {err}",
                                path.display()
                            );
                        }
                    }
                }
                match build_stream(
                    sample_tx.clone(),
                    wav.clone(),
                    stats.clone(),
                    device_id.as_deref(),
                ) {
                    Ok(stream) => {
                        active = Some(stream);
                        current_stats = Some(stats);
                    }
                    Err(err) => {
                        log::error!(
                            target: "audio",
                            "Failed to start audio capture: {} ({:?})",
                            err.user_message(),
                            err.severity()
                        );
                        finalize_wav(&wav);
                    }
                }
            }
            Ctl::Stop => {
                // Teardown + settle-window offload lives in `handle_stop` so the
                // "next `Ctl::Start` isn't blocked by the 200 ms verify wait"
                // invariant is unit-testable without a live audio device (plan
                // 039 task 15). The verify join handle is intentionally dropped
                // here — only tests observe it.
                let _ = handle_stop(active.take(), &wav, current_stats.take(), started_at.take());
            }
            Ctl::Shutdown => {
                drop(active.take());
                finalize_wav(&wav);
                break;
            }
        }
    }
}

/// Post-Stop settle window before the zombie-callback re-snapshot. Comfortably
/// longer than a single cpal callback period (~10 ms at typical buffer sizes),
/// so a callback that survived pause+drop pushes the counter past the baseline.
const STOP_SETTLE_WINDOW: std::time::Duration = std::time::Duration::from_millis(200);

/// Tear down the active stream on `Ctl::Stop` and hand the settle-window
/// zombie check to a detached thread, returning its join handle (or `None` when
/// no press was active).
///
/// Some macOS CoreAudio devices (observed: non-default inputs opened by name)
/// do NOT halt their audio unit when cpal's `Stream::Drop` runs — the realtime
/// callback keeps firing for minutes, flooding the broadcast with phantom
/// chunks and wedging the downstream orchestrator. An explicit `pause()` before
/// drop forces `AudioOutputUnitStop` to run on the host thread and reliably
/// halts the callback regardless of device.
///
/// Teardown runs on the control thread: `cpal::Stream` is `!Send` on macOS, so
/// it cannot be handed to another thread — and dropping it here is
/// deterministic (guaranteed even on a later panic) and fast. Only the 200 ms
/// settle-and-verify wait is offloaded (plan 039 task 15), which is the part
/// that actually stalled the next `Ctl::Start`.
///
/// Extracted from the control loop so the "no inline settle wait" timing
/// invariant is unit-testable without a live audio device: the caller passes
/// no active stream but a populated per-press stat, and asserts this returns
/// within the ~10 ms budget while still spawning the detached verify.
fn handle_stop(
    active: Option<cpal::Stream>,
    wav: &Mutex<Option<hound::WavWriter<BufWriter<File>>>>,
    current_stats: Option<Arc<StreamStats>>,
    started_at: Option<Instant>,
) -> Option<std::thread::JoinHandle<bool>> {
    if let Some(stream) = active {
        if let Err(err) = stream.pause() {
            log::warn!(target: "audio", "stream pause failed: {err}");
        }
        drop(stream);
    }
    finalize_wav(wav);
    let stats = current_stats?;
    let (chunks, samples, peak_abs) = stats.snapshot();
    let elapsed = started_at.map(|t| t.elapsed()).unwrap_or_default();
    let peak_norm = (peak_abs as f32) / (i16::MAX as f32);
    log::info!(
        target: "audio",
        "AudioCapture stopped — chunks={chunks} samples={samples} peak={peak_abs} ({peak_norm:.3}) duration={:.2}s",
        elapsed.as_secs_f64()
    );
    if chunks == 0 {
        log::warn!(
            target: "audio",
            "AudioCapture produced ZERO chunks — selected device did not deliver any audio"
        );
    } else if peak_abs == 0 {
        log::warn!(
            target: "audio",
            "AudioCapture captured {chunks} chunks but ALL samples were silent (peak=0) — device is open but producing no signal"
        );
    }
    // Offload the zombie-callback settle window to a detached thread so the
    // next press's `Ctl::Start` proceeds immediately instead of blocking ~200 ms.
    Some(spawn_stop_verification(stats, chunks, samples))
}

/// Spawn the zombie-callback verification on a detached thread so the
/// `muni-audio` control thread can process the next `Ctl::Start` immediately
/// instead of blocking on [`STOP_SETTLE_WINDOW`] (plan 039 task 15).
///
/// `stats` is the stopped press's OWN per-press counter (see [`StreamStats`]),
/// so the next press's fresh counter can never race this re-snapshot. Returns
/// the join handle carrying `true` when a zombie callback was observed — used
/// by tests; the control thread drops it.
fn spawn_stop_verification(
    stats: Arc<StreamStats>,
    baseline_chunks: usize,
    baseline_samples: usize,
) -> std::thread::JoinHandle<bool> {
    std::thread::Builder::new()
        .name("muni-audio-verify".into())
        .spawn(move || {
            std::thread::sleep(STOP_SETTLE_WINDOW);
            let (chunks_after, samples_after, _) = stats.snapshot();
            let zombie = chunks_after != baseline_chunks || samples_after != baseline_samples;
            if zombie {
                log::warn!(
                    target: "audio",
                    "ZOMBIE STREAM detected — after pause+drop the callback fired again: chunks {baseline_chunks}→{chunks_after} samples {baseline_samples}→{samples_after}"
                );
            }
            zombie
        })
        .expect("spawn muni-audio-verify thread")
}

fn finalize_wav(wav: &Mutex<Option<hound::WavWriter<BufWriter<File>>>>) {
    let writer = match wav.lock() {
        Ok(mut guard) => guard.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    };
    if let Some(writer) = writer {
        if let Err(err) = writer.finalize() {
            log::warn!(target: "audio", "WAV finalize failed: {err}");
        }
    }
}

/// Read [`DUMP_DIR_ENV_VAR`] and return it as a `PathBuf`. An unset or
/// empty value collapses to `None` — callers fall back to the older
/// `MUNI_DEBUG_AUDIO` single-file tee, then to no WAV at all.
fn resolve_dump_dir_from_env() -> Option<PathBuf> {
    let raw = env::var(DUMP_DIR_ENV_VAR).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// Per-press WAV filename, e.g. `press_2026-05-17T15-30-45-123Z.wav`.
/// Colons replaced with dashes so the file is portable across tooling that
/// dislikes them (some shells, Finder column displays, archive utilities).
fn per_press_wav_filename(now: time::OffsetDateTime) -> String {
    // We don't propagate format-failure: time's well-formed UTC stamp can't
    // miss this layout, and even if it did, an empty stem would still produce
    // `press_.wav` — visible, unique-ish via mtime, and not a crash.
    let stamp = now
        .format(&time::format_description::well_known::Iso8601::DEFAULT)
        .unwrap_or_default()
        .replace(':', "-");
    format!("press_{stamp}.wav")
}

fn open_wav(path: &std::path::Path) -> Result<hound::WavWriter<BufWriter<File>>, hound::Error> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    hound::WavWriter::create(path, spec)
}

// -- stream construction (per sample format) --------------------------------

type WavRef = Arc<Mutex<Option<hound::WavWriter<BufWriter<File>>>>>;

/// Resolve `device_id` against cpal's input devices.
///
/// `None`, an empty string, or the [`SELECTED_MIC_SYSTEM_DEFAULT`]
/// sentinel return the host's current default input. Any other value
/// is matched by device name; an unmatched name logs a warning and
/// falls back to the default so a stale or unplugged selection never
/// blocks a press. Returning a `MuniError::NoInputDevice` is reserved
/// for the case where the host has no input at all — that is a real
/// "your machine has no microphone" failure the caller should surface.
fn resolve_input_device(host: &cpal::Host, device_id: Option<&str>) -> Result<Device, MuniError> {
    let wants_default = matches!(
        device_id,
        None | Some("") | Some(SELECTED_MIC_SYSTEM_DEFAULT)
    );
    if !wants_default {
        let target = device_id.unwrap();
        match host.input_devices() {
            Ok(iter) => {
                for device in iter {
                    if device.name().map(|n| n == target).unwrap_or(false) {
                        return Ok(device);
                    }
                }
                log::warn!(
                    target: "audio",
                    "selected microphone {target:?} not found — falling back to system default"
                );
            }
            Err(err) => {
                log::warn!(
                    target: "audio",
                    "input_devices() failed: {err}; falling back to system default"
                );
            }
        }
    }
    host.default_input_device().ok_or(MuniError::NoInputDevice)
}

fn build_stream(
    sample_tx: XSender<(Vec<i16>, f32)>,
    wav: WavRef,
    stats: Arc<StreamStats>,
    device_id: Option<&str>,
) -> Result<cpal::Stream, MuniError> {
    let host = cpal::default_host();
    let device = resolve_input_device(&host, device_id)?;
    let resolved_name = device.name().unwrap_or_else(|_| "<unnamed>".into());
    let supported = device
        .default_input_config()
        .map_err(|e| MuniError::AudioStreamFailed {
            reason: e.to_string(),
        })?;
    let sample_format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let in_rate = config.sample_rate.0;
    let channels = config.channels as usize;

    if channels == 0 {
        return Err(MuniError::AudioStreamFailed {
            reason: "device reports 0 channels".into(),
        });
    }

    log::info!(
        target: "audio",
        "AudioCapture starting; device={resolved_name:?} format={sample_format:?} rate={in_rate}Hz channels={channels}"
    );

    let err_fn = |err| log::error!(target: "audio", "cpal stream error: {err}");

    let stream = match sample_format {
        SampleFormat::F32 => {
            let mut state = CallbackState::new(in_rate, TARGET_SAMPLE_RATE);
            let sample_tx = sample_tx.clone();
            let wav = wav.clone();
            let stats = stats.clone();
            device
                .build_input_stream::<f32, _, _>(
                    &config,
                    move |data, _| {
                        state.process(data, channels, |s| *s, &sample_tx, &wav, &stats);
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| MuniError::AudioStreamFailed {
                    reason: e.to_string(),
                })?
        }
        SampleFormat::I16 => {
            let mut state = CallbackState::new(in_rate, TARGET_SAMPLE_RATE);
            let sample_tx = sample_tx.clone();
            let wav = wav.clone();
            let stats = stats.clone();
            device
                .build_input_stream::<i16, _, _>(
                    &config,
                    move |data, _| {
                        state.process(
                            data,
                            channels,
                            |s| (*s as f32) / (i16::MAX as f32),
                            &sample_tx,
                            &wav,
                            &stats,
                        );
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| MuniError::AudioStreamFailed {
                    reason: e.to_string(),
                })?
        }
        SampleFormat::U16 => {
            let mut state = CallbackState::new(in_rate, TARGET_SAMPLE_RATE);
            let sample_tx = sample_tx.clone();
            let wav = wav.clone();
            let stats = stats.clone();
            device
                .build_input_stream::<u16, _, _>(
                    &config,
                    move |data, _| {
                        state.process(
                            data,
                            channels,
                            |s| ((*s as f32) - 32768.0) / 32768.0,
                            &sample_tx,
                            &wav,
                            &stats,
                        );
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| MuniError::AudioStreamFailed {
                    reason: e.to_string(),
                })?
        }
        other => {
            return Err(MuniError::AudioStreamFailed {
                reason: format!("unsupported sample format: {other:?}"),
            });
        }
    };

    stream.play().map_err(|e| MuniError::AudioStreamFailed {
        reason: e.to_string(),
    })?;
    Ok(stream)
}

// -- realtime callback state -----------------------------------------------

/// Per-stream mutable state owned by the cpal callback closure. Kept in one
/// struct so the format-specific callback bodies stay one-liners.
struct CallbackState {
    resampler: LinearResampler,
    mono: Vec<f32>,
}

impl CallbackState {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            resampler: LinearResampler::new(in_rate, out_rate),
            mono: Vec::with_capacity(4096),
        }
    }

    fn process<S, F>(
        &mut self,
        data: &[S],
        channels: usize,
        convert: F,
        sample_tx: &XSender<(Vec<i16>, f32)>,
        wav: &WavRef,
        stats: &StreamStats,
    ) where
        F: Fn(&S) -> f32,
    {
        mono_mixdown(data, channels, &convert, &mut self.mono);
        let mut out: Vec<i16> = Vec::with_capacity(self.mono.len());
        self.resampler.process(&self.mono, &mut out);
        if out.is_empty() {
            return;
        }
        let rms = compute_rms_i16(&out);
        stats.record(&out);
        write_wav_chunk(&out, wav);
        // unbounded crossbeam send never blocks; if the bridge has died we
        // silently drop the chunk (logged once on bridge exit, see spawn_bridge).
        let _ = sample_tx.try_send((out, rms));
    }
}

fn mono_mixdown<S, F>(data: &[S], channels: usize, convert: &F, out: &mut Vec<f32>)
where
    F: Fn(&S) -> f32,
{
    out.clear();
    if channels <= 1 {
        out.reserve(data.len());
        for s in data {
            out.push(convert(s));
        }
        return;
    }
    let frames = data.len() / channels;
    out.reserve(frames);
    let scale = 1.0 / channels as f32;
    for f in 0..frames {
        let mut acc = 0.0_f32;
        let base = f * channels;
        for c in 0..channels {
            acc += convert(&data[base + c]);
        }
        out.push(acc * scale);
    }
}

fn write_wav_chunk(samples: &[i16], wav: &WavRef) {
    let mut guard = match wav.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let Some(writer) = guard.as_mut() else {
        return;
    };
    for &s in samples {
        if let Err(err) = writer.write_sample(s) {
            log::warn!(target: "audio", "WAV write failed: {err} — disabling tee");
            *guard = None;
            break;
        }
    }
}

// -- microphone permission probe -------------------------------------------

/// Pre-flight check that surfaces a typed [`MuniError::MicrophoneDenied`]
/// instead of letting cpal silently hand back a zero-filled stream.
///
/// macOS gates microphone access through TCC (`kTCCServiceMicrophone`). When
/// the user has denied access, cpal still builds a working stream but the
/// callback only ever sees zeros — UI shows "STREAMING / 0 amplitude" with
/// no error. Asking AVCaptureDevice for the current authorization status
/// before opening lets us bail loudly.
mod permission {
    use crate::error::MuniError;

    #[cfg(target_os = "macos")]
    pub(super) fn check_microphone() -> Result<(), MuniError> {
        use objc2::{class, msg_send};
        use objc2_foundation::NSString;

        // AVAuthorizationStatus values from AVCaptureDevice.h.
        const STATUS_RESTRICTED: isize = 1;
        const STATUS_DENIED: isize = 2;

        // AVMediaTypeAudio is an `NSString * const` exported by AVFoundation.
        // It's a non-null, process-lifetime singleton, so a `&'static NSString`
        // is a sound representation in Rust.
        #[link(name = "AVFoundation", kind = "framework")]
        unsafe extern "C" {
            static AVMediaTypeAudio: &'static NSString;
        }

        // SAFETY: `AVCaptureDevice` is a class shipped with AVFoundation,
        // which is linked above. `+authorizationStatusForMediaType:` is a
        // documented class method that takes an AVMediaType (NSString) and
        // returns an AVAuthorizationStatus (NSInteger). The call is
        // synchronous and side-effect-free, so it's safe to issue from any
        // thread.
        let status: isize = unsafe {
            let cls = class!(AVCaptureDevice);
            msg_send![cls, authorizationStatusForMediaType: AVMediaTypeAudio]
        };

        match status {
            STATUS_DENIED | STATUS_RESTRICTED => {
                log::error!(
                    target: "audio",
                    "Microphone authorization denied (AVAuthorizationStatus={status})"
                );
                Err(MuniError::MicrophoneDenied)
            }
            // `notDetermined` (0): cpal will trigger the system prompt on
            // first stream open. `authorized` (3): proceed.
            _ => Ok(()),
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub(super) fn check_microphone() -> Result<(), MuniError> {
        Ok(())
    }
}

// -- bridge thread ----------------------------------------------------------

fn spawn_bridge(
    sample_rx: XReceiver<(Vec<i16>, f32)>,
    chunk_tx: broadcast::Sender<Vec<i16>>,
    amp_tx: watch::Sender<f32>,
    app: AppHandle,
) {
    std::thread::Builder::new()
        .name("muni-audio-bridge".into())
        .spawn(move || {
            while let Ok((chunk, rms)) = sample_rx.recv() {
                // Amplitude: watch always succeeds (no receivers is fine).
                let _ = amp_tx.send(rms);
                if let Err(err) = app.emit(EVENT_AMPLITUDE, rms) {
                    log::warn!(target: "audio", "emit {EVENT_AMPLITUDE} failed: {err}");
                }
                // Chunks: broadcast send returns Err when no subscribers; that
                // is the steady state for Phase 2 (Deepgram lands in Phase 3).
                let _ = chunk_tx.send(chunk);
            }
            log::info!(target: "audio", "audio bridge thread exiting");
        })
        .expect("spawn muni-audio-bridge thread");
}

// -- Linear resampler -------------------------------------------------------

/// Single-channel linear-interpolation resampler with cross-chunk continuity.
///
/// State is one `prev_last` sample (the trailing sample of the previous chunk)
/// plus the next-output sample's position in the running input-sample
/// coordinate (`next_t`). The Swift v1 path used `AVAudioConverter` which
/// applies an internal anti-alias filter; for Phase 2 we deliberately accept
/// linear interp's quality (it's adequate for ASR at 48 kHz → 16 kHz, the
/// common case) and revisit if Deepgram quality regresses.
pub(crate) struct LinearResampler {
    /// Input samples per output sample (`in_rate / out_rate`).
    step: f64,
    /// Position of the next output sample, in absolute input-sample units.
    next_t: f64,
    /// Total input samples consumed across all chunks.
    consumed: u64,
    /// The last sample of the previous chunk, used when the next output's
    /// floor index lands exactly on the chunk boundary.
    prev_last: Option<f32>,
}

impl LinearResampler {
    pub(crate) fn new(in_rate: u32, out_rate: u32) -> Self {
        assert!(in_rate > 0 && out_rate > 0, "sample rates must be > 0");
        Self {
            step: in_rate as f64 / out_rate as f64,
            next_t: 0.0,
            consumed: 0,
            prev_last: None,
        }
    }

    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<i16>) {
        if input.is_empty() {
            return;
        }
        let chunk_start = self.consumed as f64;
        let chunk_end = chunk_start + input.len() as f64;

        // For each output sample at absolute time `t`, we need input[floor(t)]
        // and input[floor(t)+1]. floor(t) may be `chunk_start - 1` (carry from
        // prev_last); floor(t)+1 must lie strictly inside the current chunk.
        loop {
            let t = self.next_t;
            if t + 1.0 >= chunk_end {
                break;
            }
            let floor_t = t.floor();
            // Guard against next_t falling so far behind we'd need data older
            // than prev_last. In steady state this never triggers (next_t
            // always advances by `step >= 1` for downsampling, and stops
            // exactly one position before crossing the chunk boundary).
            if floor_t < chunk_start - 1.0 {
                self.next_t = chunk_start - 1.0;
                continue;
            }
            let frac = (t - floor_t) as f32;
            let i_floor = floor_t as i64;
            let local = i_floor - chunk_start as i64;
            let s0 = if local >= 0 {
                input[local as usize]
            } else {
                self.prev_last.unwrap_or(0.0)
            };
            let s1 = input[(local + 1) as usize];
            let interp = s0 + (s1 - s0) * frac;
            out.push(clamp_to_i16(interp));
            self.next_t += self.step;
        }

        self.prev_last = Some(input[input.len() - 1]);
        self.consumed += input.len() as u64;
    }
}

#[inline]
fn clamp_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

#[inline]
fn compute_rms_i16(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let scale = 1.0_f64 / i16::MAX as f64;
    let mut sum_sq = 0.0_f64;
    for &s in samples {
        let f = s as f64 * scale;
        sum_sq += f * f;
    }
    (sum_sq / samples.len() as f64).sqrt() as f32
}

// -- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_verification_does_not_block_the_caller() {
        // Plan 039 task 15: spawning the settle-and-verify must return to the
        // control thread far faster than the settle window, so the next
        // `Ctl::Start` can open immediately (~10 ms budget). The 200 ms sleep
        // lives on the detached thread.
        let stats = Arc::new(StreamStats::default());
        for _ in 0..5 {
            stats.record(&[0i16; 160]);
        }
        let (chunks, samples, _) = stats.snapshot();

        let started = Instant::now();
        let handle = spawn_stop_verification(stats, chunks, samples);
        let spawn_cost = started.elapsed();
        assert!(
            spawn_cost < std::time::Duration::from_millis(10),
            "spawn_stop_verification must not block the control thread, took {spawn_cost:?}"
        );

        // No callbacks fired after baseline → not a zombie.
        assert!(!handle.join().expect("verify thread joins"));
    }

    #[test]
    fn handle_stop_offloads_the_settle_window_and_returns_immediately() {
        // Plan 039 task 15 — the acceptance criterion is "Stop→Start audio cycle
        // < ~10 ms". A real cpal stream needs an audio device (absent in CI), so
        // this drives `handle_stop` — the whole `Ctl::Stop` body — with no active
        // stream but a populated per-press stat. The invariant it guards is
        // device-independent: `handle_stop` must NOT block on the 200 ms settle
        // window inline (it hands it to a detached thread), so a regression that
        // reintroduces a blocking wait inside the stop arm before the spawn would
        // blow the ~10 ms budget below and fail here — a gap the isolated
        // `spawn_stop_verification` tests could not catch.
        let wav: Mutex<Option<hound::WavWriter<BufWriter<File>>>> = Mutex::new(None);
        let stats = Arc::new(StreamStats::default());
        for _ in 0..5 {
            stats.record(&[0i16; 160]);
        }

        let started = Instant::now();
        let handle = handle_stop(None, &wav, Some(stats), Some(Instant::now()))
            .expect("a press was active, so verification must be spawned");
        let stop_cost = started.elapsed();
        assert!(
            stop_cost < std::time::Duration::from_millis(10),
            "handle_stop must return within the ~10 ms Stop→Start budget (settle \
             window offloaded to a detached thread), took {stop_cost:?}"
        );

        // The settle window ran on the detached thread, not inline; with no
        // post-baseline callback it reports no zombie.
        assert!(
            !handle.join().expect("verify thread joins"),
            "no callback fired after baseline → not a zombie"
        );
    }

    #[test]
    fn handle_stop_without_active_press_spawns_no_verification() {
        // A stray `Ctl::Stop` with no active press (already stopped) must be a
        // clean no-op: no per-press stat → no detached verify thread.
        let wav: Mutex<Option<hound::WavWriter<BufWriter<File>>>> = Mutex::new(None);
        assert!(
            handle_stop(None, &wav, None, None).is_none(),
            "no active press → no verification thread spawned"
        );
    }

    #[test]
    fn stop_verification_flags_a_post_baseline_callback_as_zombie() {
        // A callback that fires AFTER pause+drop (i.e. after the baseline was
        // captured) advances the per-press counter and must be reported.
        let stats = Arc::new(StreamStats::default());
        stats.record(&[0i16; 160]);
        let (chunks, samples, _) = stats.snapshot();

        let handle = spawn_stop_verification(stats.clone(), chunks, samples);
        // Simulate a zombie callback landing during the settle window.
        stats.record(&[0i16; 160]);

        assert!(
            handle.join().expect("verify thread joins"),
            "a post-baseline callback must be detected as a zombie"
        );
    }

    #[test]
    fn clamp_to_i16_handles_full_range() {
        assert_eq!(clamp_to_i16(0.0), 0);
        assert_eq!(clamp_to_i16(1.0), i16::MAX);
        assert_eq!(clamp_to_i16(-1.0), -i16::MAX); // == -32767
        assert_eq!(clamp_to_i16(2.5), i16::MAX);
        assert_eq!(clamp_to_i16(-2.5), -i16::MAX);
    }

    #[test]
    fn compute_rms_i16_is_zero_for_silence_and_one_for_full_scale() {
        let silence = vec![0_i16; 64];
        assert_eq!(compute_rms_i16(&silence), 0.0);

        let full_scale = vec![i16::MAX; 64];
        let rms = compute_rms_i16(&full_scale);
        assert!((rms - 1.0).abs() < 1e-4, "expected ~1.0 RMS, got {rms}");

        let empty: Vec<i16> = vec![];
        assert_eq!(compute_rms_i16(&empty), 0.0);
    }

    #[test]
    fn mono_mixdown_passes_through_when_already_mono() {
        let stereo: Vec<f32> = vec![0.25];
        let mut out = Vec::new();
        mono_mixdown(&stereo, 1, &|s: &f32| *s, &mut out);
        assert_eq!(out, vec![0.25]);
    }

    #[test]
    fn mono_mixdown_averages_multi_channel_frames() {
        // Two frames, stereo: [L0, R0, L1, R1] → [(L0+R0)/2, (L1+R1)/2]
        let stereo: Vec<f32> = vec![0.4, 0.6, -0.2, 0.2];
        let mut out = Vec::new();
        mono_mixdown(&stereo, 2, &|s: &f32| *s, &mut out);
        assert!((out[0] - 0.5).abs() < 1e-6);
        assert!((out[1] - 0.0).abs() < 1e-6);
    }

    #[test]
    fn linear_resampler_identity_preserves_samples_across_chunks() {
        let mut r = LinearResampler::new(16_000, 16_000);
        let chunk1: Vec<f32> = (0..10).map(|i| i as f32 / 100.0).collect();
        let chunk2: Vec<f32> = (10..20).map(|i| i as f32 / 100.0).collect();
        let mut out = Vec::new();
        r.process(&chunk1, &mut out);
        r.process(&chunk2, &mut out);

        // Identity step==1 emits one fewer sample than total input (the very
        // last sample is held back because s1 isn't available yet).
        assert_eq!(out.len(), 19);
        for (i, &produced) in out.iter().enumerate() {
            let expected = clamp_to_i16(i as f32 / 100.0);
            assert_eq!(
                produced, expected,
                "mismatch at i={i}: got {produced}, expected {expected}"
            );
        }
    }

    #[test]
    fn linear_resampler_2_to_1_downsample_picks_alternate_samples() {
        let mut r = LinearResampler::new(2, 1);
        let input: Vec<f32> = (0..10).map(|i| i as f32 / 100.0).collect();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        // step=2, t=0,2,4,6,8 → 5 outputs; 8+1 < 10 holds.
        assert_eq!(out.len(), 5);
        for (k, &produced) in out.iter().enumerate() {
            let expected = clamp_to_i16(k as f32 * 2.0 / 100.0);
            assert_eq!(produced, expected, "k={k}");
        }
    }

    #[test]
    fn linear_resampler_chunk_split_matches_single_chunk_output() {
        // Splitting input across chunk boundaries must not change the output.
        let single: Vec<f32> = (0..50).map(|i| (i as f32 * 0.05).sin()).collect();
        let mut r1 = LinearResampler::new(48_000, 16_000);
        let mut out1 = Vec::new();
        r1.process(&single, &mut out1);

        let mut r2 = LinearResampler::new(48_000, 16_000);
        let mut out2 = Vec::new();
        r2.process(&single[..7], &mut out2);
        r2.process(&single[7..23], &mut out2);
        r2.process(&single[23..], &mut out2);

        assert_eq!(out1, out2, "chunk-split output diverged from single-chunk");
    }

    #[test]
    fn per_press_wav_filename_shape_is_portable_and_unique() {
        let t1 = time::macros::datetime!(2026-05-17 15:30:45.123 UTC);
        let t2 = time::macros::datetime!(2026-05-17 15:30:45.124 UTC);
        let name1 = per_press_wav_filename(t1);
        let name2 = per_press_wav_filename(t2);

        assert!(name1.starts_with("press_"), "got {name1}");
        assert!(name1.ends_with(".wav"), "got {name1}");
        assert!(
            !name1.contains(':'),
            "filename must not contain ':' for portability — got {name1}"
        );
        assert_ne!(
            name1, name2,
            "millisecond-resolution timestamps should distinguish back-to-back presses"
        );
    }

    #[test]
    fn linear_resampler_handles_empty_chunks() {
        let mut r = LinearResampler::new(48_000, 16_000);
        let mut out = Vec::new();
        r.process(&[], &mut out);
        assert!(out.is_empty());
    }
}
