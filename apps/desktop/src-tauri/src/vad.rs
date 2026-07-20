//! Content-aware Voice Activity Detection gate (feature 023 / backlog 0040).
//!
//! Wraps the [`voice_activity_detector`] crate (Silero VAD v5, ONNX model
//! bundled inside the crate) behind a thin async trait so call sites in
//! [`crate::session`] can short-circuit silent presses BEFORE they pay the
//! cost of a Whisper batch transcribe (Site B — release path) or a hybrid
//! slice transcribe + classify (Site A — `spawn_audio_hybrid_inner_classify`).
//!
//! ## Why a trait
//!
//! Same rationale as [`crate::audio_lid::AudioLidClassifier`] and
//! [`crate::text_lid::TextLidClassifier`]: gate-composition tests need a
//! deterministic [`MockVad`] without standing up the real ONNX runtime,
//! and the streaming variant flagged in backlog 0042 will plug a second
//! impl into the same surface.
//!
//! ## Fail-open contract
//!
//! Per brainstorm 005 § Decision 8b, both the load path AND the per-call
//! inference path MUST degrade to "speech detected = true" on any error.
//! The optimization layer must never silently gate real dictation. The
//! `MUNI_VAD_REQUIRED=1` escape hatch in [`crate::lib::build_vad_detector`]
//! promotes load failures to a boot-time panic for CI / dev assertions —
//! but inference failures stay quiet and disable the detector for the rest
//! of the session to avoid log floods.
//!
//! ## Bundled model
//!
//! The crate embeds the Silero v5 ONNX file as a `include_bytes!` resource
//! (~2 MB). No separate file under `apps/desktop/src-tauri/resources/` is
//! required.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use voice_activity_detector::VoiceActivityDetector;

use crate::error::MuniError;

/// Silero-published default speech probability threshold. Brainstorm 005
/// § Decision 6 picked this as the calibration starting point; overridable
/// per-session via `MUNI_VAD_THRESHOLD`.
pub const VAD_DEFAULT_THRESHOLD: f32 = 0.5;

/// Silero-published minimum continuous speech duration. Brainstorm 005
/// § Decision 6 ("P2" policy — N consecutive frames above threshold);
/// overridable via `MUNI_VAD_MIN_SPEECH_MS`.
pub const VAD_DEFAULT_MIN_SPEECH_MS: u32 = 250;

/// Sample rate the underlying Silero v5 ONNX expects. The crate validates
/// this in its `build()` impl — passing anything else fails.
pub const VAD_SAMPLE_RATE: i64 = 16_000;

/// Window size the Silero v5 ONNX expects at 16 kHz. The crate validates
/// this in its `build()` impl — at 16 kHz, only 512 is allowed.
pub const VAD_CHUNK_SIZE: usize = 512;

/// Predicate trait used by the silent-press gate sites.
///
/// Implementations MUST fail open: on any load- or inference-error,
/// log a warning and return `true` (treat as "speech detected") so the
/// optimization layer never silently gates real dictation.
#[async_trait]
pub trait VadDetector: Send + Sync {
    /// Returns `true` if `samples` contains speech per the configured
    /// `threshold` and `min_speech_ms` policy (P2 from brainstorm 005:
    /// N consecutive frames at-or-above threshold count as speech).
    /// Input is 16 kHz mono int16 PCM — matches the rest of the
    /// muni audio pipeline.
    async fn predict_speech(&self, samples: &[i16]) -> bool;

    /// Short identifier embedded in `[vad]` / `[asr]` / `[lid]` log
    /// lines and usage telemetry. Convention: `"<provider>"`, e.g.
    /// `"silero_v5"`.
    fn provider_label(&self) -> &str;
}

/// Real Silero VAD v5 backend (the production impl).
pub struct SileroVad {
    detector: Arc<Mutex<VoiceActivityDetector>>,
    threshold: f32,
    min_speech_frames: u32,
    /// Set to `true` after the first inference error so subsequent calls
    /// short-circuit fail-open without re-attempting (and without
    /// flooding the log).
    disabled: Arc<AtomicBool>,
}

