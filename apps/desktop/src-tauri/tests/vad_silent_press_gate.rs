//! Integration tests for feature 023 (backlog 0040) — silent-press
//! VAD gate.
//!
//! Scope rationale (brainstorm 005 Q10b + plan Task 13 Gotcha 1): the
//! gate-composition logic itself lives inside `DictationSession`'s
//! private async fns (`spawn_audio_hybrid_inner_classify`,
//! `handle_hotkey_released`'s widen). Standing up enough of `Session`
//! to drive each gate site end-to-end would require mocking Whisper,
//! Groq, AND the audio capture forwarder — invasive for what is
//! mechanically a one-line gate insertion already covered by the
//! unit-test inline `predict_speech` assertions in `src/vad.rs::tests`
//! and the allowlist tests in `src/session.rs::tests`.
//!
//! What this file owns:
//!
//! 1. **Wrapper-bitrot guard** (`real_silero_*`): construct the real
//!    `SileroVad` against the bundled Silero v5 ONNX model and assert
//!    the contract surfaces in `MuniError` / `VadDetector` round-trip
//!    end-to-end. Catches the case where `voice_activity_detector`
//!    bumps a major version and our wrapper compiles but no longer
//!    produces meaningful predictions.
//!
//! 2. **`MockVad` round-trip via the trait object**: confirms the
//!    test double our unit tests rely on actually satisfies the
//!    `Arc<dyn VadDetector>` shape the gate sites accept.

use std::sync::Arc;

use muni_lib::vad::{
    MockVad, SileroVad, VadDetector, VAD_DEFAULT_MIN_SPEECH_MS, VAD_DEFAULT_THRESHOLD,
};

/// Real Silero loads from the in-crate bundled ONNX and rejects a
/// digital-silence buffer. This is the wrapper-bitrot guard flagged
/// in brainstorm 005 Q10b: if the `voice_activity_detector` crate
/// changes its API in a way the wrapper compiles past but the
/// runtime contract changes, this test catches it.
#[tokio::test]
async fn real_silero_loads_and_returns_false_for_all_zero_buffer() {
    let vad = SileroVad::new(VAD_DEFAULT_THRESHOLD, VAD_DEFAULT_MIN_SPEECH_MS)
        .expect("Silero v5 must build from the bundled ONNX model");
    let detector: Arc<dyn VadDetector> = Arc::new(vad);

    // 2 s of 16 kHz mono silence — generously longer than the
    // min-speech window to make sure the gate doesn't accidentally
    // report speech on the boundary chunk.
    let silence = vec![0_i16; 32_000];
    assert!(
        !detector.predict_speech(&silence).await,
        "Silero must classify all-zero buffer as no-speech (production gate would otherwise paste hallucinations)"
    );
    assert_eq!(detector.provider_label(), "silero_v5");
}

/// Real Silero handles a short buffer (smaller than the P2
/// min-speech-frames threshold) gracefully and returns false. Covers
/// the "press too short to accumulate enough consecutive frames" case
/// the gate sites hit on sub-200 ms taps.
#[tokio::test]
async fn real_silero_rejects_buffer_too_short_for_min_speech_window() {
    let vad = SileroVad::new(VAD_DEFAULT_THRESHOLD, VAD_DEFAULT_MIN_SPEECH_MS)
        .expect("Silero v5 must build");
    let detector: Arc<dyn VadDetector> = Arc::new(vad);

    // 100 ms (1600 samples at 16 kHz) of silence — below the default
    // 250 ms / 8-frame min-speech bar even if every frame were voiced.
    let short_silence = vec![0_i16; 1_600];
    assert!(!detector.predict_speech(&short_silence).await);
}

/// `MockVad` satisfies the `Arc<dyn VadDetector>` shape the gate
/// sites accept and returns its canned verdict via the trait object.
/// Pure shape test — no production wiring touched.
#[tokio::test]
async fn mock_vad_round_trips_through_trait_object() {
    let speech: Arc<dyn VadDetector> = Arc::new(MockVad::new(true));
    let silence: Arc<dyn VadDetector> = Arc::new(MockVad::new(false));

    let buf = vec![0_i16; 16_000];
    assert!(speech.predict_speech(&buf).await);
    assert!(!silence.predict_speech(&buf).await);
    assert_eq!(speech.provider_label(), "mock");
    assert_eq!(silence.provider_label(), "mock");
}
