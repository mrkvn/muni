use serde::Serialize;
use thiserror::Error;

/// Severity is the *interrupt/intensity* dial, NOT the surface selector.
///
/// Feature 035 decoupled surface from severity: the surface an error takes
/// (notification, HUD notice chip, toast, or silent) is now chosen by
/// [`MuniError::surface`] from the error's origin/actionability. `severity`
/// survives only to drive the focus/interrupt decision — a `Loud` error
/// whose [`MuniError::requires_user_action_now`] is `true` brings the Main
/// window forward; a `Quiet` error never steals focus. Mirrors
/// `ErrorSeverity` from Swift v1's `MuniError.swift`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorSeverity {
    Loud,
    Quiet,
}

/// The on-screen surface an error is routed to (Feature 035).
///
/// Picked by [`MuniError::surface`] from origin/actionability rather than
/// severity:
///
/// - `Notification` — silent macOS banner; click deep-links to Settings.
///   For actionable errors (permissions, missing/rejected keys).
/// - `HudNotice` — short text chip above the HUD pill (~2.5s dwell). For
///   non-actionable FYI (connection blips, words-lost, partial recovered,
///   cleanup-skipped, no-speech).
/// - `Toast` — in-app Sonner toast (`error://quiet`). For Settings-window
///   interactions.
/// - `Silent` — log-only, for internal self-healing fallbacks (Gemini LID,
///   audio-LID, VAD, Parakeet).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorSurface {
    Notification,
    HudNotice,
    Toast,
    Silent,
}

/// Tone of a HUD notice chip (Feature 035). `Amber` flags a "words may be
/// lost / double-check" warning; `Neutral` is a plain FYI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HudTone {
    Neutral,
    Amber,
}

/// Settings tabs the error presenter can route a user toward.
///
/// Mirrors `SettingsTab` from Swift v1. Phase 1 only references `Hotkey`, but
/// the full enum is declared up-front so subsequent phases can extend
/// `MuniError` without revisiting this file.
// Variants beyond `Hotkey` are referenced by future phases (Phase 8 routes,
// Phase 10 error routing). Silence dead-code until then.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SettingsTab {
    General,
    Hotkey,
    Cleanup,
    History,
    ApiKeys,
    About,
}