impl std::fmt::Debug for SileroVad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SileroVad")
            .field("threshold", &self.threshold)
            .field("min_speech_frames", &self.min_speech_frames)
            .field("disabled", &self.disabled.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl SileroVad {
    /// Build a fresh detector. Logs the configuration at INFO so boot
    /// logs carry the effective threshold + min-speech for grep-based
    /// triage.
    pub fn new(threshold: f32, min_speech_ms: u32) -> Result<Self, MuniError> {
        let detector = VoiceActivityDetector::builder()
            .sample_rate(VAD_SAMPLE_RATE)
            .chunk_size(VAD_CHUNK_SIZE)
            .build()
            .map_err(|e| MuniError::VadLoadFailed {
                reason: format!("VoiceActivityDetector::builder().build() failed: {e}"),
            })?;
        // Ceiling-divide so the configured min-speech-ms always crosses
        // at least that many real milliseconds (250 ms / 32 ms per frame
        // = 7.8 → 8 frames, matching the plan's documented expectation).
        let frame_ms = (VAD_CHUNK_SIZE as u32 * 1000) / VAD_SAMPLE_RATE as u32;
        let min_speech_frames = min_speech_ms.div_ceil(frame_ms.max(1));
        log::info!(
            target: "vad",
            "boot: VAD provider=silero_v5 threshold={threshold:.2} min_speech_ms={min_speech_ms} (min_speech_frames={min_speech_frames})"
        );
        Ok(Self {
            detector: Arc::new(Mutex::new(detector)),
            threshold,
            min_speech_frames,
            disabled: Arc::new(AtomicBool::new(false)),
        })
    }
}

#[async_trait]
impl VadDetector for SileroVad {
    fn provider_label(&self) -> &str {
        "silero_v5"
    }

    async fn predict_speech(&self, samples: &[i16]) -> bool {
        if self.disabled.load(Ordering::Acquire) {
            return true;
        }
        if samples.is_empty() {
            // Fail-open: empty buffer would be classified as silence,
            // but the caller is the only one that knows whether that's
            // meaningful. Don't bias gate decisions on the boundary.
            return true;
        }
        let samples_owned = samples.to_vec();
        let detector = self.detector.clone();
        let threshold = self.threshold;
        let min_speech_frames = self.min_speech_frames;
        let disabled = self.disabled.clone();

        // The crate's `predict()` is sync and `unwrap`s on ORT errors
        // (which can panic on a corrupted bundled model). `spawn_blocking`
        // + `catch_unwind` keeps a panic from poisoning the tokio runtime
        // and lets us fail-open per the trait contract.
        let join = tokio::task::spawn_blocking(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_predict(&detector, &samples_owned, threshold, min_speech_frames)
            }))
        });

        match join.await {
            Ok(Ok(Ok(speech_detected))) => speech_detected,
            Ok(Ok(Err(err))) => {
                disabled.store(true, Ordering::Release);
                log::warn!(
                    target: "vad",
                    "VAD inference failed: {err} — disabling gate for rest of session (fail-open)"
                );
                true
            }
            Ok(Err(panic)) => {
                disabled.store(true, Ordering::Release);
                let panic_msg = panic_message(&panic);
                log::warn!(
                    target: "vad",
                    "VAD inference panicked ({panic_msg}) — disabling gate for rest of session (fail-open)"
                );
                true
            }
            Err(join_err) => {
                disabled.store(true, Ordering::Release);
                log::warn!(
                    target: "vad",
                    "VAD spawn_blocking join failed: {join_err} — disabling gate for rest of session (fail-open)"
                );
                true
            }
        }
    }
}

