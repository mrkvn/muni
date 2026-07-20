//! Integration tests for feature 024 (backlog 0042) — streaming VAD
//! for mid-press silence.
//!
//! Scope: load the real Silero v5 ONNX model via
//! [`muni_lib::vad::SileroStreamingVad`] and assert the trait surface
//! produces sensible output on representative speech/silence patterns.
//!
//! What this file owns:
//!
//! 1. **All-silence retains only the initial word-guard window.** The
//!    load-bearing word-start protection: even on a buffer of pure
//!    silence, the first `STREAM_DEFAULT_WORD_GUARD_MS` of audio passes
//!    unconditionally so a press that starts borderline-quiet doesn't
//!    clip the first phoneme.
//!
//! 2. **One-shot vs streamed retention parity.** Same input fed through
//!    `extract_speech` (one big call) and `process_chunk` (many small
//!    sub-frame calls) must produce the same retained-sample count.
//!    Catches partial-frame mishandling.
//!
//! 3. **Speech → silence → speech buffer trims the middle.** Marked
//!    `#[ignore]` because the "voiced" segments use a synthetic sine
//!    wave; Silero v5 does not reliably classify pure tones as speech.
//!    Run manually with a real-voice fixture when calibrating.

use muni_lib::vad::{
    SileroStreamingVad, StreamingVadDetector, STREAM_DEFAULT_MIN_SILENCE_MS,
    STREAM_DEFAULT_WORD_GUARD_MS, VAD_CHUNK_SIZE, VAD_DEFAULT_THRESHOLD, VAD_SAMPLE_RATE,
};

/// Build a synthetic test buffer: 1 s of (pseudo-)voiced audio, 5 s of
/// silence, 1 s of (pseudo-)voiced audio. Sine wave is not natural
/// speech; if this fixture fails on a fresh checkout, swap for a real-
/// voice recording committed under `tests/fixtures/`.
fn build_speech_silence_speech_buffer() -> Vec<i16> {
    let sr = VAD_SAMPLE_RATE as usize;
    let mut buf = Vec::with_capacity(sr * 7);
    buf.extend((0..sr).map(|i| {
        let t = i as f32 / sr as f32;
        (16_384.0 * (t * 2.0 * std::f32::consts::PI * 200.0).sin()) as i16
    }));
    buf.extend(std::iter::repeat(0_i16).take(sr * 5));
    buf.extend((0..sr).map(|i| {
        let t = i as f32 / sr as f32;
        (16_384.0 * (t * 2.0 * std::f32::consts::PI * 200.0).sin()) as i16
    }));
    buf
}

#[tokio::test]
async fn extract_speech_on_pure_silence_returns_only_initial_guard_window() {
    let mut vad = SileroStreamingVad::new(
        VAD_DEFAULT_THRESHOLD,
        STREAM_DEFAULT_MIN_SILENCE_MS,
        STREAM_DEFAULT_WORD_GUARD_MS,
    )
    .expect("Silero v5 must build from the bundled ONNX model");
    // 3 s of digital silence; only the initial ~500 ms word-guard
    // window survives. The rest is suppressed once the silence-frame
    // counter latches.
    let silence = vec![0_i16; VAD_SAMPLE_RATE as usize * 3];
    let extracted = vad.extract_speech(&silence).await;
    let guard_samples = (STREAM_DEFAULT_WORD_GUARD_MS as usize * VAD_SAMPLE_RATE as usize) / 1000;
    // Generous slop: the silence-frame counter takes a few frames to
    // latch suppression, so we may retain a small handful of extra
    // frames beyond the guard window. The bound is "much less than
    // input length."
    let upper_bound = guard_samples + VAD_CHUNK_SIZE * 8;
    assert!(
        extracted.len() < upper_bound,
        "extract_speech on all-silence retained {} samples; expected < {} (guard window + small slop)",
        extracted.len(),
        upper_bound,
    );
    assert!(
        extracted.len() < silence.len() / 2,
        "extract_speech on all-silence must trim at least half the buffer; got {} of {} samples retained",
        extracted.len(),
        silence.len(),
    );
}

#[tokio::test]
async fn process_chunk_streaming_matches_extract_speech_oneshot() {
    // Same input fed two ways: one big `extract_speech` call vs many
    // small `process_chunk` calls. The retained-sample count must
    // match — partial-frame leftover handling is the load-bearing
    // invariant.
    let buf = vec![0_i16; VAD_SAMPLE_RATE as usize];

    let mut vad_a = SileroStreamingVad::new(
        VAD_DEFAULT_THRESHOLD,
        STREAM_DEFAULT_MIN_SILENCE_MS,
        STREAM_DEFAULT_WORD_GUARD_MS,
    )
    .expect("silero builds");
    let one_shot = vad_a.extract_speech(&buf).await;

    let mut vad_b = SileroStreamingVad::new(
        VAD_DEFAULT_THRESHOLD,
        STREAM_DEFAULT_MIN_SILENCE_MS,
        STREAM_DEFAULT_WORD_GUARD_MS,
    )
    .expect("silero builds");
    let mut streamed = Vec::new();
    // Deliberately NOT aligned to VAD_CHUNK_SIZE so the partial-frame
    // path is exercised.
    for chunk in buf.chunks(100) {
        vad_b.process_chunk(chunk, &mut streamed).await;
    }
    // `extract_speech` flushes the trailing partial-frame remainder
    // unconditionally; `process_chunk` keeps it buffered for the next
    // call. The retained samples must therefore match up to one
    // partial-frame's worth — never more.
    assert!(
        one_shot.len() >= streamed.len(),
        "one-shot must retain ≥ streamed (one_shot={}, streamed={})",
        one_shot.len(),
        streamed.len(),
    );
    let diff = one_shot.len() - streamed.len();
    assert!(
        diff < VAD_CHUNK_SIZE,
        "one-shot retained {} extra samples; expected < VAD_CHUNK_SIZE ({}) — partial-frame mishandling?",
        diff,
        VAD_CHUNK_SIZE,
    );
}

#[tokio::test]
#[ignore = "calibration-dependent — synthetic sine may not classify as speech; run manually with real-voice fixture"]
async fn extract_speech_trims_middle_silence_keeps_edges() {
    let mut vad = SileroStreamingVad::new(
        VAD_DEFAULT_THRESHOLD,
        STREAM_DEFAULT_MIN_SILENCE_MS,
        STREAM_DEFAULT_WORD_GUARD_MS,
    )
    .expect("silero builds");
    let buf = build_speech_silence_speech_buffer();
    let extracted = vad.extract_speech(&buf).await;
    // ~7 s input. Expected upper bound: less than the original by a
    // significant margin (trim happened). Expected lower bound: more
    // than 1 s (didn't over-trim into the speech regions).
    let target_max = VAD_SAMPLE_RATE as usize * 4;
    let target_min = VAD_SAMPLE_RATE as usize;
    assert!(
        extracted.len() < target_max,
        "expected significant trim, got {} samples (original {})",
        extracted.len(),
        buf.len()
    );
    assert!(
        extracted.len() > target_min,
        "expected speech retention, got {} samples — over-trimmed?",
        extracted.len()
    );
}