/// All user-visible failure modes for Muni.
///
/// New variants are added per phase as their owning subsystem is built. Phase 1
/// covers the hotkey domain; Phase 2 adds the audio capture variants. Later
/// phases extend with Deepgram/Groq/Injection/Settings/Secrets/History/Prompt.
#[derive(Debug, Clone, Error, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MuniError {
    /// CGEventTap listening at `kCGSessionEventTap` requires Input Monitoring
    /// on macOS 10.15+. Mirrors Swift v1's `HotkeyError.inputMonitoringDenied`.
    #[error("Input Monitoring permission denied — Muni cannot observe modifier keys without it.")]
    InputMonitoringDenied,

    #[error("Failed to install macOS event tap for the hotkey listener.")]
    HotkeyTapInstallFailed,

    /// macOS / Windows microphone access has not been granted. Mirrors Swift
    /// v1's `AudioCaptureError.microphoneDenied`. Phase 2 cannot reliably
    /// distinguish a denied permission from a generic cpal stream-build
    /// failure (cpal collapses both into a `BackendSpecificError`); the
    /// dedicated AVCaptureDevice probe lands in Phase 9 onboarding, which is
    /// the first construction site for this variant outside tests.
    #[allow(dead_code)]
    #[error("Microphone permission denied — Muni cannot record audio without it.")]
    MicrophoneDenied,

    /// No default input device is available (e.g. headphones unplugged with no
    /// built-in mic, or all inputs disabled in System Settings).
    #[error("No microphone is available. Connect or enable a microphone and try again.")]
    NoInputDevice,

    /// `cpal` failed to build or start the input stream. The wrapped string
    /// carries the underlying device-/host-specific reason for logging only.
    /// Struct-shaped (rather than tuple) so the internally-tagged serde
    /// representation has somewhere to put the kind discriminator.
    #[error("Couldn't start audio capture: {reason}")]
    AudioStreamFailed { reason: String },

    /// Phase 3 — Deepgram streaming ASR. Mirrors Swift v1's `DeepgramError`.
    ///
    /// Loud: the user must do something (configure the API key) before
    /// dictation can work at all.
    #[error("Deepgram API key is missing. Add it in Settings → API Keys.")]
    DeepgramMissingApiKey,

    /// Quiet — the WebSocket failed to open or dropped mid-session. The
    /// wrapped reason is logged but not shown to the user verbatim.
    #[error("Couldn't connect to Deepgram: {reason}")]
    DeepgramConnectionFailed { reason: String },

    /// Quiet — Deepgram returned a message that didn't parse as the expected
    /// `Results`/`Metadata`/`Error`/`UtteranceEnd`/`SpeechStarted` envelope,
    /// or `finalize` was called without a successful `open`.
    #[error("Received an invalid response from Deepgram.")]
    DeepgramInvalidResponse,

    /// Quiet — Deepgram returned an `Error`-typed message (e.g. quota
    /// exhausted, bad audio header). Message is the server-supplied
    /// `description` field.
    #[error("Deepgram error: {message}")]
    DeepgramServerError { message: String },

    /// Phase 4 — Groq cleanup. Loud counterpart of `DeepgramMissingApiKey`:
    /// the user must configure the key before cleanup can run at all.
    #[error("Groq API key is missing. Add it in Settings → API Keys.")]
    GroqMissingApiKey,

    /// Quiet — the HTTPS request to Groq failed (DNS, TLS, transport, or
    /// timeout). The wrapped reason is logged; the user-facing copy mirrors
    /// Swift v1's `connectionFailed` — `Couldn't reach Groq: ...`.
    #[error("Couldn't reach Groq: {reason}")]
    GroqConnectionFailed { reason: String },

    /// Quiet — the streaming SSE response was malformed (no `Content-Type`,
    /// invalid `data:` payload, or non-HTTP-shaped reply). Mirrors Swift v1's
    /// `invalidResponse`.
    #[error("Received an invalid response from Groq.")]
    GroqInvalidResponse,

    /// Quiet — Groq replied with a non-2xx status. Body is captured (truncated
    /// to ~4 KB by the caller) for log diagnostics; the user-facing copy
    /// mirrors Swift v1 by surfacing only the status code.
    #[error("Groq returned HTTP {status}.")]
    GroqServerError { status: u16, body: String },

    /// Phase 4 — `CleanupPrompt.md` could not be located on disk OR loaded as
    /// UTF-8. Mirrors Swift v1's `CleanupPromptError.resourceMissing`. Loud
    /// because the cleanup prompt is shipped as a bundle resource — its
    /// absence indicates a packaging problem, not a recoverable transient.
    #[error("Cleanup prompt resource missing from app bundle.")]
    CleanupPromptMissing,

    /// Phase 5 — Accessibility permission missing. Without it, posting Cmd+V
    /// via `CGEvent` is silently dropped by the system, so paste cannot work.
    /// Mirrors Swift v1's `TextInjectorError.accessibilityDenied`. Loud — the
    /// user must grant the permission before any paste will work at all.
    #[error("Muni needs Accessibility permission to paste text into apps.")]
    AccessibilityDenied,

    /// Phase 5 — Caller asked us to paste an empty string. Quiet — the user
    /// likely held the hotkey without speaking, or every word came back blank
    /// from the cleanup model.
    #[error("Nothing to paste.")]
    NothingToPaste,

    /// Phase 5 — Text injection requested on a platform that doesn't have a
    /// real injector (Windows port is v2). Loud because the app is unusable
    /// in this state.
    #[error("Text injection isn't supported on this platform yet.")]
    PlatformUnsupported,

    /// Feature 003 — Gemini text-LID. Loud counterpart of
    /// `GroqMissingApiKey` and `DeepgramMissingApiKey`: the user must
    /// configure a key for auto-detect to use Gemini classification.
    /// When this fires from the LID path the orchestrator silently
    /// falls back to Whisper (the safe path that handles both
    /// languages) — surfacing the variant via emit_error is reserved
    /// for the validator IPC and any future explicit handler.
    #[error("Gemini API key is missing. Add it in Settings → API Keys.")]
    GeminiMissingApiKey,

    /// Quiet — HTTPS request to Gemini failed (DNS, TLS, transport,
    /// timeout). The wrapped reason is logged; the press silently
    /// falls through to the Whisper-fallback decision in
    /// `spawn_lid_task`.
    #[error("Couldn't reach Gemini for language detection: {reason}")]
    GeminiConnectionFailed { reason: String },

    /// Quiet — Gemini replied with a non-2xx status. Body is captured
    /// (truncated) for log diagnostics.
    #[error("Gemini returned HTTP {status}.")]
    GeminiServerError { status: u16, body: String },

    /// Quiet — Gemini replied 2xx but the response body didn't carry
    /// a parseable `candidates[0].content.parts[0].text`. Treat as a
    /// classification failure → default to Whisper.
    #[error("Received an invalid response from Gemini.")]
    GeminiInvalidResponse,

    /// Phase 8 — caller asked the secrets store to save an empty value.
    /// Loud because it always indicates a UI bug (the Save button should
    /// be disabled when the input is empty); surfacing it visibly during
    /// development beats silently writing a blank entry.
    #[error("API key cannot be empty.")]
    EmptyApiKey,

    /// Phase 8 — the OS keyring rejected a write (Keychain locked, agent
    /// unavailable, sandboxed CI runner without a keychain). Loud — the
    /// user must either unlock the keychain or fix their environment
    /// before the key can be persisted.
    #[error("Couldn't save to the keychain: {reason}")]
    KeychainWriteFailed { reason: String },

    /// Slice 10 (plan 039 task 29) — a keychain *read* could not be
    /// completed because the keychain is locked / access was denied by an
    /// ACL, NOT because the entry is genuinely absent. Distinct from the
    /// per-provider `*MissingApiKey` errors on purpose: the fix here is
    /// "unlock your keychain / restart", never "add your key in Settings".
    /// Loud + Notification — dictation can't read the stored key until the
    /// keychain is reachable again, so the user needs to know.
    #[error("macOS keychain is unavailable (locked or access denied).")]
    KeychainUnavailable,

    /// Phase 8 — IPC arrived with a service identifier the secrets module
    /// doesn't know how to route. Quiet — it's a frontend-side typo, not
    /// something the user can fix from the UI.
    #[error("Unknown secrets service: {service}")]
    UnknownSecretsService { service: String },

    /// Phase 8 — Cleanup-prompt editor saved an empty override. Loud —
    /// the UI should disable Save in this state, but if it fires anyway
    /// surface it loudly so the bug is visible.
    #[error("Cleanup prompt cannot be empty.")]
    CleanupPromptInvalid,

    /// Phase 8 — Couldn't write or remove the cleanup-prompt override
    /// file (permissions, full disk, etc). Loud — the user must clear
    /// the underlying issue before the override sticks.
    #[error("Couldn't update the cleanup prompt: {reason}")]
    CleanupPromptWriteFailed { reason: String },

    /// Gladia Solaria-1 fallback ASR — loud counterpart of
    /// `DeepgramMissingApiKey`. Raised when the Whisper-batch fallback
    /// attempts to open a Gladia session without an API key configured.
    /// In practice `attempt_gladia_fallback_transcribe` short-circuits
    /// on the missing key before reaching `GladiaClient::open`, so this
    /// variant exists primarily as the typed error returned by the
    /// shared `GladiaClient` constructor (used by the fallback and
    /// surfaced via the `secrets::get` route on save/probe).
    #[error("Gladia API key is missing. Add it in Settings → API Keys.")]
    GladiaMissingApiKey,

    /// Quiet — the POST `/v2/live` HTTPS request or the subsequent
    /// WebSocket handshake failed (DNS, TLS, transport, timeout).
    #[error("Couldn't connect to Gladia: {reason}")]
    GladiaConnectionFailed { reason: String },

    /// Quiet — Gladia returned a message we couldn't parse as the
    /// documented `transcript`/`error`/lifecycle envelope, OR the
    /// `POST /v2/live` response body didn't parse as `{id, url}`.
    #[error("Received an invalid response from Gladia.")]
    GladiaInvalidResponse,

    /// Quiet — Gladia returned a non-2xx status on `POST /v2/live`,
    /// or emitted an error frame on the WebSocket. Message is the
    /// server-supplied reason (status code or `description` field).
    #[error("Gladia error: {message}")]
    GladiaServerError { message: String },

    /// Loud — `finalize()` waited the full `FINALIZE_TIMEOUT` after
    /// Gladia had already acknowledged at least one `audio_chunk`
    /// frame on the WebSocket. This is distinct from
    /// `GladiaConnectionFailed`: the handshake worked, audio was
    /// flowing, then the server went silent mid-press — typically a
    /// transient Gladia-side outage or local-network blip. Promoted
    /// to Loud (backlog 0019) so the user actually sees the failure
    /// instead of losing a press to a silent empty paste, and knows
    /// to redictate.
    ///
    /// Plan 012: starting with the Whisper-fallback recovery layer,
    /// this variant is *internal-only*: it's still emitted via
    /// `emit_error` so the orchestrator logs / telemetry can record
    /// the Gladia-specific failure mode, but the orchestrator wraps
    /// the call in a recovery attempt before reaching the user. Only
    /// `TranscriptionUnavailable` reaches the user as a notification.
    #[error("Gladia didn't respond. Try again.")]
    GladiaFinalizeTimeoutAfterAudio,

    /// Both ASR backends failed for this press. Backend-agnostic copy
    /// on purpose: the user shouldn't have to learn "Gladia" or
    /// "Whisper" vocabulary to understand the failure. Loud — no paste
    /// landed, the user must redictate, and the menu-bar app's main
    /// window is almost always backgrounded so the Quiet HUD pill
    /// alone isn't enough.
    ///
    /// Post-feature-020 emit site: `finalize_auto_detect`'s
    /// `RouterDecision::Whisper` arm, when Groq Whisper's `transcribe`
    /// returns `Err` AND `attempt_gladia_fallback_transcribe` returns
    /// `None` (no Gladia key configured, or Gladia open/send/finalize
    /// also failed). The original Groq error is still logged and
    /// recorded as a failed usage row for diagnostics; only the
    /// user-facing surface is wrapped in this variant.
    ///
    /// Originally introduced (feature 012) when Gladia was primary
    /// and Whisper was the recovery layer; the emit roles flipped
    /// after feature 020 made audio-LID the default, but the
    /// "primary + recovery both failed → user must redictate"
    /// semantics are unchanged.
    #[error("Couldn't transcribe — check your connection and try again.")]
    TranscriptionUnavailable,

    /// Feature 020 — local audio-LID model failed to load at app boot
    /// (file missing, corrupt, or whisper.cpp rejected it). Quiet —
    /// the orchestrator's fallback is "no audio-LID classifier → fall
    /// back to multilingual ASR directly", so the user still gets a
    /// paste; the dev log carries the reason.
    #[error("Couldn't load the audio language detector: {reason}")]
    AudioLidLoadFailed { reason: String },

    /// Feature 020 — per-press inference failure inside
    /// `WhisperAudioLid::classify` (encoder error, empty buffer, etc).
    /// Quiet — the orchestrator's per-press fallback ("on LID failure
    /// → route to multilingual") absorbs it.
    #[error("Audio language detection failed: {reason}")]
    AudioLidInferenceFailed { reason: String },

    /// Feature 023 (backlog 0040) — Silero VAD model failed to load at
    /// boot (bundled ONNX missing, ORT init failed, etc). Quiet —
    /// `build_vad_detector()` already degrades to "no VAD gate" so the
    /// app still works; the dev log carries the reason. The
    /// `MUNI_VAD_REQUIRED=1` escape hatch promotes the failure to a
    /// boot-time panic for CI / dev assertions.
    #[error("Couldn't load the voice activity detector: {reason}")]
    VadLoadFailed { reason: String },

    /// Feature 023 (backlog 0040) — per-call VAD inference failure
    /// inside `SileroVad::predict_speech` (Mutex poisoning, ORT
    /// surface error, etc). Quiet — the predicate fails open (treats
    /// as "speech detected") so no real dictation gets silently
    /// gated; the dev log carries the reason and the detector is
    /// disabled for the rest of the session to avoid log floods.
    #[error("Voice activity detection failed: {reason}")]
    VadInferenceFailed { reason: String },

    /// Parakeet local-ASR sidecar could not be started at boot (binary or
    /// model dir missing, or it never reported READY within the timeout).
    /// Quiet — the English path degrades to Deepgram so the user still gets
    /// a paste; the dev log carries the reason. Emitted from
    /// `ParakeetClient::spawn`, mapped to `parakeet: None` in `setup()`.
    #[error("Couldn't start the Parakeet sidecar: {reason}")]
    ParakeetSidecarUnavailable { reason: String },

    /// Parakeet per-press transcription failed (sidecar crash, pipe error,
    /// or it returned an `ERR:` body). Quiet — the English arm of
    /// `finalize_auto_detect` falls back to the Deepgram finalize path for
    /// that press; the dev log carries the reason and the sidecar is lazily
    /// respawned on the next press.
    #[error("Parakeet transcription failed: {reason}")]
    ParakeetTranscribeFailed { reason: String },

    /// Feature 034 — a Deepgram press's `finalize()` handshake failed
    /// (timeout or mid-press disconnect) but the chunks Deepgram had
    /// already streamed are recovered and pasted. Quiet — this is a
    /// success-with-warning, not a failure: text DID land, so the copy
    /// stays non-alarming and `requires_user_action_now()` is `false`
    /// (the presenter must not steal focus). Flashes the amber HUD pill
    /// so the user knows to double-check the recovered text for
    /// truncation.
    #[error("Connection dropped — pasted what was captured. Double-check it's complete.")]
    DeepgramPartialRecovered,

    /// Feature 035 — Deepgram rejected the API key at the WebSocket handshake
    /// (HTTP 401/403). Actionable: the key is expired or revoked, so dictation
    /// can't run until the user updates it. Surfaces as a silent notification
    /// deep-linking to Settings → API Keys. The auth status code is logged at
    /// the client site, not carried in the variant.
    #[error("Deepgram rejected the API key (401/403).")]
    DeepgramKeyRejected,

    /// Feature 035 — Gladia rejected the API key (HTTP 401/403 on the
    /// `POST /v2/live` request or the WebSocket handshake). Actionable; routes
    /// to Settings → API Keys. Auth status logged at the client site.
    #[error("Gladia rejected the API key (401/403).")]
    GladiaKeyRejected,

    /// Feature 035 — Groq rejected the API key (HTTP 401/403). Actionable, but
    /// non-blocking: the raw Deepgram transcript still pastes, so cleanup just
    /// degrades. Routes to Settings → API Keys; auth status logged at the
    /// client site.
    #[error("Groq rejected the API key (401/403).")]
    GroqKeyRejected,

    /// Plan 039 task 37 — synthesizing the paste keystroke itself failed:
    /// `CGEventSource::new` or `CGEvent::new_keyboard_event` returned an error
    /// while posting the `⌘V` (or the follow-up Return) in
    /// `injection::macos`. Distinct from [`Self::AccessibilityDenied`] (a
    /// permission the user grants) and from [`Self::HotkeyTapInstallFailed`]
    /// (the *listener* install, a wholly different subsystem it used to be
    /// mis-mapped to): this is a transient CoreGraphics event-creation fault at
    /// paste time, so the copy is paste-specific and points the user at a retry,
    /// not at a settings toggle. Surfaces as a mid-dictation HUD notice — the
    /// paste didn't land, so the amber "double-check" tone applies.
    #[error("Couldn't post the paste keystroke to macOS.")]
    PasteInjectionFailed,
}