/// Sync hot path. Holds the Mutex for the duration of one prediction
/// pass — the lock is acquired inside `spawn_blocking`, never held
/// across an `await`.
fn run_predict(
    detector: &Mutex<VoiceActivityDetector>,
    samples: &[i16],
    threshold: f32,
    min_speech_frames: u32,
) -> Result<bool, MuniError> {
    let mut guard = detector.lock().map_err(|e| MuniError::VadInferenceFailed {
        reason: format!("VAD mutex poisoned: {e}"),
    })?;
    guard.reset();
    let mut consecutive: u32 = 0;
    for chunk in samples.chunks(VAD_CHUNK_SIZE) {
        let prob = guard.predict(chunk.iter().copied());
        if prob >= threshold {
            consecutive += 1;
            if consecutive >= min_speech_frames {
                return Ok(true);
            }
        } else {
            consecutive = 0;
        }
    }
    Ok(false)
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Test double that returns a canned verdict. Used by gate-composition
/// tests to verify the call sites short-circuit correctly without
/// loading the real ONNX runtime.
#[derive(Debug, Clone)]
pub struct MockVad {
    speech_detected: bool,
}

impl MockVad {
    pub fn new(speech_detected: bool) -> Self {
        Self { speech_detected }
    }
}

#[async_trait]
impl VadDetector for MockVad {
    async fn predict_speech(&self, _samples: &[i16]) -> bool {
        self.speech_detected
    }

    fn provider_label(&self) -> &str {
        "mock"
    }
}

// ============================================================================
// Feature 024 (backlog 0042) — streaming VAD for mid-press silence.
// ============================================================================
//
// Sibling of the batch [`VadDetector`] above. The batch trait gates whole
// buffers at decision points; the streaming sibling gates *frames* at the
// audio-chunk level so mid-press silence stretches can be trimmed before they
// reach Whisper batch (Site E) or accumulate into hybrid slices (Site D).
//
// ## Design discipline
//
// - **Per-stream ownership.** Each press constructs its own
//   [`SileroStreamingVad`] via [`StreamingVadFactory`]; no shared `Mutex`,
//   no cross-stream contention. The trait is `Send`-only (not `Send + Sync`)
//   to encode this.
// - **Fail-open at every layer.** Construction failure → `PassThroughStreamingVad`
//   (lets the factory always return a working detector when at least one
//   kill switch is on). Per-stream inference failure → `disabled = true` on
//   that instance, all subsequent frames pass unconditionally. The
//   optimization layer never silently gates real dictation.
// - **Word-start protection is non-negotiable.** Silence must accumulate to
//   at least `min_silence_frames` *before* suppression engages, AND a
//   `word_guard_frames` window of unconditional pass-through fires after
//   every silence→speech transition. Speed > Intent priority means a
//   false-positive word clip would violate the project's #1 priority.

/// Continuous-silence duration before the streaming detector latches
/// suppression. Brainstorm 006 § Decision 3 picked 500 ms as the
/// conservative starting point; tunable via
/// `MUNI_VAD_STREAM_MIN_SILENCE_MS`.
pub const STREAM_DEFAULT_MIN_SILENCE_MS: u32 = 500;

/// Post-silence-end frames passed unconditionally to protect word starts.
/// Mathematically prevents first-phoneme clipping when speech resumes
/// after a suppressed silence stretch. Tunable via
/// `MUNI_VAD_STREAM_WORD_GUARD_MS`.
pub const STREAM_DEFAULT_WORD_GUARD_MS: u32 = 500;

/// Per-frame classification result. Stored in dogfood logs at debug level
/// to distinguish "passed because VAD said speech" from "passed because
/// post-silence word-guard window was active." The latter is correctness
/// load-bearing — it's the mechanism that prevents word-start clipping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameVerdict {
    /// VAD probability ≥ threshold; emit.
    Speech,
    /// VAD probability < threshold; suppress if past the
    /// `min_silence_frames` accumulator.
    Silence,
    /// Frame emitted regardless of verdict because the post-silence
    /// word-guard window is active. Diagnostic-only; the trait surface
    /// does not branch on this.
    PassedByWordGuard,
}

/// Factory closure returning fresh per-stream detectors. The factory
/// presence (`Some`) means at least one of the three streaming-VAD kill
/// switches ([`crate::lib::MUNI_VAD_STREAM_HYBRID_ENV`],
/// [`crate::lib::MUNI_VAD_TRIM_RELEASE_BUFFER_ENV`], or feat/025's
/// [`crate::lib::MUNI_VAD_AUDIO_LID_GATE_ENV`]) is enabled at boot;
/// each invocation builds a fresh detector with the boot-resolved
/// calibration knobs.
pub type StreamingVadFactory = Arc<dyn Fn() -> Box<dyn StreamingVadDetector> + Send + Sync>;

/// Streaming sibling of [`VadDetector`]. Stateful per instance — each
/// press/stream constructs its own detector so the per-frame inference
/// calls never need a shared Mutex.
///
/// # State machine (per instance lifetime)
///
/// - **Partial-frame buffer**: holds leftover samples from chunks that
///   weren't multiples of [`VAD_CHUNK_SIZE`]. Drained on next call.
/// - **Consecutive-silent-frames counter**: increments each silence frame
///   (`prob < threshold`); reset on speech.
/// - **Post-silence guard frames remaining**: when transitioning silence
///   → speech, set to `word_guard_frames`. While > 0, all frames pass
///   unconditionally regardless of VAD verdict (mathematically prevents
///   first-phoneme clipping).
/// - **Suppressing latch**: `true` once consecutive silence ≥
///   `min_silence_frames`; cleared on first speech frame.
///
/// # Fail-open contract
///
/// On any inference panic or join error, the instance flips a per-instance
/// `disabled` flag. For the rest of this stream's lifetime, the entire
/// input chunk is appended to `output` unchanged. The next press
/// constructs a fresh instance (the disable is per-instance, not
/// process-wide).
#[async_trait]
pub trait StreamingVadDetector: Send {
    /// Process one audio chunk. Frames the detector decides to retain
    /// are appended to `output`. Chunks may be any length; the
    /// implementation tracks partial-frame leftovers across calls.
    async fn process_chunk(&mut self, chunk: &[i16], output: &mut Vec<i16>);

    /// One-shot pass: process an entire buffer and return the
    /// speech-only concatenation. Resets internal state after the call.
    /// Used by Site E's fallback path when the hybrid mirror is empty.
    async fn extract_speech(&mut self, buffer: &[i16]) -> Vec<i16>;

    /// Reset the LSTM hidden state + counters. Called at the start of a
    /// new stream by the construction site; called internally by
    /// [`Self::extract_speech`].
    fn reset(&mut self);

    /// Short identifier embedded in log lines and usage telemetry.
    /// Convention: `"<provider>_stream"`.
    fn provider_label(&self) -> &str;
}

/// Per-instance mutable state for [`SileroStreamingVad`]. Held inside
/// `Option<>` so [`tokio::task::spawn_blocking`] can take ownership for
/// the duration of the sync inference call and return it.
struct StreamingVadState {
    detector: VoiceActivityDetector,
    partial_frame_buffer: Vec<i16>,
    consecutive_silent_frames: u32,
    post_silence_guard_frames_remaining: u32,
    suppressing: bool,
}

/// Production streaming VAD impl wrapping Silero v5 via the
/// `voice_activity_detector` crate. Per-instance state means no shared
/// Mutex; each press owns its own detector.
pub struct SileroStreamingVad {
    state: Option<StreamingVadState>,
    threshold: f32,
    min_silence_frames: u32,
    word_guard_frames: u32,
    /// Once any inference fails on this instance, all subsequent calls
    /// pass-through the input unchanged. Per-instance (not process-wide)
    /// because the next stream constructs a fresh detector.
    disabled: bool,
}

impl std::fmt::Debug for SileroStreamingVad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SileroStreamingVad")
            .field("threshold", &self.threshold)
            .field("min_silence_frames", &self.min_silence_frames)
            .field("word_guard_frames", &self.word_guard_frames)
            .field("disabled", &self.disabled)
            .finish_non_exhaustive()
    }
}

impl SileroStreamingVad {
    /// Build a fresh streaming detector. The first
    /// [`Self::word_guard_frames`] of every stream pass unconditionally
    /// so press-start word clipping is also impossible.
    pub fn new(threshold: f32, min_silence_ms: u32, word_guard_ms: u32) -> Result<Self, MuniError> {
        let detector = VoiceActivityDetector::builder()
            .sample_rate(VAD_SAMPLE_RATE)
            .chunk_size(VAD_CHUNK_SIZE)
            .build()
            .map_err(|e| MuniError::VadLoadFailed {
                reason: format!("VoiceActivityDetector::builder().build() failed: {e}"),
            })?;
        let frame_ms = (VAD_CHUNK_SIZE as u32 * 1000) / VAD_SAMPLE_RATE as u32;
        let min_silence_frames = min_silence_ms.div_ceil(frame_ms.max(1));
        let word_guard_frames = word_guard_ms.div_ceil(frame_ms.max(1));
        log::info!(
            target: "vad",
            "constructed: SileroStreamingVad threshold={threshold:.2} min_silence_ms={min_silence_ms} (min_silence_frames={min_silence_frames}) word_guard_ms={word_guard_ms} (word_guard_frames={word_guard_frames})"
        );
        Ok(Self {
            state: Some(StreamingVadState {
                detector,
                partial_frame_buffer: Vec::with_capacity(VAD_CHUNK_SIZE),
                consecutive_silent_frames: 0,
                // Seed the guard so the very first frames of a new
                // stream pass unconditionally — covers press-start
                // word clipping on borderline leading audio.
                post_silence_guard_frames_remaining: word_guard_frames,
                suppressing: false,
            }),
            threshold,
            min_silence_frames,
            word_guard_frames,
            disabled: false,
        })
    }
}