impl MuniError {
    /// Severity classification used by the (future) ErrorPresenter.
    pub fn severity(&self) -> ErrorSeverity {
        match self {
            MuniError::InputMonitoringDenied
            | MuniError::HotkeyTapInstallFailed
            | MuniError::MicrophoneDenied
            | MuniError::NoInputDevice
            | MuniError::AudioStreamFailed { .. }
            | MuniError::DeepgramMissingApiKey
            | MuniError::GroqMissingApiKey
            | MuniError::GeminiMissingApiKey
            | MuniError::CleanupPromptMissing
            | MuniError::AccessibilityDenied
            | MuniError::PlatformUnsupported
            | MuniError::EmptyApiKey
            | MuniError::KeychainWriteFailed { .. }
            | MuniError::KeychainUnavailable
            | MuniError::CleanupPromptInvalid
            | MuniError::CleanupPromptWriteFailed { .. }
            | MuniError::GladiaMissingApiKey
            | MuniError::GladiaFinalizeTimeoutAfterAudio
            | MuniError::TranscriptionUnavailable
            // Feature 035 — rejected keys are Loud (Notification surface);
            // severity here only drives the focus decision below.
            | MuniError::DeepgramKeyRejected
            | MuniError::GladiaKeyRejected
            | MuniError::GroqKeyRejected
            // Plan 039 task 37 — a failed paste keystroke means no text landed;
            // Loud so the focus/interrupt dial treats it seriously (though the
            // HudNotice surface never steals focus).
            | MuniError::PasteInjectionFailed => ErrorSeverity::Loud,
            MuniError::DeepgramConnectionFailed { .. }
            | MuniError::DeepgramInvalidResponse
            | MuniError::DeepgramServerError { .. }
            | MuniError::GroqConnectionFailed { .. }
            | MuniError::GroqInvalidResponse
            | MuniError::GroqServerError { .. }
            | MuniError::GeminiConnectionFailed { .. }
            | MuniError::GeminiInvalidResponse
            | MuniError::GeminiServerError { .. }
            | MuniError::NothingToPaste
            | MuniError::UnknownSecretsService { .. }
            | MuniError::GladiaConnectionFailed { .. }
            | MuniError::GladiaInvalidResponse
            | MuniError::GladiaServerError { .. }
            | MuniError::AudioLidLoadFailed { .. }
            | MuniError::AudioLidInferenceFailed { .. }
            | MuniError::VadLoadFailed { .. }
            | MuniError::VadInferenceFailed { .. }
            | MuniError::ParakeetSidecarUnavailable { .. }
            | MuniError::ParakeetTranscribeFailed { .. }
            | MuniError::DeepgramPartialRecovered => ErrorSeverity::Quiet,
        }
    }

    /// User-facing copy. Mirrors the `userMessage` strings from the Swift
    /// per-domain error enums so wording stays stable across the rewrite.
    pub fn user_message(&self) -> String {
        match self {
            MuniError::InputMonitoringDenied => {
                "Muni needs Input Monitoring permission to detect your push-to-talk hotkey. \
                 Grant it in System Settings → Privacy & Security → Input Monitoring."
                    .to_string()
            }
            MuniError::HotkeyTapInstallFailed => {
                "Muni could not install its keyboard listener. Restart Muni; if the issue \
                 persists, restart your Mac."
                    .to_string()
            }
            MuniError::MicrophoneDenied => {
                // macOS caches the AVAuthorizationStatus per-process,
                // so a runtime grant or revoke only takes effect after
                // a restart. Lead the user there directly — pointing
                // them at System Settings without mentioning the
                // restart leaves them stuck in the same loop the QA
                // session surfaced (toggle on, click Later, dictate,
                // see this message, repeat).
                "Muni isn't receiving any microphone audio. Grant access in System \
                 Settings → Privacy & Security → Microphone, then restart Muni — \
                 macOS only refreshes the decision after a relaunch."
                    .to_string()
            }
            MuniError::NoInputDevice => {
                "No microphone is available. Connect a microphone or enable one in \
                 System Settings → Sound → Input."
                    .to_string()
            }
            MuniError::AudioStreamFailed { .. } => {
                "Couldn't start audio capture. Check your microphone and try again.".to_string()
            }
            MuniError::DeepgramMissingApiKey => {
                "Deepgram API key is missing. Add it in Settings → API Keys.".to_string()
            }
            MuniError::DeepgramConnectionFailed { .. } => {
                "Couldn't connect to Deepgram.".to_string()
            }
            MuniError::DeepgramInvalidResponse => {
                "Received an invalid response from Deepgram.".to_string()
            }
            MuniError::DeepgramServerError { .. } => "Deepgram reported an error.".to_string(),
            MuniError::GroqMissingApiKey => {
                "Groq API key is missing. Add it in Settings → API Keys.".to_string()
            }
            MuniError::GroqConnectionFailed { .. } => "Couldn't reach Groq.".to_string(),
            MuniError::GroqInvalidResponse => "Received an invalid response from Groq.".to_string(),
            MuniError::GroqServerError { .. } => "Groq reported an error.".to_string(),
            MuniError::GeminiMissingApiKey => {
                "Gemini API key is missing. Add it in Settings → API Keys.".to_string()
            }
            MuniError::GeminiConnectionFailed { .. } => {
                "Couldn't reach Gemini for language detection.".to_string()
            }
            MuniError::GeminiServerError { .. } => {
                "Gemini reported an error during language detection.".to_string()
            }
            MuniError::GeminiInvalidResponse => {
                "Received an invalid response from Gemini.".to_string()
            }
            MuniError::CleanupPromptMissing => {
                "Cleanup prompt resource missing from app bundle.".to_string()
            }
            MuniError::AccessibilityDenied => {
                "Muni needs Accessibility permission to paste text into apps. \
                 Grant it in System Settings → Privacy & Security → Accessibility."
                    .to_string()
            }
            MuniError::NothingToPaste => "Nothing to paste.".to_string(),
            MuniError::PlatformUnsupported => {
                "Text injection isn't supported on this platform yet.".to_string()
            }
            MuniError::EmptyApiKey => "API key cannot be empty.".to_string(),
            MuniError::KeychainWriteFailed { .. } => "Couldn't save to the keychain.".to_string(),
            MuniError::KeychainUnavailable => {
                "macOS keychain is unavailable — unlock your keychain (or restart your Mac), \
                 then try again."
                    .to_string()
            }
            MuniError::UnknownSecretsService { .. } => "Unknown secrets service.".to_string(),
            MuniError::CleanupPromptInvalid => "Cleanup prompt cannot be empty.".to_string(),
            MuniError::CleanupPromptWriteFailed { .. } => {
                "Couldn't update the cleanup prompt.".to_string()
            }
            MuniError::GladiaMissingApiKey => {
                "Gladia API key is missing. Add it in Settings → API Keys.".to_string()
            }
            MuniError::GladiaConnectionFailed { .. } => "Couldn't connect to Gladia.".to_string(),
            MuniError::GladiaInvalidResponse => {
                "Received an invalid response from Gladia.".to_string()
            }
            MuniError::GladiaServerError { .. } => "Gladia reported an error.".to_string(),
            MuniError::GladiaFinalizeTimeoutAfterAudio => {
                "Gladia didn't respond. Try again.".to_string()
            }
            MuniError::TranscriptionUnavailable => {
                "Couldn't transcribe — check your connection and try again.".to_string()
            }
            MuniError::AudioLidLoadFailed { .. } => {
                "Couldn't load the audio language detector.".to_string()
            }
            MuniError::AudioLidInferenceFailed { .. } => {
                "Audio language detection failed.".to_string()
            }
            MuniError::VadLoadFailed { .. } => {
                "Couldn't load the voice activity detector.".to_string()
            }
            MuniError::VadInferenceFailed { .. } => "Voice activity detection failed.".to_string(),
            MuniError::ParakeetSidecarUnavailable { .. } => {
                "Couldn't start the Parakeet sidecar.".to_string()
            }
            MuniError::ParakeetTranscribeFailed { .. } => {
                "Parakeet transcription failed.".to_string()
            }
            MuniError::DeepgramPartialRecovered => {
                "Connection dropped — pasted what was captured. Double-check it's complete."
                    .to_string()
            }
            MuniError::DeepgramKeyRejected => {
                "Your Deepgram API key was rejected — it may be expired or revoked. \
                 Update it in Settings → API Keys."
                    .to_string()
            }
            MuniError::GladiaKeyRejected => {
                "Your Gladia API key was rejected — it may be expired or revoked. \
                 Update it in Settings → API Keys."
                    .to_string()
            }
            MuniError::GroqKeyRejected => {
                "Your Groq API key was rejected — it may be expired or revoked. \
                 Update it in Settings → API Keys."
                    .to_string()
            }
            MuniError::PasteInjectionFailed => {
                "Muni couldn't paste the text into the focused app. Try again.".to_string()
            }
        }
    }