/// Sync hot path: process a single chunk's worth of audio + leftover
/// partial-frame samples. Mutates `state` in place and returns the
/// speech-only bytes to emit.
fn run_streaming_predict(
    state: &mut StreamingVadState,
    chunk: &[i16],
    threshold: f32,
    min_silence_frames: u32,
    word_guard_frames: u32,
) -> Vec<i16> {
    state.partial_frame_buffer.extend_from_slice(chunk);
    let total_samples = state.partial_frame_buffer.len();
    let complete_frames = total_samples / VAD_CHUNK_SIZE;
    let consumed = complete_frames * VAD_CHUNK_SIZE;
    if complete_frames == 0 {
        return Vec::new();
    }
    let mut output = Vec::with_capacity(consumed);
    // Iterate the complete frames in-place. We `drain(..consumed)` at
    // the end so the leftover sub-frame samples persist for the next
    // call.
    for frame_idx in 0..complete_frames {
        let start = frame_idx * VAD_CHUNK_SIZE;
        let end = start + VAD_CHUNK_SIZE;
        let frame = &state.partial_frame_buffer[start..end];
        let prob = state.detector.predict(frame.iter().copied());
        let is_speech = prob >= threshold;

        if state.post_silence_guard_frames_remaining > 0 {
            // Word-guard window active — emit unconditionally.
            output.extend_from_slice(frame);
            state.post_silence_guard_frames_remaining -= 1;
            if is_speech {
                state.consecutive_silent_frames = 0;
                state.suppressing = false;
            } else {
                // Silence inside the guard window: counts toward
                // future suppression but does NOT suppress this
                // frame (guard is unconditional).
                state.consecutive_silent_frames = state.consecutive_silent_frames.saturating_add(1);
            }
            continue;
        }

        if is_speech {
            output.extend_from_slice(frame);
            if state.suppressing {
                // Transition silence → speech: re-arm the guard so
                // the next `word_guard_frames` pass unconditionally.
                //
                // "dropped" counts only the frames that crossed the
                // suppression latch — the pre-latch frames (count =
                // min_silence_frames - 1) were emitted to output in
                // case the silence resolved short. Reporting the
                // total silent run here would overstate the savings
                // by up to one `min_silence_ms` window (visible at
                // very short pauses where post-latch count is 0).
                let dropped_frames = state
                    .consecutive_silent_frames
                    .saturating_sub(min_silence_frames.saturating_sub(1));
                let dropped_ms =
                    (dropped_frames as usize * VAD_CHUNK_SIZE * 1000) / VAD_SAMPLE_RATE as usize;
                let total_ms = (state.consecutive_silent_frames as usize * VAD_CHUNK_SIZE * 1000)
                    / VAD_SAMPLE_RATE as usize;
                let guard_ms =
                    (word_guard_frames as usize * VAD_CHUNK_SIZE * 1000) / VAD_SAMPLE_RATE as usize;
                log::info!(
                    target: "vad",
                    "audio-hybrid: streaming VAD released suppression after speech resumed (silence lasted {total_ms} ms, {dropped_ms} ms dropped; next {guard_ms} ms passed unconditionally)"
                );
                state.post_silence_guard_frames_remaining = word_guard_frames;
            }
            state.consecutive_silent_frames = 0;
            state.suppressing = false;
            continue;
        }

        // Silence frame outside the guard window.
        state.consecutive_silent_frames = state.consecutive_silent_frames.saturating_add(1);
        if state.suppressing || state.consecutive_silent_frames >= min_silence_frames {
            if !state.suppressing {
                let silence_ms = (state.consecutive_silent_frames as usize * VAD_CHUNK_SIZE * 1000)
                    / VAD_SAMPLE_RATE as usize;
                log::info!(
                    target: "vad",
                    "audio-hybrid: streaming VAD entered suppression after {silence_ms} ms continuous silence"
                );
            }
            state.suppressing = true;
            // Drop the frame.
            continue;
        }
        // Not yet at the suppression threshold — keep the frame in
        // case the silence is short and resolves to speech before
        // we cross the latch.
        output.extend_from_slice(frame);
    }
    state.partial_frame_buffer.drain(..consumed);
    debug_assert!(
        state.partial_frame_buffer.len() < VAD_CHUNK_SIZE,
        "partial frame buffer must never carry ≥ VAD_CHUNK_SIZE samples (got {})",
        state.partial_frame_buffer.len()
    );
    output
}

#[async_trait]
impl StreamingVadDetector for SileroStreamingVad {
    fn provider_label(&self) -> &str {
        "silero_v5_stream"
    }

    fn reset(&mut self) {
        if let Some(state) = self.state.as_mut() {
            state.detector.reset();
            state.partial_frame_buffer.clear();
            state.consecutive_silent_frames = 0;
            state.post_silence_guard_frames_remaining = self.word_guard_frames;
            state.suppressing = false;
        }
    }

    async fn process_chunk(&mut self, chunk: &[i16], output: &mut Vec<i16>) {
        if self.disabled {
            output.extend_from_slice(chunk);
            return;
        }
        let Some(mut state) = self.state.take() else {
            // Should never happen: state is only `None` while a
            // spawn_blocking call holds it. Fail-open.
            output.extend_from_slice(chunk);
            return;
        };
        let chunk_owned = chunk.to_vec();
        let threshold = self.threshold;
        let min_silence_frames = self.min_silence_frames;
        let word_guard_frames = self.word_guard_frames;
        let join = tokio::task::spawn_blocking(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_streaming_predict(
                    &mut state,
                    &chunk_owned,
                    threshold,
                    min_silence_frames,
                    word_guard_frames,
                )
            }));
            (state, chunk_owned, result)
        });
        match join.await {
            Ok((returned_state, chunk_owned, Ok(speech_only))) => {
                output.extend_from_slice(&speech_only);
                self.state = Some(returned_state);
                let _ = chunk_owned;
            }
            Ok((_, chunk_owned, Err(panic))) => {
                self.disabled = true;
                let msg = panic_message(&panic);
                log::warn!(
                    target: "vad",
                    "streaming VAD inference panicked ({msg}) — instance disabled for rest of stream (fail-open)"
                );
                output.extend_from_slice(&chunk_owned);
            }
            Err(join_err) => {
                self.disabled = true;
                log::warn!(
                    target: "vad",
                    "streaming VAD spawn_blocking join failed: {join_err} — instance disabled for rest of stream (fail-open)"
                );
                output.extend_from_slice(chunk);
            }
        }
    }

    async fn extract_speech(&mut self, buffer: &[i16]) -> Vec<i16> {
        self.reset();
        let mut output = Vec::with_capacity(buffer.len());
        self.process_chunk(buffer, &mut output).await;
        // Flush any leftover sub-frame samples unconditionally — a
        // one-shot pass should never lose the tail of a buffer that
        // happens to end mid-frame.
        if let Some(state) = self.state.as_mut() {
            if !state.partial_frame_buffer.is_empty() {
                output.extend_from_slice(&state.partial_frame_buffer);
                state.partial_frame_buffer.clear();
            }
        }
        self.reset();
        output
    }
}

/// Test double scripted with a per-frame verdict list. `process_chunk`
/// consumes frames in order from the script; `true` emits the frame,
/// `false` drops it. Used by gate-composition tests that need
/// deterministic streaming behavior without loading the real ONNX
/// runtime.
#[derive(Debug, Clone)]
pub struct MockStreamingVad {
    script: Vec<bool>,
    cursor: usize,
}

impl MockStreamingVad {
    pub fn new(script: Vec<bool>) -> Self {
        Self { script, cursor: 0 }
    }
}

#[async_trait]
impl StreamingVadDetector for MockStreamingVad {
    fn provider_label(&self) -> &str {
        "mock_stream"
    }

    fn reset(&mut self) {
        self.cursor = 0;
    }

    async fn process_chunk(&mut self, chunk: &[i16], output: &mut Vec<i16>) {
        let complete_frames = chunk.len() / VAD_CHUNK_SIZE;
        for frame_idx in 0..complete_frames {
            let verdict = self.script.get(self.cursor).copied().unwrap_or(true);
            self.cursor += 1;
            if verdict {
                let start = frame_idx * VAD_CHUNK_SIZE;
                let end = start + VAD_CHUNK_SIZE;
                output.extend_from_slice(&chunk[start..end]);
            }
        }
    }

    async fn extract_speech(&mut self, buffer: &[i16]) -> Vec<i16> {
        self.reset();
        let mut output = Vec::with_capacity(buffer.len());
        self.process_chunk(buffer, &mut output).await;
        output
    }
}

/// No-op streaming detector that emits the entire input unchanged. Used
/// as the fail-open fallback by [`crate::lib::build_streaming_vad_factory`]
/// when [`SileroStreamingVad::new`] errors at construction time, so the
/// factory always returns a working detector instance.
#[derive(Debug, Clone, Default)]
pub struct PassThroughStreamingVad;

#[async_trait]
impl StreamingVadDetector for PassThroughStreamingVad {
    fn provider_label(&self) -> &str {
        "pass_through"
    }

    fn reset(&mut self) {}

    async fn process_chunk(&mut self, chunk: &[i16], output: &mut Vec<i16>) {
        output.extend_from_slice(chunk);
    }

    async fn extract_speech(&mut self, buffer: &[i16]) -> Vec<i16> {
        buffer.to_vec()
    }
}

/// Feature 025 (backlog 0046) test-only streaming detector that drops
/// *all* input — the inverse of [`PassThroughStreamingVad`]. Used by
/// the audio-LID gate's regression tests in `session.rs::tests` to
/// simulate "all-silent press" deterministically without spinning up
/// Silero. Per-instance state is trivial (no LSTM), matching the
/// `Send`-only trait contract.
#[derive(Debug, Clone, Default)]
pub struct SilentStreamingVad;

#[async_trait]
impl StreamingVadDetector for SilentStreamingVad {
    fn provider_label(&self) -> &str {
        "silent_mock"
    }