    /// True when surfacing this error must interrupt the user — i.e. no
    /// paste will land for this press, so stealing focus to surface the
    /// fix is welcome rather than disruptive.
    ///
    /// Used by the ErrorPresenter to decide whether to bring the Main
    /// window forward in addition to firing a notification. Errors with
    /// a working raw-text fallback (most Groq + cleanup-prompt paths)
    /// return `false` because the user already has their text in their
    /// editor; pulling focus into Muni at that moment would interrupt
    /// the actual workflow they came to do.
    pub fn requires_user_action_now(&self) -> bool {
        match self {
            // Dictation cannot proceed without these — the user has no
            // text to keep working with.
            MuniError::InputMonitoringDenied
            | MuniError::HotkeyTapInstallFailed
            | MuniError::MicrophoneDenied
            | MuniError::NoInputDevice
            | MuniError::AudioStreamFailed { .. }
            | MuniError::DeepgramMissingApiKey
            | MuniError::DeepgramConnectionFailed { .. }
            | MuniError::DeepgramInvalidResponse
            | MuniError::DeepgramServerError { .. }
            | MuniError::AccessibilityDenied
            | MuniError::PlatformUnsupported
            | MuniError::EmptyApiKey
            | MuniError::KeychainWriteFailed { .. }
            // A locked/denied keychain blocks reading the stored key, so no
            // transcript can land — surfacing it with focus is welcome.
            | MuniError::KeychainUnavailable
            | MuniError::GladiaMissingApiKey
            | MuniError::GladiaConnectionFailed { .. }
            | MuniError::GladiaInvalidResponse
            | MuniError::GladiaServerError { .. }
            | MuniError::GladiaFinalizeTimeoutAfterAudio
            | MuniError::TranscriptionUnavailable
            // Feature 035 — a rejected ASR key blocks dictation entirely
            // (no transcript lands), so surfacing it with focus is welcome.
            | MuniError::DeepgramKeyRejected
            | MuniError::GladiaKeyRejected => true,

            // Raw-text fallback (or no-op for these): the orchestrator
            // pastes the unmodified Deepgram transcript, so the user
            // got something they can keep editing. Don't interrupt.
            MuniError::GroqMissingApiKey
            | MuniError::GroqConnectionFailed { .. }
            | MuniError::GroqInvalidResponse
            | MuniError::GroqServerError { .. }
            | MuniError::GeminiMissingApiKey
            | MuniError::GeminiConnectionFailed { .. }
            | MuniError::GeminiInvalidResponse
            | MuniError::GeminiServerError { .. }
            | MuniError::CleanupPromptMissing
            | MuniError::CleanupPromptInvalid
            | MuniError::CleanupPromptWriteFailed { .. }
            | MuniError::NothingToPaste
            | MuniError::UnknownSecretsService { .. }
            | MuniError::AudioLidLoadFailed { .. }
            | MuniError::AudioLidInferenceFailed { .. }
            | MuniError::VadLoadFailed { .. }
            | MuniError::VadInferenceFailed { .. }
            | MuniError::ParakeetSidecarUnavailable { .. }
            | MuniError::ParakeetTranscribeFailed { .. }
            // Partial recovery already pasted the captured text — don't
            // steal focus; the amber pill is enough.
            | MuniError::DeepgramPartialRecovered
            // Feature 035 — a rejected Groq key only degrades cleanup; the
            // raw Deepgram transcript still pastes, so don't interrupt.
            | MuniError::GroqKeyRejected
            // Plan 039 task 37 — nothing the user can fix in-app (a transient
            // CoreGraphics fault); the retry lives in redictating, and the
            // HudNotice surface never consults this flag anyway.
            | MuniError::PasteInjectionFailed => false,
        }
    }

    /// Optional settings tab the UI can route to from a notification click.
    /// First consumer is the ErrorPresenter wired in Phase 10.
    ///
    /// Phase 11 update: every macOS-permission failure now routes to
    /// `General`. The Permissions card on Settings → General surfaces
    /// the live status of Microphone, Accessibility, and Input
    /// Monitoring with a per-row "Open System Settings" deep link, so
    /// it's the only Settings destination that actually helps the user
    /// fix a denied permission. The previous routing (Accessibility +
    /// Input Monitoring → `Hotkey`) was a placeholder choice from
    /// Phase 10 when Settings had no permissions surface to point at.
    #[allow(dead_code)]
    pub fn settings_tab(&self) -> Option<SettingsTab> {
        match self {
            MuniError::InputMonitoringDenied
            | MuniError::HotkeyTapInstallFailed
            | MuniError::MicrophoneDenied
            | MuniError::NoInputDevice
            | MuniError::AudioStreamFailed { .. }
            | MuniError::AccessibilityDenied => Some(SettingsTab::General),
            MuniError::DeepgramMissingApiKey
            | MuniError::GroqMissingApiKey
            | MuniError::GeminiMissingApiKey
            | MuniError::GladiaMissingApiKey
            // Feature 035 — rejected keys are fixed on the API Keys tab.
            | MuniError::DeepgramKeyRejected
            | MuniError::GladiaKeyRejected
            | MuniError::GroqKeyRejected => Some(SettingsTab::ApiKeys),
            MuniError::CleanupPromptMissing => Some(SettingsTab::Cleanup),
            MuniError::EmptyApiKey | MuniError::KeychainWriteFailed { .. } => {
                Some(SettingsTab::ApiKeys)
            }
            MuniError::CleanupPromptInvalid | MuniError::CleanupPromptWriteFailed { .. } => {
                Some(SettingsTab::Cleanup)
            }
            // No Settings tab resolves a locked/denied keychain — the fix is a
            // system-level action (unlock keychain / restart), so leave it
            // unrouted rather than pointing at API Keys (which would wrongly
            // imply "re-enter your key").
            MuniError::KeychainUnavailable => None,
            MuniError::DeepgramConnectionFailed { .. }
            | MuniError::DeepgramInvalidResponse
            | MuniError::DeepgramServerError { .. }
            | MuniError::GroqConnectionFailed { .. }
            | MuniError::GroqInvalidResponse
            | MuniError::GroqServerError { .. }
            | MuniError::GeminiConnectionFailed { .. }
            | MuniError::GeminiInvalidResponse
            | MuniError::GeminiServerError { .. }
            | MuniError::NothingToPaste
            | MuniError::PlatformUnsupported
            | MuniError::UnknownSecretsService { .. }
            | MuniError::GladiaConnectionFailed { .. }
            | MuniError::GladiaInvalidResponse
            | MuniError::GladiaServerError { .. }
            | MuniError::GladiaFinalizeTimeoutAfterAudio
            | MuniError::TranscriptionUnavailable
            | MuniError::AudioLidLoadFailed { .. }
            | MuniError::AudioLidInferenceFailed { .. }
            | MuniError::VadLoadFailed { .. }
            | MuniError::VadInferenceFailed { .. }
            | MuniError::ParakeetSidecarUnavailable { .. }
            | MuniError::ParakeetTranscribeFailed { .. }
            | MuniError::DeepgramPartialRecovered
            // Plan 039 task 37 — no Settings tab resolves a transient paste
            // fault; pointing anywhere would be misleading.
            | MuniError::PasteInjectionFailed => None,
        }
    }