    fn reset(&mut self) {}

    async fn process_chunk(&mut self, _chunk: &[i16], _output: &mut Vec<i16>) {
        // Intentionally drop everything — emit zero bytes regardless
        // of input. Drives the audio-LID gate's "all silent" skip
        // predicate in tests.
    }

    async fn extract_speech(&mut self, _buffer: &[i16]) -> Vec<i16> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_vad_returns_canned_verdict() {
        let speech = MockVad::new(true);
        assert!(speech.predict_speech(&[0_i16; 16]).await);
        assert_eq!(speech.provider_label(), "mock");

        let silent = MockVad::new(false);
        assert!(!silent.predict_speech(&[0_i16; 16]).await);
    }

    #[tokio::test]
    async fn silent_streaming_vad_emits_zero_bytes() {
        let mut vad = SilentStreamingVad;
        let chunk = vec![1234_i16; 1000];
        let mut sink = Vec::new();
        vad.process_chunk(&chunk, &mut sink).await;
        assert!(sink.is_empty(), "SilentStreamingVad must drop all input");
        let extracted = vad.extract_speech(&chunk).await;
        assert!(
            extracted.is_empty(),
            "SilentStreamingVad's extract_speech must also drop all input"
        );
        assert_eq!(vad.provider_label(), "silent_mock");
    }

    #[test]
    fn silero_vad_construction_succeeds_with_defaults() {
        let vad = SileroVad::new(VAD_DEFAULT_THRESHOLD, VAD_DEFAULT_MIN_SPEECH_MS);
        assert!(vad.is_ok(), "expected SileroVad::new to succeed: {vad:?}");
        let vad = vad.unwrap();
        assert_eq!(vad.provider_label(), "silero_v5");
        // 250 ms / 32 ms/frame = 7.8 → 8 frames after ceil-div
        assert_eq!(vad.min_speech_frames, 8);
    }

    #[tokio::test]
    async fn silero_vad_returns_false_for_all_zero_buffer() {
        let vad = SileroVad::new(VAD_DEFAULT_THRESHOLD, VAD_DEFAULT_MIN_SPEECH_MS)
            .expect("silero builds");
        // 1 s of digital silence at 16 kHz — Silero should never call
        // this speech. This is the wrapper-bitrot guard for the
        // production gate path.
        let silence = vec![0_i16; 16_000];
        assert!(
            !vad.predict_speech(&silence).await,
            "all-zero buffer must not be classified as speech"
        );
    }