    /// Feature 035 — the on-screen surface this error routes to, picked by
    /// origin/actionability (not severity). See [`ErrorSurface`] for the
    /// four tiers and the Surface mapping table in feature plan 035.
    ///
    /// Exhaustive on purpose (no wildcard arm): a new `MuniError` variant
    /// must declare its surface here, so the routing decision is never made
    /// silently by a fall-through.
    pub fn surface(&self) -> ErrorSurface {
        match self {
            // Actionable — the user must fix a permission or key before (or
            // to restore) normal operation. Silent notification.
            MuniError::InputMonitoringDenied
            | MuniError::HotkeyTapInstallFailed
            | MuniError::MicrophoneDenied
            | MuniError::NoInputDevice
            | MuniError::AudioStreamFailed { .. }
            | MuniError::AccessibilityDenied
            | MuniError::PlatformUnsupported
            | MuniError::DeepgramMissingApiKey
            | MuniError::GladiaMissingApiKey
            | MuniError::GroqMissingApiKey
            | MuniError::DeepgramKeyRejected
            | MuniError::GladiaKeyRejected
            | MuniError::GroqKeyRejected
            // A locked/denied keychain is actionable (unlock it) and can fire
            // while the Main window is closed, so a silent banner is the right
            // surface.
            | MuniError::KeychainUnavailable => ErrorSurface::Notification,

            // Non-actionable FYI surfaced mid-dictation — connection blips,
            // server hiccups, words-lost / partial-recovered, cleanup-skipped,
            // no-speech. Short HUD notice chip above the pill.
            MuniError::DeepgramConnectionFailed { .. }
            | MuniError::DeepgramServerError { .. }
            | MuniError::DeepgramInvalidResponse
            | MuniError::GladiaConnectionFailed { .. }
            | MuniError::GladiaServerError { .. }
            | MuniError::GladiaInvalidResponse
            | MuniError::DeepgramPartialRecovered
            | MuniError::GroqConnectionFailed { .. }
            | MuniError::GroqServerError { .. }
            | MuniError::GroqInvalidResponse
            | MuniError::CleanupPromptMissing
            | MuniError::TranscriptionUnavailable
            | MuniError::GladiaFinalizeTimeoutAfterAudio
            // Plan 039 task 37 — a paste fault fires at delivery time (main
            // window backgrounded); a chip above the pill is the in-context cue.
            | MuniError::PasteInjectionFailed
            | MuniError::NothingToPaste => ErrorSurface::HudNotice,

            // Settings-window interactions — the Main window is open, so an
            // in-app toast is the right surface.
            MuniError::EmptyApiKey
            | MuniError::KeychainWriteFailed { .. }
            | MuniError::CleanupPromptInvalid
            | MuniError::CleanupPromptWriteFailed { .. }
            | MuniError::UnknownSecretsService { .. } => ErrorSurface::Toast,

            // Internal self-healing fallbacks the user never needs to know
            // about (Gemini LID, audio-LID, VAD, Parakeet). Log-only.
            MuniError::GeminiMissingApiKey
            | MuniError::GeminiConnectionFailed { .. }
            | MuniError::GeminiServerError { .. }
            | MuniError::GeminiInvalidResponse
            | MuniError::AudioLidLoadFailed { .. }
            | MuniError::AudioLidInferenceFailed { .. }
            | MuniError::VadLoadFailed { .. }
            | MuniError::VadInferenceFailed { .. }
            | MuniError::ParakeetSidecarUnavailable { .. }
            | MuniError::ParakeetTranscribeFailed { .. } => ErrorSurface::Silent,
        }
    }

    /// Feature 035 — short chip copy for kinds that drive a HUD notice chip.
    /// Two groups return non-empty copy: every [`ErrorSurface::HudNotice`]-tier
    /// kind, AND the actionable provider-key [`ErrorSurface::Notification`]
    /// kinds, which also flash a chip for immediate mid-dictation feedback
    /// (dogfood 2026-06-17 — a silent notification + focus-steal left the user
    /// with no visible reason for the window yank). The presenter (`route`)
    /// uses a non-empty chip on a Notification kind as the "show chip, skip
    /// focus-steal" signal. Every other tier returns `""` (never displayed).
    /// Kept terse so it fits the wipe-reveal chip above the HUD pill.
    pub fn hud_message(&self) -> &'static str {
        match self {
            MuniError::DeepgramConnectionFailed { .. }
            | MuniError::GladiaConnectionFailed { .. }
            | MuniError::DeepgramPartialRecovered => "Connection dropped",
            MuniError::DeepgramServerError { .. }
            | MuniError::GladiaServerError { .. }
            | MuniError::DeepgramInvalidResponse
            | MuniError::GladiaInvalidResponse => "Service error — try again",
            // Groq is the cleanup provider — ANY Groq failure (drop, 5xx, bad
            // response) means cleanup was skipped and the raw transcript
            // pasted. So all three read the same as a missing prompt, not a
            // misleading "connection/service" message about the dictation.
            MuniError::GroqConnectionFailed { .. }
            | MuniError::GroqServerError { .. }
            | MuniError::GroqInvalidResponse
            | MuniError::CleanupPromptMissing => "Pasted without cleanup",
            MuniError::TranscriptionUnavailable => "Couldn't transcribe — try again",
            MuniError::GladiaFinalizeTimeoutAfterAudio => "Partial result — check text",
            MuniError::NothingToPaste => "No speech detected",
            // Plan 039 task 37 — the paste keystroke couldn't be posted (CGEvent
            // creation failed). The paste-failure arm in `deliver_final` emits
            // this error WITHOUT persisting a history row, so the dictation is
            // not recoverable via the re-paste hotkey — "try again" means
            // re-dictate. Keep the copy a low-alarm nudge, not a scary failure.
            MuniError::PasteInjectionFailed => "Couldn't paste — try again",

            // Actionable provider-key errors (Notification-surface) ALSO flash
            // a chip: they fire mid-dictation, so an immediate "what just
            // failed" cue where the user is looking beats a silent banner +
            // window yank. The notification still carries the full actionable
            // copy + deep-link to Settings → API Keys.
            MuniError::DeepgramKeyRejected => "Deepgram key rejected — check Settings",
            MuniError::GladiaKeyRejected => "Gladia key rejected — check Settings",
            MuniError::GroqKeyRejected => "Groq key rejected — check Settings",
            MuniError::DeepgramMissingApiKey => "Add your Deepgram key in Settings",
            MuniError::GladiaMissingApiKey => "Add your Gladia key in Settings",
            MuniError::GroqMissingApiKey => "Add your Groq key in Settings",
            // A locked keychain can fire mid-dictation (reading the key for an
            // API call); flash an in-context cue rather than only a silent
            // banner + window yank.
            MuniError::KeychainUnavailable => "Keychain locked — unlock it",

            // Remaining non-chip tiers never display a chip; return empty so
            // the presenter (and tests) can assert it's unused.
            MuniError::InputMonitoringDenied
            | MuniError::HotkeyTapInstallFailed
            | MuniError::MicrophoneDenied
            | MuniError::NoInputDevice
            | MuniError::AudioStreamFailed { .. }
            | MuniError::AccessibilityDenied
            | MuniError::PlatformUnsupported
            | MuniError::EmptyApiKey
            | MuniError::KeychainWriteFailed { .. }
            | MuniError::CleanupPromptInvalid
            | MuniError::CleanupPromptWriteFailed { .. }
            | MuniError::UnknownSecretsService { .. }
            | MuniError::GeminiMissingApiKey
            | MuniError::GeminiConnectionFailed { .. }
            | MuniError::GeminiServerError { .. }
            | MuniError::GeminiInvalidResponse
            | MuniError::AudioLidLoadFailed { .. }
            | MuniError::AudioLidInferenceFailed { .. }
            | MuniError::VadLoadFailed { .. }
            | MuniError::VadInferenceFailed { .. }
            | MuniError::ParakeetSidecarUnavailable { .. }
            | MuniError::ParakeetTranscribeFailed { .. } => "",
        }
    }

    /// Feature 035 — the tone of the HUD notice chip. `Amber` flags a
    /// "words may be lost / double-check the text" warning; everything else
    /// is a `Neutral` FYI. Only meaningful for [`ErrorSurface::HudNotice`]
    /// kinds (other tiers never render a chip).
    pub fn hud_tone(&self) -> HudTone {
        match self {
            MuniError::DeepgramPartialRecovered
            | MuniError::TranscriptionUnavailable
            | MuniError::GladiaFinalizeTimeoutAfterAudio
            // Plan 039 task 37 — the paste didn't land, so flag it amber
            // ("something to double-check") rather than a neutral FYI.
            | MuniError::PasteInjectionFailed => HudTone::Amber,

            MuniError::InputMonitoringDenied
            | MuniError::HotkeyTapInstallFailed
            | MuniError::MicrophoneDenied
            | MuniError::NoInputDevice
            | MuniError::AudioStreamFailed { .. }
            | MuniError::AccessibilityDenied
            | MuniError::PlatformUnsupported
            | MuniError::DeepgramMissingApiKey
            | MuniError::GladiaMissingApiKey
            | MuniError::GroqMissingApiKey
            | MuniError::DeepgramKeyRejected
            | MuniError::GladiaKeyRejected
            | MuniError::GroqKeyRejected
            | MuniError::DeepgramConnectionFailed { .. }
            | MuniError::DeepgramServerError { .. }
            | MuniError::DeepgramInvalidResponse
            | MuniError::GladiaConnectionFailed { .. }
            | MuniError::GladiaServerError { .. }
            | MuniError::GladiaInvalidResponse
            | MuniError::GroqConnectionFailed { .. }
            | MuniError::GroqServerError { .. }
            | MuniError::GroqInvalidResponse
            | MuniError::CleanupPromptMissing
            | MuniError::NothingToPaste
            | MuniError::EmptyApiKey
            | MuniError::KeychainWriteFailed { .. }
            | MuniError::KeychainUnavailable
            | MuniError::CleanupPromptInvalid
            | MuniError::CleanupPromptWriteFailed { .. }
            | MuniError::UnknownSecretsService { .. }
            | MuniError::GeminiMissingApiKey
            | MuniError::GeminiConnectionFailed { .. }
            | MuniError::GeminiServerError { .. }
            | MuniError::GeminiInvalidResponse
            | MuniError::AudioLidLoadFailed { .. }
            | MuniError::AudioLidInferenceFailed { .. }
            | MuniError::VadLoadFailed { .. }
            | MuniError::VadInferenceFailed { .. }
            | MuniError::ParakeetSidecarUnavailable { .. }
            | MuniError::ParakeetTranscribeFailed { .. } => HudTone::Neutral,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_monitoring_denied_is_loud_and_routes_to_general_tab() {
        // Phase 11: routes to General → Permissions (the only place
        // with a working "Open System Settings" deep link).
        let err = MuniError::InputMonitoringDenied;
        assert_eq!(err.severity(), ErrorSeverity::Loud);
        assert_eq!(err.settings_tab(), Some(SettingsTab::General));
        assert!(!err.user_message().is_empty());
    }

    #[test]
    fn keychain_unavailable_is_loud_notification_unrouted_and_distinct_copy() {
        // Slice 10 task 29 — a locked/denied keychain must NOT read as a
        // missing key: Loud + silent Notification, no Settings tab (the fix is
        // a system-level unlock, not "add your key"), and the copy points at
        // the keychain, never at Settings → API Keys.
        let err = MuniError::KeychainUnavailable;
        assert_eq!(err.severity(), ErrorSeverity::Loud);
        assert_eq!(err.surface(), ErrorSurface::Notification);
        assert_eq!(err.settings_tab(), None);
        let msg = err.user_message();
        assert!(msg.to_lowercase().contains("keychain"), "copy: {msg}");
        assert!(
            !msg.contains("API Keys"),
            "must not tell the user to re-enter a key: {msg}"
        );
        // Serde tag matches the presenter's `kind_of`.
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(
            json.get("kind").and_then(|v| v.as_str()),
            Some("keychainUnavailable")
        );
    }

    #[test]
    fn paste_injection_failed_is_paste_specific_not_tap_install() {
        // Plan 039 task 37 — a failed paste keystroke must NOT masquerade as
        // the hotkey-listener install failure it used to be mis-mapped to.
        // Paste-specific copy, an amber mid-dictation HUD notice (text didn't
        // land), no Settings tab, and its own serde tag.
        let err = MuniError::PasteInjectionFailed;
        assert_eq!(err.severity(), ErrorSeverity::Loud);
        assert_eq!(err.surface(), ErrorSurface::HudNotice);
        assert_eq!(err.hud_tone(), HudTone::Amber);
        assert_eq!(err.settings_tab(), None);
        assert!(!err.hud_message().is_empty());
        let msg = err.user_message().to_lowercase();
        assert!(msg.contains("paste"), "copy must be paste-specific: {msg}");
        assert!(
            !msg.contains("listener") && !msg.contains("keyboard listener"),
            "must not read as the tap-install failure: {msg}"
        );
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(
            json.get("kind").and_then(|v| v.as_str()),
            Some("pasteInjectionFailed")
        );
    }

    #[test]
    fn errors_round_trip_through_serde_json() {
        let err = MuniError::HotkeyTapInstallFailed;
        let json = serde_json::to_string(&err).expect("serialize");
        // Externally tagged on `kind` so the frontend can switch on it.
        assert!(json.contains("\"kind\""));
        assert!(json.contains("hotkeyTapInstallFailed"));
    }

    #[test]
    fn audio_variants_serialize_with_camel_case_kind() {
        let denied = serde_json::to_string(&MuniError::MicrophoneDenied).unwrap();
        assert!(denied.contains("\"kind\":\"microphoneDenied\""));

        let none = serde_json::to_string(&MuniError::NoInputDevice).unwrap();
        assert!(none.contains("\"kind\":\"noInputDevice\""));

        let failed = serde_json::to_string(&MuniError::AudioStreamFailed {
            reason: "eek".into(),
        })
        .unwrap();
        assert!(failed.contains("\"kind\":\"audioStreamFailed\""));
        assert!(failed.contains("\"reason\":\"eek\""));
    }

    #[test]
    fn audio_errors_route_to_general_tab() {
        for err in [
            MuniError::MicrophoneDenied,
            MuniError::NoInputDevice,
            MuniError::AudioStreamFailed { reason: "x".into() },
        ] {
            assert_eq!(err.severity(), ErrorSeverity::Loud);
            assert_eq!(err.settings_tab(), Some(SettingsTab::General));
            assert!(!err.user_message().is_empty());
        }
    }

    #[test]
    fn deepgram_missing_key_is_loud_and_routes_to_api_keys_tab() {
        let err = MuniError::DeepgramMissingApiKey;
        assert_eq!(err.severity(), ErrorSeverity::Loud);
        assert_eq!(err.settings_tab(), Some(SettingsTab::ApiKeys));
        assert!(err.user_message().contains("Deepgram"));
    }

    #[test]
    fn deepgram_runtime_errors_are_quiet_and_unrouted() {
        let cases = [
            MuniError::DeepgramConnectionFailed {
                reason: "boom".into(),
            },
            MuniError::DeepgramInvalidResponse,
            MuniError::DeepgramServerError {
                message: "quota exhausted".into(),
            },
        ];
        for err in cases {
            assert_eq!(err.severity(), ErrorSeverity::Quiet);
            assert_eq!(err.settings_tab(), None);
            assert!(!err.user_message().is_empty());
        }
    }

    #[test]
    fn deepgram_variants_serialize_with_camel_case_kind() {
        let missing = serde_json::to_string(&MuniError::DeepgramMissingApiKey).unwrap();
        assert!(missing.contains("\"kind\":\"deepgramMissingApiKey\""));

        let server = serde_json::to_string(&MuniError::DeepgramServerError {
            message: "rate limited".into(),
        })
        .unwrap();
        assert!(server.contains("\"kind\":\"deepgramServerError\""));
        assert!(server.contains("\"message\":\"rate limited\""));
    }

    #[test]
    fn groq_missing_key_is_loud_and_routes_to_api_keys_tab() {
        let err = MuniError::GroqMissingApiKey;
        assert_eq!(err.severity(), ErrorSeverity::Loud);
        assert_eq!(err.settings_tab(), Some(SettingsTab::ApiKeys));
        assert!(err.user_message().contains("Groq"));
    }

    #[test]
    fn groq_runtime_errors_are_quiet_and_unrouted() {
        let cases = [
            MuniError::GroqConnectionFailed {
                reason: "timeout".into(),
            },
            MuniError::GroqInvalidResponse,
            MuniError::GroqServerError {
                status: 503,
                body: "service unavailable".into(),
            },
        ];
        for err in cases {
            assert_eq!(err.severity(), ErrorSeverity::Quiet);
            assert_eq!(err.settings_tab(), None);
            assert!(!err.user_message().is_empty());
        }
    }

    #[test]
    fn groq_server_error_user_message_omits_body_and_status() {
        // Feature 035 — user-facing copy must carry NO raw detail: neither
        // the response body nor the status code leak through user_message().
        let err = MuniError::GroqServerError {
            status: 429,
            body: "rate limited please retry later".into(),
        };
        let msg = err.user_message();
        assert!(!msg.contains("429"));
        assert!(!msg.contains("rate limited"));
    }

    #[test]
    fn groq_variants_serialize_with_camel_case_kind() {
        let missing = serde_json::to_string(&MuniError::GroqMissingApiKey).unwrap();
        assert!(missing.contains("\"kind\":\"groqMissingApiKey\""));

        let server = serde_json::to_string(&MuniError::GroqServerError {
            status: 503,
            body: "boom".into(),
        })
        .unwrap();
        assert!(server.contains("\"kind\":\"groqServerError\""));
        assert!(server.contains("\"status\":503"));
        assert!(server.contains("\"body\":\"boom\""));
    }

    #[test]
    fn gemini_missing_key_is_loud_and_routes_to_api_keys_tab() {
        let err = MuniError::GeminiMissingApiKey;
        assert_eq!(err.severity(), ErrorSeverity::Loud);
        assert_eq!(err.settings_tab(), Some(SettingsTab::ApiKeys));
        assert!(err.user_message().contains("Gemini"));
    }

    #[test]
    fn gemini_runtime_errors_are_quiet_and_unrouted() {
        let cases = [
            MuniError::GeminiConnectionFailed {
                reason: "timeout".into(),
            },
            MuniError::GeminiInvalidResponse,
            MuniError::GeminiServerError {
                status: 503,
                body: "service unavailable".into(),
            },
        ];
        for err in cases {
            assert_eq!(err.severity(), ErrorSeverity::Quiet);
            assert_eq!(err.settings_tab(), None);
            assert!(!err.user_message().is_empty());
        }
    }

    #[test]
    fn gemini_server_error_user_message_omits_body_and_status() {
        // Feature 035 — see groq_server_error_user_message_omits_body_and_status.
        let err = MuniError::GeminiServerError {
            status: 429,
            body: "rate limited please retry later".into(),
        };
        let msg = err.user_message();
        assert!(!msg.contains("429"));
        assert!(!msg.contains("rate limited"));
    }

    #[test]
    fn cleanup_prompt_missing_is_loud_and_routes_to_cleanup_tab() {
        let err = MuniError::CleanupPromptMissing;
        assert_eq!(err.severity(), ErrorSeverity::Loud);
        assert_eq!(err.settings_tab(), Some(SettingsTab::Cleanup));
        assert!(err.user_message().contains("Cleanup prompt"));
    }

    #[test]
    fn accessibility_denied_is_loud_and_routes_to_general_tab() {
        // Phase 11: re-routed from Hotkey to General so the loud-error
        // landing tab shows the Permissions card with the
        // Accessibility row + "Open System Settings" deep link.
        let err = MuniError::AccessibilityDenied;
        assert_eq!(err.severity(), ErrorSeverity::Loud);
        assert_eq!(err.settings_tab(), Some(SettingsTab::General));
        assert!(err.user_message().contains("Accessibility"));
    }

    #[test]
    fn nothing_to_paste_is_quiet_and_unrouted() {
        let err = MuniError::NothingToPaste;
        assert_eq!(err.severity(), ErrorSeverity::Quiet);
        assert_eq!(err.settings_tab(), None);
        assert!(err.user_message().contains("Nothing"));
    }

    #[test]
    fn platform_unsupported_is_loud_and_unrouted() {
        let err = MuniError::PlatformUnsupported;
        assert_eq!(err.severity(), ErrorSeverity::Loud);
        assert_eq!(err.settings_tab(), None);
        assert!(!err.user_message().is_empty());
    }

    #[test]
    fn gladia_missing_key_is_loud_and_routes_to_api_keys_tab() {
        let err = MuniError::GladiaMissingApiKey;
        assert_eq!(err.severity(), ErrorSeverity::Loud);
        assert_eq!(err.settings_tab(), Some(SettingsTab::ApiKeys));
        assert!(err.user_message().contains("Gladia"));
    }

    #[test]
    fn gladia_runtime_errors_are_quiet_and_unrouted() {
        let cases = [
            MuniError::GladiaConnectionFailed {
                reason: "timeout".into(),
            },
            MuniError::GladiaInvalidResponse,
            MuniError::GladiaServerError {
                message: "quota exhausted".into(),
            },
        ];
        for err in cases {
            assert_eq!(err.severity(), ErrorSeverity::Quiet);
            assert_eq!(err.settings_tab(), None);
            assert!(!err.user_message().is_empty());
        }
    }

    #[test]
    fn gladia_finalize_timeout_after_audio_is_loud_and_unrouted() {
        // Backlog 0019 — the variant that flags "Gladia ACKed audio
        // then went silent" needs to be Loud so the user gets a
        // system notification instead of a silent empty paste.
        // Settings tab stays None because there is nothing the
        // user can fix in Settings; they just need to redictate.
        let err = MuniError::GladiaFinalizeTimeoutAfterAudio;
        assert_eq!(err.severity(), ErrorSeverity::Loud);
        assert_eq!(err.settings_tab(), None);
        assert!(err.requires_user_action_now());
        assert!(err.user_message().contains("Gladia"));
        assert!(err.user_message().contains("Try again"));
    }

    #[test]
    fn gladia_variants_serialize_with_camel_case_kind() {
        let missing = serde_json::to_string(&MuniError::GladiaMissingApiKey).unwrap();
        assert!(missing.contains("\"kind\":\"gladiaMissingApiKey\""));

        let server = serde_json::to_string(&MuniError::GladiaServerError {
            message: "rate limited".into(),
        })
        .unwrap();
        assert!(server.contains("\"kind\":\"gladiaServerError\""));
        assert!(server.contains("\"message\":\"rate limited\""));
    }

    #[test]
    fn transcription_unavailable_is_loud_and_unrouted() {
        // Plan 012 — generic backend-agnostic surface fired when
        // both Gladia primary and Whisper fallback fail. Loud so
        // the user sees it; no settings tab because there's nothing
        // to fix in Settings (just the network / try again).
        let err = MuniError::TranscriptionUnavailable;
        assert_eq!(err.severity(), ErrorSeverity::Loud);
        assert_eq!(err.settings_tab(), None);
        assert!(err.requires_user_action_now());
        assert!(err.user_message().contains("Couldn't transcribe"));
    }

    #[test]
    fn transcription_unavailable_serialises_with_camel_case_kind() {
        let json = serde_json::to_string(&MuniError::TranscriptionUnavailable).unwrap();
        assert!(json.contains("\"kind\":\"transcriptionUnavailable\""));
    }

    #[test]
    fn audio_lid_load_failed_is_quiet_and_unrouted_and_non_blocking() {
        let err = MuniError::AudioLidLoadFailed {
            reason: "file missing".into(),
        };
        assert_eq!(err.severity(), ErrorSeverity::Quiet);
        assert_eq!(err.settings_tab(), None);
        assert!(!err.requires_user_action_now());
        // Feature 035 — user_message() must not leak the raw reason.
        assert!(!err.user_message().is_empty());
        assert!(!err.user_message().contains("file missing"));
    }

    #[test]
    fn audio_lid_inference_failed_is_quiet_and_unrouted_and_non_blocking() {
        let err = MuniError::AudioLidInferenceFailed {
            reason: "empty buffer".into(),
        };
        assert_eq!(err.severity(), ErrorSeverity::Quiet);
        assert_eq!(err.settings_tab(), None);
        assert!(!err.requires_user_action_now());
        assert!(!err.user_message().is_empty());
        assert!(!err.user_message().contains("empty buffer"));
    }

    #[test]
    fn audio_lid_variants_serialize_with_camel_case_kind() {
        let load = serde_json::to_string(&MuniError::AudioLidLoadFailed {
            reason: "boom".into(),
        })
        .unwrap();
        assert!(load.contains("\"kind\":\"audioLidLoadFailed\""));
        assert!(load.contains("\"reason\":\"boom\""));

        let infer = serde_json::to_string(&MuniError::AudioLidInferenceFailed {
            reason: "boom".into(),
        })
        .unwrap();
        assert!(infer.contains("\"kind\":\"audioLidInferenceFailed\""));
    }

    #[test]
    fn vad_load_failed_is_quiet_and_unrouted_and_non_blocking() {
        let err = MuniError::VadLoadFailed {
            reason: "ort init failed".into(),
        };
        assert_eq!(err.severity(), ErrorSeverity::Quiet);
        assert_eq!(err.settings_tab(), None);
        assert!(!err.requires_user_action_now());
        assert!(!err.user_message().is_empty());
        assert!(!err.user_message().contains("ort init failed"));
    }

    #[test]
    fn vad_inference_failed_is_quiet_and_unrouted_and_non_blocking() {
        let err = MuniError::VadInferenceFailed {
            reason: "mutex poisoned".into(),
        };
        assert_eq!(err.severity(), ErrorSeverity::Quiet);
        assert_eq!(err.settings_tab(), None);
        assert!(!err.requires_user_action_now());
        assert!(!err.user_message().is_empty());
        assert!(!err.user_message().contains("mutex poisoned"));
    }

    #[test]
    fn vad_variants_serialize_with_camel_case_kind() {
        let load = serde_json::to_string(&MuniError::VadLoadFailed {
            reason: "boom".into(),
        })
        .unwrap();
        assert!(load.contains("\"kind\":\"vadLoadFailed\""));
        assert!(load.contains("\"reason\":\"boom\""));

        let infer = serde_json::to_string(&MuniError::VadInferenceFailed {
            reason: "boom".into(),
        })
        .unwrap();
        assert!(infer.contains("\"kind\":\"vadInferenceFailed\""));
    }

    #[test]
    fn deepgram_partial_recovered_is_quiet_unrouted_non_blocking_and_camel_case() {
        // Feature 034 — success-with-warning: the recovered partial DID
        // paste, so this must be Quiet (amber pill), unrouted (nothing
        // to fix in Settings), and non-blocking (never steal focus).
        let err = MuniError::DeepgramPartialRecovered;
        assert_eq!(err.severity(), ErrorSeverity::Quiet);
        assert_eq!(err.settings_tab(), None);
        assert!(!err.requires_user_action_now());
        assert!(!err.user_message().is_empty());

        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("\"kind\":\"deepgramPartialRecovered\""));
    }

    #[test]
    fn injection_variants_serialize_with_camel_case_kind() {
        let denied = serde_json::to_string(&MuniError::AccessibilityDenied).unwrap();
        assert!(denied.contains("\"kind\":\"accessibilityDenied\""));

        let empty = serde_json::to_string(&MuniError::NothingToPaste).unwrap();
        assert!(empty.contains("\"kind\":\"nothingToPaste\""));

        let unsupported = serde_json::to_string(&MuniError::PlatformUnsupported).unwrap();
        assert!(unsupported.contains("\"kind\":\"platformUnsupported\""));
    }

    // ----- Feature 035 — surface routing + no-raw-detail + new key-rejected kinds -----

    #[test]
    fn surface_routes_a_representative_kind_of_each_tier() {
        // One witness per surface tier. If a future change re-routes any of
        // these, this is the canary — the four tiers are the whole point of
        // Feature 035, so each must keep a stable, named destination.
        assert_eq!(
            MuniError::DeepgramConnectionFailed {
                reason: "blip".into(),
            }
            .surface(),
            ErrorSurface::HudNotice,
        );
        assert_eq!(
            MuniError::DeepgramKeyRejected.surface(),
            ErrorSurface::Notification,
        );
        assert_eq!(MuniError::EmptyApiKey.surface(), ErrorSurface::Toast);
        assert_eq!(
            MuniError::ParakeetTranscribeFailed {
                reason: "crash".into(),
            }
            .surface(),
            ErrorSurface::Silent,
        );
        assert_eq!(MuniError::NothingToPaste.surface(), ErrorSurface::HudNotice);
    }

    #[test]
    fn deepgram_key_rejected_is_an_actionable_api_keys_notification() {
        // The triggering incident for Feature 035: a deactivated provider key
        // must surface as a silent, actionable notification that deep-links to
        // API Keys and (since no transcript lands) steals focus.
        let err = MuniError::DeepgramKeyRejected;
        assert_eq!(err.surface(), ErrorSurface::Notification);
        assert_eq!(err.settings_tab(), Some(SettingsTab::ApiKeys));
        assert_eq!(err.severity(), ErrorSeverity::Loud);
        assert!(err.requires_user_action_now());
    }

    #[test]
    fn key_rejected_kinds_classify_consistently() {
        // All three rejected-key kinds route to API Keys and are Loud. Groq is
        // the one non-blocking case (raw transcript still pastes), so it must
        // NOT steal focus; the ASR-key kinds must.
        for err in [MuniError::DeepgramKeyRejected, MuniError::GladiaKeyRejected] {
            assert_eq!(err.surface(), ErrorSurface::Notification);
            assert_eq!(err.settings_tab(), Some(SettingsTab::ApiKeys));
            assert_eq!(err.severity(), ErrorSeverity::Loud);
            assert!(err.requires_user_action_now(), "{err:?}");
        }
        let groq = MuniError::GroqKeyRejected;
        assert_eq!(groq.surface(), ErrorSurface::Notification);
        assert_eq!(groq.settings_tab(), Some(SettingsTab::ApiKeys));
        assert!(
            !groq.requires_user_action_now(),
            "Groq key rejection still pastes raw text — must not steal focus",
        );
    }

    #[test]
    fn key_rejected_variants_serialize_with_camel_case_kind() {
        let deepgram = serde_json::to_string(&MuniError::DeepgramKeyRejected).unwrap();
        assert!(deepgram.contains("\"kind\":\"deepgramKeyRejected\""));

        let gladia = serde_json::to_string(&MuniError::GladiaKeyRejected).unwrap();
        assert!(gladia.contains("\"kind\":\"gladiaKeyRejected\""));

        let groq = serde_json::to_string(&MuniError::GroqKeyRejected).unwrap();
        assert!(groq.contains("\"kind\":\"groqKeyRejected\""));
    }

    #[test]
    fn surface_enum_serializes_camel_case() {
        // The surface is serialized for any telemetry / IPC consumer; lock the
        // tag spelling so a frontend switch can't silently drift.
        assert_eq!(
            serde_json::to_string(&ErrorSurface::HudNotice).unwrap(),
            "\"hudNotice\"",
        );
        assert_eq!(
            serde_json::to_string(&ErrorSurface::Notification).unwrap(),
            "\"notification\"",
        );
    }

    #[test]
    fn user_message_never_leaks_raw_reason_status_or_body() {
        // Feature 035 — no user-facing string may interpolate the raw
        // {reason}/{status}/{body}/{service} fields. Feed unmistakable probe
        // tokens into every struct-shaped variant and assert none surface.
        const REASON: &str = "SECRET_PROBE_123";
        const STATUS: u16 = 499;
        const BODY: &str = "BODYPROBE";

        let cases = [
            MuniError::AudioStreamFailed {
                reason: REASON.into(),
            },
            MuniError::DeepgramConnectionFailed {
                reason: REASON.into(),
            },
            MuniError::DeepgramServerError {
                message: BODY.into(),
            },
            MuniError::GroqConnectionFailed {
                reason: REASON.into(),
            },
            MuniError::GroqServerError {
                status: STATUS,
                body: BODY.into(),
            },
            MuniError::GeminiConnectionFailed {
                reason: REASON.into(),
            },
            MuniError::GeminiServerError {
                status: STATUS,
                body: BODY.into(),
            },
            MuniError::GladiaConnectionFailed {
                reason: REASON.into(),
            },
            MuniError::GladiaServerError {
                message: BODY.into(),
            },
            MuniError::KeychainWriteFailed {
                reason: REASON.into(),
            },
            MuniError::CleanupPromptWriteFailed {
                reason: REASON.into(),
            },
            MuniError::UnknownSecretsService {
                service: REASON.into(),
            },
            MuniError::AudioLidLoadFailed {
                reason: REASON.into(),
            },
            MuniError::AudioLidInferenceFailed {
                reason: REASON.into(),
            },
            MuniError::VadLoadFailed {
                reason: REASON.into(),
            },
            MuniError::VadInferenceFailed {
                reason: REASON.into(),
            },
            MuniError::ParakeetSidecarUnavailable {
                reason: REASON.into(),
            },
            MuniError::ParakeetTranscribeFailed {
                reason: REASON.into(),
            },
        ];

        for err in cases {
            let msg = err.user_message();
            assert!(!msg.is_empty(), "{err:?} has empty user_message");
            assert!(!msg.contains(REASON), "{err:?} leaked reason/service probe");
            assert!(
                !msg.contains(&STATUS.to_string()),
                "{err:?} leaked status probe",
            );
            assert!(!msg.contains(BODY), "{err:?} leaked body probe");
        }
    }

    #[test]
    fn hud_message_is_non_empty_for_every_hud_notice_kind() {
        // Every HudNotice-tier kind drives a chip, so its hud_message() copy
        // must be non-empty; conversely the other tiers never render a chip,
        // so their hud_message() is deliberately "" (asserted below).
        let hud_notice_kinds = [
            MuniError::DeepgramConnectionFailed { reason: "x".into() },
            MuniError::DeepgramServerError {
                message: "x".into(),
            },
            MuniError::DeepgramInvalidResponse,
            MuniError::GladiaConnectionFailed { reason: "x".into() },
            MuniError::GladiaServerError {
                message: "x".into(),
            },
            MuniError::GladiaInvalidResponse,
            MuniError::DeepgramPartialRecovered,
            MuniError::GroqConnectionFailed { reason: "x".into() },
            MuniError::GroqServerError {
                status: 503,
                body: "x".into(),
            },
            MuniError::GroqInvalidResponse,
            MuniError::CleanupPromptMissing,
            MuniError::TranscriptionUnavailable,
            MuniError::GladiaFinalizeTimeoutAfterAudio,
            MuniError::NothingToPaste,
        ];
        for err in hud_notice_kinds {
            assert_eq!(
                err.surface(),
                ErrorSurface::HudNotice,
                "{err:?} drifted out of the HudNotice tier — update this test",
            );
            assert!(
                !err.hud_message().is_empty(),
                "{err:?} is HudNotice-tier but has empty hud_message()",
            );
        }
    }

    #[test]
    fn hud_message_is_empty_for_non_chip_kinds() {
        // Witnesses that never render a chip: a Toast kind and a Silent kind.
        // Their chip copy must stay "". NOTE: provider-key Notification kinds
        // (DeepgramKeyRejected etc.) DO carry chip copy now — see
        // `provider_key_notifications_also_drive_a_chip`.
        for err in [
            MuniError::EmptyApiKey,
            MuniError::ParakeetTranscribeFailed { reason: "x".into() },
        ] {
            assert_ne!(err.surface(), ErrorSurface::HudNotice);
            assert_eq!(err.hud_message(), "", "{err:?} should have no chip copy");
        }
    }

    #[test]
    fn provider_key_notifications_also_drive_a_chip() {
        // Dogfood 2026-06-17: actionable provider-key errors fire mid-dictation,
        // so they surface as a Notification (persistent, deep-links to Settings)
        // AND carry non-empty, neutral-toned chip copy. The presenter shows that
        // chip and skips the focus-steal. Guard both invariants here so neither
        // half can silently regress.
        for err in [
            MuniError::DeepgramKeyRejected,
            MuniError::GladiaKeyRejected,
            MuniError::GroqKeyRejected,
            MuniError::DeepgramMissingApiKey,
            MuniError::GladiaMissingApiKey,
            MuniError::GroqMissingApiKey,
        ] {
            assert_eq!(
                err.surface(),
                ErrorSurface::Notification,
                "{err:?} must stay a Notification (the chip is in addition, not instead)",
            );
            assert!(
                !err.hud_message().is_empty(),
                "{err:?} should carry chip copy for mid-dictation feedback",
            );
            assert_eq!(
                err.hud_tone(),
                HudTone::Neutral,
                "{err:?} chip should be neutral (amber is reserved for words-at-risk)",
            );
        }
    }

    #[test]
    fn hud_tone_is_amber_only_for_words_at_risk_kinds() {
        // Amber = "text may be lost / double-check it". Everything else is a
        // neutral FYI. Guard the three amber kinds and a neutral witness.
        assert_eq!(
            MuniError::DeepgramPartialRecovered.hud_tone(),
            HudTone::Amber,
        );
        assert_eq!(
            MuniError::TranscriptionUnavailable.hud_tone(),
            HudTone::Amber,
        );
        assert_eq!(
            MuniError::GladiaFinalizeTimeoutAfterAudio.hud_tone(),
            HudTone::Amber,
        );
        assert_eq!(
            MuniError::DeepgramConnectionFailed { reason: "x".into() }.hud_tone(),
            HudTone::Neutral,
        );
    }
}