    #[tokio::test]
    #[ignore = "calibration-dependent — sine wave is not natural speech; real-voice fixture lives in the integration test"]
    async fn silero_vad_returns_true_for_obviously_voiced_buffer() {
        let vad = SileroVad::new(VAD_DEFAULT_THRESHOLD, VAD_DEFAULT_MIN_SPEECH_MS)
            .expect("silero builds");
        // 1 s of 200 Hz sine wave at half amplitude. A pure tone is
        // NOT speech, and Silero v5 (correctly) rejects it on most
        // configurations — kept as documentation of the calibration
        // bound, marked #[ignore] so CI doesn't flake.
        let voiced: Vec<i16> = (0..16_000)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                (16_384.0 * (t * 2.0 * std::f32::consts::PI * 200.0).sin()) as i16
            })
            .collect();
        assert!(vad.predict_speech(&voiced).await);
    }

    #[tokio::test]
    async fn silero_vad_threshold_above_one_yields_no_speech() {
        let vad = SileroVad::new(1.5, VAD_DEFAULT_MIN_SPEECH_MS).expect("silero builds");
        // No probability ever exceeds 1.0; threshold > 1.0 makes the
        // gate uniformly say "no speech" regardless of input.
        let silence = vec![0_i16; 16_000];
        assert!(!vad.predict_speech(&silence).await);
        let noisy: Vec<i16> = (0..16_000).map(|i| ((i % 8000) - 4000) as i16).collect();
        assert!(!vad.predict_speech(&noisy).await);
    }

    #[tokio::test]
    async fn silero_vad_empty_buffer_fails_open() {
        let vad = SileroVad::new(VAD_DEFAULT_THRESHOLD, VAD_DEFAULT_MIN_SPEECH_MS)
            .expect("silero builds");
        // Empty buffer at the boundary: fail-open contract returns true
        // so an upstream caller bug doesn't silently gate.
        assert!(vad.predict_speech(&[]).await);
    }

    #[tokio::test]
    async fn silero_vad_short_voiced_below_min_duration_returns_false() {
        // High `min_speech_ms` (5 s) on a 1 s buffer ensures the P2
        // policy can never accumulate enough frames even if the model
        // hallucinates speech here. Tests the gate's duration arm in
        // isolation from threshold tuning.
        let vad = SileroVad::new(VAD_DEFAULT_THRESHOLD, 5_000).expect("silero builds");
        let buf = vec![0_i16; 16_000];
        assert!(!vad.predict_speech(&buf).await);
    }

    // ---- feature 024 (backlog 0042): streaming VAD ------------------------

    #[tokio::test]
    async fn mock_streaming_vad_returns_scripted_output() {
        // Script: speech, speech, silence, speech → 3 of 4 frames retained.
        let mut mock = MockStreamingVad::new(vec![true, true, false, true]);
        let chunk: Vec<i16> = vec![1_i16; VAD_CHUNK_SIZE * 4];
        let mut output = Vec::new();
        mock.process_chunk(&chunk, &mut output).await;
        assert_eq!(
            output.len(),
            VAD_CHUNK_SIZE * 3,
            "MockStreamingVad must emit one frame per `true` script entry"
        );
        assert_eq!(mock.provider_label(), "mock_stream");
    }

    #[test]
    fn silero_streaming_vad_construction_succeeds_with_defaults() {
        let vad = SileroStreamingVad::new(
            VAD_DEFAULT_THRESHOLD,
            STREAM_DEFAULT_MIN_SILENCE_MS,
            STREAM_DEFAULT_WORD_GUARD_MS,
        );
        assert!(
            vad.is_ok(),
            "expected SileroStreamingVad::new to succeed: {vad:?}"
        );
        let vad = vad.unwrap();
        assert_eq!(vad.provider_label(), "silero_v5_stream");
        // 500 ms / 32 ms/frame = 15.6 → 16 frames after ceil-div.
        assert_eq!(vad.min_silence_frames, 16);
        assert_eq!(vad.word_guard_frames, 16);
    }

    #[tokio::test]
    async fn silero_streaming_vad_extract_speech_from_all_silence_returns_only_guard_window() {
        let mut vad = SileroStreamingVad::new(
            VAD_DEFAULT_THRESHOLD,
            STREAM_DEFAULT_MIN_SILENCE_MS,
            STREAM_DEFAULT_WORD_GUARD_MS,
        )
        .expect("silero builds");
        // 2 seconds of pure digital silence at 16 kHz. The first
        // `word_guard_frames` pass unconditionally (initial guard
        // window), then suppression latches and the rest is dropped.
        let silence = vec![0_i16; 16_000 * 2];
        let extracted = vad.extract_speech(&silence).await;
        let word_guard_samples =
            (STREAM_DEFAULT_WORD_GUARD_MS as usize * VAD_SAMPLE_RATE as usize) / 1000;
        let lower = word_guard_samples.saturating_sub(VAD_CHUNK_SIZE);
        let upper = word_guard_samples + VAD_CHUNK_SIZE;
        assert!(
            extracted.len() >= lower && extracted.len() <= upper,
            "expected ≈ {word_guard_samples} samples (word-guard window only), got {} (lower={lower}, upper={upper})",
            extracted.len()
        );
    }

    #[tokio::test]
    async fn pass_through_streaming_vad_returns_full_input() {
        let mut vad = PassThroughStreamingVad;
        let chunk: Vec<i16> = vec![42_i16; 1000];
        let mut output = Vec::new();
        vad.process_chunk(&chunk, &mut output).await;
        assert_eq!(output, chunk);
        assert_eq!(vad.provider_label(), "pass_through");
    }

    #[tokio::test]
    async fn silero_streaming_vad_partial_frame_carried_across_calls() {
        let mut vad = SileroStreamingVad::new(
            VAD_DEFAULT_THRESHOLD,
            STREAM_DEFAULT_MIN_SILENCE_MS,
            STREAM_DEFAULT_WORD_GUARD_MS,
        )
        .expect("silero builds");
        // Two sub-frame chunks: 256 samples each = 512 total = exactly
        // one VAD_CHUNK_SIZE frame after the second call.
        let half = vec![0_i16; 256];
        let mut out_a = Vec::new();
        vad.process_chunk(&half, &mut out_a).await;
        assert!(
            out_a.len() < VAD_CHUNK_SIZE,
            "first sub-frame call must not have emitted a complete frame (got {})",
            out_a.len()
        );
        let mut out_b = Vec::new();
        vad.process_chunk(&half, &mut out_b).await;
        // The first complete frame is inside the initial word-guard
        // window, so it passes unconditionally regardless of verdict.
        assert!(
            !out_b.is_empty(),
            "second sub-frame call should have completed at least one frame"
        );
    }
}
