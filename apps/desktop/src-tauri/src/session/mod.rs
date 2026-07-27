//! Dictation session orchestrator.
//!
//! Mirrors Swift v1's `DictationSession` (`muni/Core/DictationSession.swift`):
//! one orchestrator drives the press → audio + Deepgram → release → finalize
//! → Groq cleanup → paste pipeline. Phase 17 of the Tauri pivot lifts the
//! orchestration that previously lived inline in `lib.rs::run()` into this
//! module so future phases (history, secrets-via-keychain, error presenter)
//! can extend the behavior without touching every call site.
//!
//! ## Deepgram WebSocket pre-warming
//!
//! `DeepgramClient::open()` does TLS + WebSocket handshake on every press,
//! costing 200–500 ms before any audio reaches the server. cpal starts
//! producing audio immediately, so the broadcast channel buffer fills and the
//! head of every utterance is dropped. The `DeepgramPool` keeps a warm WS
//! parked at all times so each press takes a ready socket and the next warmer
//! is scheduled the moment a press starts — eliminating the "talk-too-soon"
//! gap users perceive after pressing the hotkey. See plan §002 Task 17 and
//! the Phase 3 deferred-work paragraph.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::async_runtime::JoinHandle;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex as TokioMutex, Notify};

use crate::audio::{AudioCapture, TARGET_SAMPLE_RATE};
use crate::audio_lid::{AudioLidClassifier, AudioLidVerdict};
use crate::deepgram::{DeepgramClient, FinalizeOutcome};
use crate::error::MuniError;
use crate::error_presenter::PresentError;
use crate::gladia::GladiaClient;
use crate::groq::{self, GroqClient, GroqRequest};
use crate::groq_activity::GroqActivity;
use crate::groq_whisper::GroqWhisperClient;
use crate::history_store::{
    frontmost_app_bundle_id, HistoryStore, NewDictationRecord, SERVED_BY_DEEPGRAM,
    SERVED_BY_DEEPGRAM_PARTIAL, SERVED_BY_GLADIA_PRIMARY, SERVED_BY_GLADIA_RESCUE,
    SERVED_BY_PARAKEET_LOCAL, SERVED_BY_WHISPER_FALLBACK,
};
use crate::injection::{FocusProbe, PlatformInjector};
use crate::parakeet::ParakeetClient;
use crate::permissions::{self, MicrophoneStatus};
use crate::press_timing::PressTiming;
use crate::prompt::CleanupPrompt;
use crate::secrets;
use crate::text_lid::{LidLabel, TextLidClassifier};
use crate::usage_store::UsageStore;
use crate::usage_writer::{try_send_drop_oldest, UsageRecord};

// Plan 039 slice 25: the Deepgram pre-warming pool moved verbatim to
// `deepgram_pool.rs`. Re-exported here so `crate::session::DeepgramPool` (and
// the `muni_lib::session::…` integration-test path) resolve unchanged.
#[cfg(test)]
pub(crate) use crate::deepgram_pool::ParkedEntry;
pub use crate::deepgram_pool::{fixed_deepgram_key, DeepgramKeyProvider, DeepgramPool};

// Plan 039 slice 25: pipeline stages split into child modules. Each holds an
// `impl DictationSession` block reaching parent-private state via `use super::*`.
mod cleanup;
mod confidence_trigger;
mod lid_router;
pub use confidence_trigger::{
    load_confidence_trigger_config, load_confidence_trigger_drain_ms, ConfidenceTriggerConfig,
};

/// Tauri event carrying the raw Deepgram transcript before cleanup. Surfaced
/// to the debug overlay so devs can see the input the cleanup stage operates
/// on.
pub const EVENT_TRANSCRIPT_RAW: &str = "transcript://raw";

/// Tauri event carrying the cleaned-final transcript text once the press is
/// resolved (paste landed, was skipped because the result was empty, or fell
/// back to the raw transcript when cleanup failed).
pub const EVENT_TRANSCRIPT_FINAL: &str = "transcript://final";

/// Tauri event carrying a user-facing error message when the press could not
/// produce a transcript (missing key, connection failure, etc.).
pub const EVENT_TRANSCRIPT_ERROR: &str = "transcript://error";

/// Tauri event broadcasting the orchestrator's state. Payload is the
/// camelCase variant name of [`SessionState`] (e.g. `"listening"`). The
/// React layer drives the HUD fade and any "I'm working" UI from this.
pub const EVENT_SESSION_STATE_CHANGED: &str = "session://state-changed";

/// Tauri event fired AFTER a successful insert into the history store.
/// Distinct from [`EVENT_TRANSCRIPT_FINAL`] (which fires before the
/// SQLite write completes) so the History tab can refresh without
/// racing the writer. Payload is the empty string — listeners just
/// re-fetch via the `history_list` IPC command.
pub const EVENT_HISTORY_CHANGED: &str = "history://changed";

/// Env var the dev workflow reads for the Deepgram API key. Resolution
/// is centralised in `secrets::get` (env-var first, then OS keychain)
/// so a key saved through Settings → API Keys feeds the live session.
pub const DEEPGRAM_API_KEY_ENV: &str = "MUNI_DEEPGRAM_KEY";

/// Env var the dev workflow reads for the Groq API key. Same env-first
/// then keychain resolution as Deepgram.
pub const GROQ_API_KEY_ENV: &str = "MUNI_GROQ_KEY";

/// Number of milliseconds the forwarder continues draining audio after
/// release before considering the press finished.
///
/// cpal's audio thread keeps producing samples for one or two more callbacks
/// after `audio.stop()` is queued. Without this drain, the biased select
/// would re-enter, see the release flag set, and exit before emptying the
/// broadcast queue — the user would lose the tail of every utterance unless
/// they held the hotkey for an extra ~1 s past their last word. 80 ms covers
/// ~2 cpal callbacks at typical macOS buffer sizes (5–40 ms each) without
/// adding perceptible latency to the press-release cycle.
pub const POST_RELEASE_DRAIN_MS: u64 = 80;

/// Backoff schedule for warmer retries when Deepgram is unreachable.
pub(crate) const WARMER_BACKOFF_S: &[u64] = &[1, 2, 5, 10, 30];

/// Peak |i16| amplitude below which a press is treated as "the mic
/// delivered no audio." Calibrated against `i16::MAX = 32767`:
/// - Digital silence (mic muted by macOS after a TCC revoke): peak ~ 0.
/// - Built-in mic ambient room tone: peak typically > 200 (~ -44 dBFS).
/// - Quiet whispered speech: peak > 1000 (~ -30 dBFS).
///
/// 64 sits two orders of magnitude below ambient room tone — high
/// enough to ride out a single random spike, low enough that a real
/// voiced press never trips it.
const SILENCED_PEAK_THRESHOLD: i16 = 64;

/// Minimum press duration before silence detection is allowed to fire.
/// Short presses (release within ~150 ms) can legitimately produce
/// near-zero peak amplitude even with a healthy mic — cpal's first
/// callback may not have arrived. Below this floor we keep the legacy
/// "empty transcript → silent idle" path.
const MIN_PRESS_FOR_SILENCE_DETECTION: Duration = Duration::from_millis(500);

/// Peak ceiling for classifying a press as a *dead capture stream* —
/// the only signal that should flip the Permissions pill to "Stale".
///
/// Far stricter than [`SILENCED_PEAK_THRESHOLD`] (64) on purpose. That
/// threshold answers "should we skip Whisper?"; this one answers "is
/// the mic stream actually dead?" — a much higher bar. Per the
/// [`SILENCED_PEAK_THRESHOLD`] doc, a mid-session macOS TCC revoke
/// delivers digitally-zeroed buffers (peak ~ 0) while a live mic always
/// carries a nonzero noise floor (ambient room tone peaks > 200). 4
/// rides out a stray glitch LSB while staying ~50× below the whisper
/// threshold and ~500× below room tone, so no live-but-quiet or
/// speechless-but-audible press can trip it — those keep the mic honest
/// as "Granted".
const DEAD_STREAM_PEAK_THRESHOLD: i16 = 4;

/// Wall-clock unix seconds (UTC), saturating to 0 on the impossible
/// pre-1970 case. Mirrors `history_store::unix_seconds_now` so the
/// orchestrator's cost-tracking timestamps match its history rows.
fn unix_seconds_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Split a `TextLidClassifier::provider_label` (`"<provider>:<model>"`)
/// into the two halves so usage records can store provider and model
/// separately. Falls back to `("unknown", label)` for any value that
/// doesn't follow the convention — defensive only; every shipping
/// implementation matches the format.
fn split_lid_provider_label(label: &str) -> (&str, &str) {
    match label.split_once(':') {
        Some((p, m)) => (p, m),
        None => ("unknown", label),
    }
}

/// Feature 021 round-4 fix 2026-05-18 — append `padding_samples`
/// zero-valued int16 samples to the end of a slice. Used by the
/// audio-LID hybrid path to discourage Whisper's mid-word truncation
/// on short multilingual slices: the trailing silence signals to
/// Whisper that the utterance is complete, which (empirically)
/// reduces the rate of "truncated to leading English prefix"
/// transcripts from Whisper-large-v3 on 3 s Taglish audio.
///
/// Returns a freshly allocated `Vec<i16>` so the caller can pass
/// `&padded` directly to the transcribe path. Pure function — no
/// side effects, easy to unit-test.
pub(crate) fn pad_with_trailing_silence(samples: &[i16], padding_samples: usize) -> Vec<i16> {
    let mut padded = Vec::with_capacity(samples.len() + padding_samples);
    padded.extend_from_slice(samples);
    padded.resize(samples.len() + padding_samples, 0);
    padded
}

/// Feature 020 — pure function deriving the route a single audio-LID
/// window argues for, given the *verdict label* (not the raw top-1
/// ISO code — see dogfood bug 2026-05-18). The label already folds
/// the en/tl probability split into the routing decision:
///
/// - [`LidLabel::English`] → `Some(Deepgram)` (fast path).
/// - [`LidLabel::Tagalog`] / [`LidLabel::Taglish`] → `Some(Whisper)`
///   (multilingual). Both share the "non-English → Whisper" rule
///   from `.claude/learned/002_deepgram_alone_is_not_enough_for_taglish.md`.
/// - [`LidLabel::Other`] → `None` (the windowing layer interprets
///   this as "keep checking").
///
/// Reading the label rather than `top1_lang` fixes the dogfood
/// failure where a Taglish verdict (top1=en, p_tl ≥ 0.10) was
/// routed to Deepgram by the prior `top1_lang`-only rule and the
/// Tagalog content was dropped.
pub(crate) fn audio_lid_proposed_route(label: &LidLabel) -> Option<RouterDecision> {
    match label {
        LidLabel::English => Some(RouterDecision::Deepgram),
        LidLabel::Tagalog | LidLabel::Taglish => Some(RouterDecision::Whisper),
        LidLabel::Other(_) => None,
    }
}

/// Backlog 0052 / plan 039 task 20 — whether a mid-press hybrid
/// text-LID verdict should arm the symmetric drift veto.
///
/// Only an explicit [`LidLabel::English`] verdict arms it. An
/// [`LidLabel::Other`] verdict is treated *identically to non-English*
/// per the `LidLabel::Other` contract (see `text_lid.rs`: "the router
/// treats it identically to non-English"), so it must NOT suppress a
/// subsequent audio-LID drift override to Whisper — arming the veto on
/// `Other` would wrongly pin an ambiguous press to Deepgram. Tagalog /
/// Taglish take the override path and never reach this decision.
///
/// This mirrors the `text_lid.rs` invariant that `Other` never counts
/// as an English signal.
pub(crate) fn hybrid_verdict_arms_drift_veto(label: &LidLabel) -> bool {
    matches!(label, LidLabel::English)
}

/// Feature 020 — minimum `p_en` whisper-LID must assign to commit
/// Deepgram on the first window. Below this floor, an `English`-labelled
/// verdict is treated as too uncertain to commit; the windowing layer
/// returns [`AudioLidAction::KeepChecking`] and waits for the next
/// window instead.
///
/// Calibrated from feature 020 dogfood (2026-05-18). Real Taglish
/// presses whose first window saw weak English signal (`p_en` in
/// `[0.30, 0.50]`) were committing Deepgram and dropping the Tagalog
/// content. Lifting the floor to 0.50 turns those weak commits into
/// "keep checking", giving the second window (which typically carries
/// the actual Tagalog signal) a chance to commit Whisper instead.
///
/// Cost: a clean English press with `p_en` just under 0.50 in the
/// first window defers commit by 1 s. The next window almost always
/// shows higher confidence and commits Deepgram normally — so the
/// only real cost is a one-window latency budget on borderline
/// English starts.
const MIN_P_EN_TO_COMMIT_DEEPGRAM: f32 = 0.50;

/// Feature 020 — actions the windowing state machine emits after
/// looking at a single audio-LID verdict's label. Each variant is
/// independently observable so unit tests can assert the expected
/// action without driving side effects (Deepgram close, decision-cell
/// write).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioLidAction {
    /// Pre-commit, label was [`LidLabel::Other`] OR was English with
    /// `p_en` below [`MIN_P_EN_TO_COMMIT_DEEPGRAM`]. Wait for the next
    /// window without changing route or drift state.
    KeepChecking,
    /// First en/tl window — commit the corresponding route.
    Commit(RouterDecision),
    /// Post-commit, the new window agrees with the committed route OR
    /// proposes the opposite route in a direction that's been
    /// permanently disabled (Whisper-committed press seeing an
    /// English window). Reset the drift counter.
    ///
    /// The "permanently disabled" case is the post-feature-020-dogfood
    /// removal of the `whisper → deepgram` override: once a press has
    /// committed Whisper, subsequent English windows are no longer
    /// evidence to flip back. The Deepgram socket is already torn
    /// down at commit time, so a route flip back to Deepgram would
    /// finalize with an empty paste (silent failure observed twice in
    /// the 2026-05-18 dogfood corpus).
    Agree,
    /// Post-commit, the new window is [`LidLabel::Other`] (cough,
    /// silence, brief ambient noise). The routing layer treats this
    /// as an ambiguous data point — neither agreement nor
    /// disagreement — so the drift counter is preserved. Without
    /// this rule, a mid-press pause resets accumulated drift evidence
    /// and the override fails to fire on the Tagalog tail
    /// (dogfood 2026-05-18: scenario "so basically what happened
    /// was… sabi ko sa kanya na wag nalang tayong pumunta" lost the
    /// Tagalog half).
    IgnoreNoise,
    /// Post-commit disagreement that has not yet crossed the threshold.
    /// Bump the drift counter to `new_count`.
    IncrementDrift { new_count: usize },
    /// Post-Deepgram-commit disagreement that crossed the threshold —
    /// fire the `deepgram → whisper` override.
    ///
    /// Only this direction survives after feature 020 dogfood. The
    /// inverse (`whisper → deepgram`) was removed because the Whisper
    /// commit already aborts the forwarder and closes the Deepgram
    /// socket; flipping back left the press with no producer for
    /// either backend, paste landed empty.
    FireOverrideToWhisper,
}

/// Feature 020 — pure windowing decision: given the current committed
/// route, the current drift counter, the threshold, and the new
/// window's verdict label + `p_en`, return the action
/// [`apply_audio_lid_verdict`] should take.
///
/// Behaviour:
/// - Pre-commit (committed_route = None):
///   - `label = English` AND `p_en >= MIN_P_EN_TO_COMMIT_DEEPGRAM` →
///     [`AudioLidAction::Commit(Deepgram)`].
///   - `label = English` AND `p_en < MIN_P_EN_TO_COMMIT_DEEPGRAM` →
///     [`AudioLidAction::KeepChecking`] (low-confidence English; defer
///     to the next window).
///   - `label = Tagalog` / `Taglish` → [`AudioLidAction::Commit(Whisper)`]
///     (no confidence gate on the safe path).
///   - `label = Other` → [`AudioLidAction::KeepChecking`].
/// - Post-commit:
///   - `label = Other` → [`AudioLidAction::IgnoreNoise`] (preserve
///     drift counter so a mid-press pause doesn't erase accumulated
///     drift evidence).
///   - proposed route equals committed → [`AudioLidAction::Agree`].
///   - committed = Whisper, proposed = Deepgram → [`AudioLidAction::Agree`]
///     (override direction permanently disabled; window is ignored).
///   - committed = Deepgram, proposed = Whisper:
///     - new count < threshold → [`AudioLidAction::IncrementDrift`].
///     - new count ≥ threshold → [`AudioLidAction::FireOverrideToWhisper`].
/// - **Backlog 0052 symmetric veto**: when `hybrid_recent_english == true`
///   AND the would-be action is [`AudioLidAction::FireOverrideToWhisper`],
///   downgrade to [`AudioLidAction::Agree`] (drift counter resets to 0;
///   route stays on Deepgram). The veto NEVER blocks the
///   [`AudioLidAction::IncrementDrift`] action — only the final fire —
///   so a true late-press Tagalog code-switch can still cross the drift
///   threshold once the hybrid sees the Tagalog content and updates its
///   verdict (or fires the commit/override directly via the hybrid task).
///   Caller must AND this argument with the
///   [`MUNI_AUDIO_LID_HYBRID_VETO_DRIFT_ENV`] env knob before passing in.
pub(crate) fn audio_lid_decide_action(
    label: &LidLabel,
    p_en: f32,
    committed_route: Option<RouterDecision>,
    consecutive_drift: usize,
    drift_threshold: usize,
    hybrid_recent_english: bool,
) -> AudioLidAction {
    let proposed = audio_lid_proposed_route(label);
    match (committed_route, proposed) {
        (None, None) => AudioLidAction::KeepChecking,
        (None, Some(RouterDecision::Deepgram)) if p_en < MIN_P_EN_TO_COMMIT_DEEPGRAM => {
            AudioLidAction::KeepChecking
        }
        (None, Some(route)) => AudioLidAction::Commit(route),
        (Some(_), None) => AudioLidAction::IgnoreNoise,
        // Agreement: post-commit window proposes the same route. Reset
        // drift counter.
        (Some(RouterDecision::Deepgram), Some(RouterDecision::Deepgram))
        | (Some(RouterDecision::Whisper), Some(RouterDecision::Whisper)) => AudioLidAction::Agree,
        // Permanently disabled override direction: a Whisper-committed
        // press seeing English windows can no longer flip back to
        // Deepgram. Treat as a no-op so the drift counter doesn't
        // accumulate evidence that can never fire.
        (Some(RouterDecision::Whisper), Some(RouterDecision::Deepgram)) => AudioLidAction::Agree,
        (Some(RouterDecision::Deepgram), Some(RouterDecision::Whisper)) => {
            let new_count = consecutive_drift + 1;
            if new_count >= drift_threshold {
                // Backlog 0052 — symmetric hybrid veto. When the parallel
                // hybrid text-LID has classified a recent slice of this
                // press as English (plan 039 task 20 — English only; an
                // `Other` verdict no longer arms the bit), block the fire
                // and treat this window as agreement instead. The drift counter
                // resets to 0; the route stays on Deepgram. See
                // `docs/findings/006_feat_027_post_implementation_dogfood.md`
                // observation #4 for the motivating evidence (Press 7/8/9,
                // all with hybrid English verdicts in hand ~400 ms before
                // the drift fire).
                if hybrid_recent_english {
                    AudioLidAction::Agree
                } else {
                    AudioLidAction::FireOverrideToWhisper
                }
            } else {
                AudioLidAction::IncrementDrift { new_count }
            }
        }
    }
}

/// Backlog 0048 — action emitted by [`audio_lid_decide_release_action`]
/// when the audio-LID press loop's `released(&mut release_rx)` arm
/// fires. Captures the at-release consumption of partial drift
/// evidence: if the press ended while `drift >= release_fire_floor` AND
/// the route was Deepgram-committed, fire the `deepgram → whisper`
/// override anyway (the mid-press threshold
/// [`DEFAULT_AUDIO_LID_DRIFT_CONSECUTIVE`] never got a chance to land
/// its second consecutive disagreement).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioLidReleaseAction {
    /// Press ended in a state where no at-release override is warranted —
    /// either drift was zero, the route wasn't Deepgram, or the
    /// fire-floor wasn't crossed.
    NoOp,
    /// Press ended with partial drift evidence on a Deepgram-committed
    /// route. Fire the `deepgram → whisper` override.
    FireOverrideToWhisper,
}

/// Backlog 0048 — pure release-time decision. The orchestrator's
/// `finalize_auto_detect` (and the LID task's release-arm as a
/// defensive double-fire) call this with the final committed route,
/// drift counter, and the "last post-commit verdict was Other" bit to
/// decide whether to fire the stale-drift override.
///
/// Returns [`AudioLidReleaseAction::FireOverrideToWhisper`] iff
/// `committed_route == Some(RouterDecision::Deepgram)` AND EITHER:
/// - **v1 rule:** `consecutive_drift >= release_fire_floor` — partial
///   drift evidence crosses the at-release floor (default 1). Catches
///   the late-Tagalog code-switch case where the second consecutive
///   disagreement never lands before release.
/// - **v2 rule (active when `treat_other_as_taglish == true`):**
///   `last_post_commit_was_other == true` — the most recent classified
///   post-commit verdict landed `Other(_)` (whisper-tiny top1 ∉
///   {en, tl}). Catches the gotcha #4 case in backlog 0048 where
///   whisper-tiny hallucinates `id`/`ru`/`es` on a Tagalog tail and
///   the IgnoreNoise action never increments the drift counter.
///
/// Returns [`AudioLidReleaseAction::NoOp`] otherwise.
///
/// Independence from mid-press calibration: this function does NOT
/// read [`DEFAULT_AUDIO_LID_DRIFT_CONSECUTIVE`]. The mid-press
/// threshold controls how aggressively the override fires *during*
/// the press; the release-time rules above are orthogonal axes.
/// Backlog 0047's false-positive trade-off lives on the mid-press
/// axis; this function is on the release-time axis.
///
/// **Backlog 0052 symmetric veto** (applies to both v1 and v2 rules):
/// when `hybrid_recent_english == true`, return [`AudioLidReleaseAction::NoOp`]
/// regardless of drift counter or last-was-Other state. The hybrid
/// text-LID's content-aware classification overrides whisper-tiny's
/// probability-vector evidence in the at-release window. Caller must
/// AND this argument with the [`MUNI_AUDIO_LID_HYBRID_VETO_DRIFT_ENV`]
/// env knob before passing in.
pub(crate) fn audio_lid_decide_release_action(
    committed_route: Option<RouterDecision>,
    consecutive_drift: usize,
    release_fire_floor: usize,
    last_post_commit_was_other: bool,
    treat_other_as_taglish: bool,
    hybrid_recent_english: bool,
) -> AudioLidReleaseAction {
    if !matches!(committed_route, Some(RouterDecision::Deepgram)) {
        return AudioLidReleaseAction::NoOp;
    }
    // Backlog 0052 — symmetric hybrid veto. If the parallel hybrid
    // text-LID classified a recent slice of this press as English
    // (or Other), the at-release stale-drift override is blocked
    // entirely — the route stays on Deepgram. This applies to BOTH
    // the v1 drift-counter rule and the v2 last-was-Other rule
    // (feat/026's at-release axis). The cross-axis fix is explicit
    // here so a future feat/026 follow-up doesn't accidentally
    // re-enable the fire.
    if hybrid_recent_english {
        return AudioLidReleaseAction::NoOp;
    }
    if consecutive_drift >= release_fire_floor {
        return AudioLidReleaseAction::FireOverrideToWhisper;
    }
    if treat_other_as_taglish && last_post_commit_was_other {
        return AudioLidReleaseAction::FireOverrideToWhisper;
    }
    AudioLidReleaseAction::NoOp
}

/// Feature 025 (backlog 0046) — pure predicate for the audio-LID
/// windowing loop's per-window VAD gate. Extracted from
/// [`DictationSession::run_audio_lid_pass`] so the gate's branching
/// logic is unit-testable without standing up broadcast channels,
/// Notifies, and Silero. Mirrors the [`audio_lid_decide_action`]
/// pattern.
///
/// Returns `true` when the candidate window should be classified;
/// `false` when it should be skipped or the buffer is not yet ready.
///
/// Invariants enforced here (brainstorm 007 Q3 + Q7):
/// - The **first** window is never gated — protects route-commit
///   latency. A skipped window does NOT count as a "first window done"
///   — the caller must only flip its `first_window_done` flag on a
///   real classify.
/// - When the gate is disabled (`gate_active == false`), the predicate
///   degenerates to today's behavior: fire iff buffer has enough
///   samples and the advance cadence has elapsed.
/// - When the gate is enabled, the predicate adds one clause: skip
///   iff the candidate window's last [`AUDIO_LID_WINDOW_SAMPLES`]
///   samples contained zero speech frames (i.e.
///   `samples_since_last_speech >= AUDIO_LID_WINDOW_SAMPLES`).
pub(crate) fn should_fire_audio_lid_window(
    first_window_done: bool,
    accumulated_since_last_window: usize,
    rolling_len: usize,
    samples_since_last_speech: usize,
    gate_active: bool,
) -> bool {
    // Buffer-readiness check — identical to the inline predicate at
    // `run_audio_lid_pass`'s window-ready site (pre-feat/025).
    let buffer_ready = if !first_window_done {
        rolling_len >= AUDIO_LID_WINDOW_SAMPLES
    } else {
        accumulated_since_last_window >= AUDIO_LID_WINDOW_ADVANCE_SAMPLES
            && rolling_len >= AUDIO_LID_WINDOW_SAMPLES
    };
    if !buffer_ready {
        return false;
    }
    // First-window protection: never gate the first classify, even if
    // the gate is active and the entire pre-classify span was silent.
    // The first verdict commits the route — skipping it would defer
    // routing past press-end on a silent start.
    if !first_window_done {
        return true;
    }
    // Gate-off path: behavior bit-identical to pre-feat/025.
    if !gate_active {
        return true;
    }
    // Gate-on path: skip iff the *entire* candidate window's worth of
    // most-recent samples was silence. Strict `<` (not `<=`) so that
    // `samples_since_last_speech == AUDIO_LID_WINDOW_SAMPLES` causes a
    // skip — that's the "all silent" boundary by definition.
    samples_since_last_speech < AUDIO_LID_WINDOW_SAMPLES
}

/// Feature 025 (backlog 0046) — emit the per-press audio-LID windowing
/// summary log line. Invoked by [`AudioLidPressSummaryGuard::drop`] so
/// the summary fires uniformly across every termination path — including
/// when [`DictationSession::finalize_auto_detect`] aborts the LID task
/// via `lid_handle.abort()`. Tokio's `abort()` cancels the task at its
/// next `.await` point and drops all in-scope locals, so an explicit
/// in-function call at each `return`/`break` would miss the (common)
/// happy-path abort. RAII guard pattern guarantees one summary per
/// press regardless of how the task ends.
fn log_audio_lid_press_summary(
    windows_classified: usize,
    windows_skipped: usize,
    gate_active: bool,
) {
    let total = windows_classified + windows_skipped;
    let ratio = if total > 0 {
        windows_skipped as f32 / total as f32
    } else {
        0.0
    };
    log::info!(
        target: "lid",
        "audio-LID: windows classified={} skipped={} (gate={}, vad_silent_ratio={:.2})",
        windows_classified,
        windows_skipped,
        if gate_active { "on" } else { "off" },
        ratio
    );
}

/// Feature 025 (backlog 0046) — RAII guard for per-press audio-LID
/// counters. The summary log fires from `Drop` so it survives external
/// task cancellation by [`DictationSession::finalize_auto_detect`]
/// (`lid_handle.abort()` at session.rs:5495). Without this guard, the
/// orchestrator's happy-path abort would race ahead of any in-function
/// `return` and silence the per-press summary on every committed
/// press — exactly the dogfood-blocking failure surfaced on 2026-05-21.
struct AudioLidPressSummaryGuard {
    windows_classified: usize,
    windows_skipped: usize,
    gate_active: bool,
}

impl AudioLidPressSummaryGuard {
    fn new(gate_active: bool) -> Self {
        Self {
            windows_classified: 0,
            windows_skipped: 0,
            gate_active,
        }
    }

    fn record_classified(&mut self) {
        self.windows_classified += 1;
    }

    fn record_skipped(&mut self) {
        self.windows_skipped += 1;
    }
}

impl Drop for AudioLidPressSummaryGuard {
    fn drop(&mut self) {
        log_audio_lid_press_summary(
            self.windows_classified,
            self.windows_skipped,
            self.gate_active,
        );
    }
}

/// Feature 021 — pure decision function: should the orchestrator
/// spawn the Gemini text-LID hybrid task for this press, given
/// audio-LID's first-window verdict?
///
/// Selective-trigger rule:
/// - `label = English` AND `p_en >= skip_threshold` → `false`
///   (audio-LID is confident; Gemini adds no value).
/// - `label = Tagalog | Taglish` → `false` (already routing to
///   Whisper; Gemini agreement would be a no-op).
/// - everything else (`Other`, weak `English`) → `true` (audio-LID
///   is uncertain — Gemini's accuracy may help).
///
/// Long-press late-Tagalog recovery is gated separately by the
/// caller (not by this function) since that signal requires
/// observing accumulated audio duration, not first-window verdict.
pub(crate) fn should_spawn_audio_hybrid(
    first_window_label: &LidLabel,
    first_window_p_en: f32,
    skip_threshold: f32,
) -> bool {
    match first_window_label {
        LidLabel::English if first_window_p_en >= skip_threshold => false,
        LidLabel::Tagalog | LidLabel::Taglish => false,
        _ => true,
    }
}

/// Feature 020 — read [`MUNI_AUDIO_LID_DRIFT_CONSECUTIVE_ENV`] once at
/// task spawn. Falls back to [`DEFAULT_AUDIO_LID_DRIFT_CONSECUTIVE`]
/// for unset / unparseable / zero values (a zero threshold would fire
/// the override on every single disagreeing window, defeating the
/// purpose of the "consecutive" guard).
fn load_audio_lid_drift_consecutive() -> usize {
    match std::env::var(MUNI_AUDIO_LID_DRIFT_CONSECUTIVE_ENV) {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(n) if n >= 1 => n,
            Ok(_) => {
                log::warn!(
                    target: "lid",
                    "{MUNI_AUDIO_LID_DRIFT_CONSECUTIVE_ENV}=0 disallowed — using default {DEFAULT_AUDIO_LID_DRIFT_CONSECUTIVE}"
                );
                DEFAULT_AUDIO_LID_DRIFT_CONSECUTIVE
            }
            Err(_) => {
                log::warn!(
                    target: "lid",
                    "{MUNI_AUDIO_LID_DRIFT_CONSECUTIVE_ENV}={raw:?} not parseable — using default {DEFAULT_AUDIO_LID_DRIFT_CONSECUTIVE}"
                );
                DEFAULT_AUDIO_LID_DRIFT_CONSECUTIVE
            }
        },
        Err(_) => DEFAULT_AUDIO_LID_DRIFT_CONSECUTIVE,
    }
}

/// Backlog 0048 v2 — read [`MUNI_AUDIO_LID_RELEASE_OTHER_AS_TAGLISH_ENV`]
/// once at session construction. Parses `on`/`off`/`true`/`false`/`1`/`0`
/// case-insensitively. Unset / unparseable values fall back to the
/// default (currently `true` — see [`DEFAULT_AUDIO_LID_RELEASE_OTHER_AS_TAGLISH`]).
fn load_audio_lid_release_other_as_taglish() -> bool {
    match std::env::var(MUNI_AUDIO_LID_RELEASE_OTHER_AS_TAGLISH_ENV) {
        Ok(raw) => match raw.trim().to_lowercase().as_str() {
            "on" | "true" | "1" | "yes" => true,
            "off" | "false" | "0" | "no" => false,
            other => {
                log::warn!(
                    target: "lid",
                    "{MUNI_AUDIO_LID_RELEASE_OTHER_AS_TAGLISH_ENV}={other:?} not parseable — using default {DEFAULT_AUDIO_LID_RELEASE_OTHER_AS_TAGLISH}"
                );
                DEFAULT_AUDIO_LID_RELEASE_OTHER_AS_TAGLISH
            }
        },
        Err(_) => DEFAULT_AUDIO_LID_RELEASE_OTHER_AS_TAGLISH,
    }
}

/// Backlog 0052 — read [`MUNI_AUDIO_LID_HYBRID_VETO_DRIFT_ENV`] once
/// at session construction. Parses `on`/`off`/`true`/`false`/`1`/`0`
/// case-insensitively. Unset / unparseable values fall back to
/// [`DEFAULT_AUDIO_LID_HYBRID_VETO_DRIFT`].
fn load_audio_lid_hybrid_veto_drift() -> bool {
    match std::env::var(MUNI_AUDIO_LID_HYBRID_VETO_DRIFT_ENV) {
        Ok(raw) => match raw.trim().to_lowercase().as_str() {
            "on" | "true" | "1" | "yes" => true,
            "off" | "false" | "0" | "no" => false,
            other => {
                log::warn!(
                    target: "lid",
                    "{MUNI_AUDIO_LID_HYBRID_VETO_DRIFT_ENV}={other:?} not parseable — using default {DEFAULT_AUDIO_LID_HYBRID_VETO_DRIFT}"
                );
                DEFAULT_AUDIO_LID_HYBRID_VETO_DRIFT
            }
        },
        Err(_) => DEFAULT_AUDIO_LID_HYBRID_VETO_DRIFT,
    }
}

/// Backlog 0048 — read [`MUNI_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR_ENV`]
/// once at task spawn. Falls back to
/// [`DEFAULT_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR`] for unset / unparseable /
/// zero values (a zero floor would disable the stale-drift override on
/// every release even when drift evidence exists, which is the wrong
/// semantic — a user who wants to disable the feature should set the
/// env var to a very large value, not zero).
fn load_audio_lid_release_drift_fire_floor() -> usize {
    match std::env::var(MUNI_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR_ENV) {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(n) if n >= 1 => n,
            Ok(_) => {
                log::warn!(
                    target: "lid",
                    "{MUNI_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR_ENV}=0 disallowed — using default {DEFAULT_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR}"
                );
                DEFAULT_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR
            }
            Err(_) => {
                log::warn!(
                    target: "lid",
                    "{MUNI_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR_ENV}={raw:?} not parseable — using default {DEFAULT_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR}"
                );
                DEFAULT_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR
            }
        },
        Err(_) => DEFAULT_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR,
    }
}

/// True when a press's peak amplitude sits at or below
/// [`SILENCED_PEAK_THRESHOLD`] and the press lasted at least
/// [`MIN_PRESS_FOR_SILENCE_DETECTION`] — i.e. the mic delivered
/// no audible audio for a long enough window to be intentional.
///
/// Pulled out of [`DictationSession::handle_hotkey_released`] so the
/// rule is unit-testable without standing up Deepgram + cpal mocks.
/// Composes with [`is_noise_only_transcript`] at the release path and
/// stands alone at the pre-Whisper / pre-slice amplitude gates added
/// for feat/022.
pub(crate) fn is_silent_press(peak_amplitude: i16, press_duration: Duration) -> bool {
    press_duration >= MIN_PRESS_FOR_SILENCE_DETECTION
        && peak_amplitude.unsigned_abs() <= SILENCED_PEAK_THRESHOLD as u16
}

/// True when a press captured *digital silence* — a peak at or below
/// [`DEAD_STREAM_PEAK_THRESHOLD`] over a long-enough press — the
/// signature of a dead capture stream (macOS zeroing buffers after a
/// mid-session TCC revoke while AVFoundation's per-process cache still
/// reports `Authorized`).
///
/// This is the ONLY press shape allowed to mark the mic "Stale". It is
/// deliberately far stricter than [`is_silent_press`]: a quiet or
/// speechless-but-audible press (peak nonzero, i.e. the mic is plainly
/// alive) must never flip the pill, because that is the overwhelmingly
/// common case — a user tapping the hotkey without speaking. Gating the
/// stale mark on a live-mic signal is exactly the false positive that
/// pinned the pill to "Stale" for whole sessions.
pub(crate) fn is_dead_capture_stream(peak_amplitude: i16, press_duration: Duration) -> bool {
    press_duration >= MIN_PRESS_FOR_SILENCE_DETECTION
        && peak_amplitude.unsigned_abs() <= DEAD_STREAM_PEAK_THRESHOLD as u16
}

/// True when a press was served by the recovered-partial path (plan 034).
/// Derives the "possibly-truncated" flag from the already-threaded
/// `served_by` tag rather than a parallel bool, keeping a single source of
/// truth for how the row was served.
fn served_by_is_partial(served_by: &str) -> bool {
    served_by == SERVED_BY_DEEPGRAM_PARTIAL
}

/// True when a trimmed transcript carries no alphanumeric content —
/// i.e. punctuation, whitespace, or empty. Catches Whisper
/// hallucinations on silent presses that crept just above the peak
/// threshold (e.g. `.`, `-`, `...`) without filtering legitimate
/// content. Unicode letters (Japanese, etc.) count as alphanumeric
/// and fall through to cleanup — that's a separate concern.
pub(crate) fn is_noise_only_transcript(trimmed: &str) -> bool {
    !trimmed.chars().any(|c| c.is_alphanumeric())
}

/// feat/022 — true when a hybrid slice's peak `|i16|` amplitude sits
/// at or below [`SILENCED_PEAK_THRESHOLD`], i.e. the slice carries no
/// audible audio worth classifying. Pulled into a pure helper so the
/// gate body inside [`DictationSession::spawn_audio_hybrid_inner_classify`]
/// is unit-testable without standing up Whisper + Deepgram mocks.
///
/// An empty slice returns true (no audio = silent) — the hybrid
/// caller guarantees `slice.len() == AUDIO_HYBRID_SLICE_SAMPLES > 0`
/// in production, so this branch is defensive.
pub(crate) fn is_silent_slice(slice: &[i16]) -> bool {
    let peak: u16 = slice.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    peak <= SILENCED_PEAK_THRESHOLD as u16
}

/// True when an audio buffer is too short for Groq's Whisper
/// `/audio/transcriptions` endpoint, which rejects buffers below
/// 0.01 seconds with HTTP 400 (`"Audio file is too short. Minimum
/// audio length is 0.01 seconds."`).
///
/// At [`crate::groq_whisper::PCM_SAMPLE_RATE`] (16 kHz) the boundary
/// is 160 samples. In practice the trigger is a sub-50 ms accidental
/// hotkey graze where cpal never delivered its first callback, so the
/// buffer is exactly zero samples — but the `< 160` predicate also
/// catches the (rare) case where one partial callback landed.
///
/// Pulled into a pure helper so the gate body inside
/// [`DictationSession::finalize_auto_detect`]'s Whisper branch is
/// unit-testable without standing up an `AutoDetectActive` scaffold.
/// See backlog 0041.
pub(crate) fn audio_too_short_for_groq_whisper(samples: &[i16]) -> bool {
    const GROQ_WHISPER_MIN_SAMPLES: usize = (crate::groq_whisper::PCM_SAMPLE_RATE as usize) / 100;
    samples.len() < GROQ_WHISPER_MIN_SAMPLES
}

/// Feature 023 (backlog 0040) — known Whisper hallucination phrases.
/// The Whisper-large family confidently emits these on near-silent
/// audio (training-data outro bias: YouTube farewells, Japanese
/// fillers). They survive feat/022's amplitude gates AND the new VAD
/// gate when VAD says "speech" on borderline ambient buffers. Caught
/// at the post-Whisper widen in [`DictationSession::handle_hotkey_released`]
/// and at the post-slice-transcribe site in
/// [`DictationSession::spawn_audio_hybrid_inner_classify`]. Entries are
/// matched after normalization via [`normalize_for_hallucination_match`]
/// — see brainstorm 005 § Decision 7c for the source list.
const KNOWN_HALLUCINATIONS: &[&str] = &[
    "thank you",
    "thanks for watching",
    "thank you for watching",
    "thanks",
    "bye",
    "goodbye",
    "you",
    "はい",
    "ありがとうございました",
];

/// Normalize a transcript for hallucination-allowlist comparison.
/// Lowercase, trim leading/trailing ASCII punctuation and whitespace,
/// then collapse internal whitespace. Returns an owned `String` so
/// the caller can compare against the const allowlist directly.
pub(crate) fn normalize_for_hallucination_match(s: &str) -> String {
    s.trim()
        .trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns `true` when `text` matches a known Whisper hallucination
/// phrase exactly (after normalization). Composes with
/// [`is_noise_only_transcript`] (which catches punct-only / empty) at
/// the same gate sites. Brainstorm 005 § Decision 7a fixed the policy
/// to **exact match** — substring would gate real dictations that
/// contain "thank you" mid-sentence (correctness regression).
/// Feature 024 (backlog 0042) Site E — resolve the buffer to hand to
/// Groq Whisper batch (and to Gladia on the cross-provider fallback) at
/// release time. Two paths:
///
/// - **Mirror path** — when the audio-LID hybrid task ran with Site D
///   enabled, the `mirror` argument points at the speech-only
///   accumulator the hybrid populated. If it's non-empty, hand that to
///   Whisper.
/// - **One-shot fallback** — when the hybrid never armed (Tagalog-
///   leading committed-fast presses, Gladia recovery path) OR the
///   mirror is empty, construct a fresh [`crate::vad::StreamingVadDetector`]
///   on the spot and `extract_speech` over the original buffer.
///
/// When the kill switch ([`crate::lib::MUNI_VAD_TRIM_RELEASE_BUFFER_ENV`])
/// is off (the default on first ship), the function returns the original
/// buffer unchanged. When the factory is `None` (both streaming-VAD kill
/// switches off at boot), same.
///
/// The lock on `mirror` is held only across a small clone — never
/// across the Whisper call.
pub(crate) async fn resolve_trimmed_release_buffer(
    deps: &SessionDeps,
    mirror: Option<&Arc<TokioMutex<Vec<i16>>>>,
    original: &[i16],
) -> Vec<i16> {
    let Some(factory) = deps.streaming_vad_factory.as_ref() else {
        return original.to_vec();
    };
    if !crate::resolve_vad_trim_release_buffer_enabled() {
        // Hybrid kill switch may be on while the trim kill switch is
        // off — in that case the factory exists but Site E remains a
        // no-op.
        return original.to_vec();
    }
    if let Some(mirror_arc) = mirror {
        let mirror_clone = {
            let guard = mirror_arc.lock().await;
            if guard.is_empty() {
                None
            } else {
                Some(guard.clone())
            }
        };
        if let Some(samples) = mirror_clone {
            let denom = original.len().max(1) as f64;
            log::info!(
                target: "asr",
                "release-path trim: using hybrid speech mirror ({} → {} samples, {:.1}% retained)",
                original.len(),
                samples.len(),
                100.0 * samples.len() as f64 / denom
            );
            return samples;
        }
    }
    let mut detector = (factory)();
    let trimmed = detector.extract_speech(original).await;
    let denom = original.len().max(1) as f64;
    log::info!(
        target: "asr",
        "release-path trim: one-shot streaming VAD ({} → {} samples, {:.1}% retained)",
        original.len(),
        trimmed.len(),
        100.0 * trimmed.len() as f64 / denom
    );
    trimmed
}

pub(crate) fn matches_known_hallucination(text: &str) -> bool {
    let normalized = normalize_for_hallucination_match(text);
    if normalized.is_empty() {
        // Punct-only / whitespace-only delegate to is_noise_only_transcript;
        // returning false here keeps gate-fire log lines unambiguous
        // (no double-counting in metrics or logs).
        return false;
    }
    KNOWN_HALLUCINATIONS
        .iter()
        .any(|entry| normalize_for_hallucination_match(entry) == normalized)
}

/// Cross-process flag that records "the AVFoundation cache is lying
/// about microphone authorisation this session." The flag is set only
/// when silence detection fires AND AVFoundation insists the mic is
/// authorized — the specific failure mode the Permissions card's
/// red "Stale — restart Muni" pill is meant to surface.
///
/// Why gated on `Authorized`: a fresh-launch process where the user
/// has never granted (AV reports `denied`/`notDetermined`) also
/// produces silenced presses, but the AV cache there is *honest*. The
/// standard "Denied" pill + Open System Settings button is the right
/// signal in that case; flagging it as "stale" would confuse a user
/// who hasn't toggled anything in-session. See QA repro:
/// fresh-relaunch-after-disable showed the wrong "Stale" pill before
/// this gate.
///
/// Self-healing: the flag clears the moment a press delivers real,
/// audible content (see [`MicSilencedFlag::clear_silenced`]). The
/// silence heuristic that sets it (peak ≤ threshold, or VAD no-speech)
/// has benign false positives — a hold where the user simply didn't
/// speak, or spoke below the threshold — and without a clear path a
/// single such press pinned the pill to "Stale" for the whole session
/// even while dictation kept working. An audible press is positive
/// proof the AV cache is *not* lying, so it is the correct reset
/// signal. A session where the mic really is muted never reaches that
/// path (all its presses are silent), so it can't be wrongly cleared.
#[derive(Clone, Default)]
pub struct MicSilencedFlag(Arc<AtomicBool>);

impl MicSilencedFlag {
    pub fn is_silenced(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn mark_silenced(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Clear the "cache is lying" latch. Called on the audible-press
    /// path — see the type doc for why an audible press is proof the
    /// latch is stale.
    pub fn clear_silenced(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Spike-only — runtime selector for the per-press ASR backend.
///
/// `is_enabled() == true` routes the press through Deepgram Nova-3
/// (the existing streaming "English Fast Mode" path); `false` routes
/// through the Groq Whisper batch client. Defaults to `false` so a
/// fresh launch picks Groq Whisper as the new default.
///
/// Atomic so the tray menu thread can flip the value without
/// coordinating with the session's tokio task for a simple boolean
/// read.
#[derive(Clone, Default)]
pub struct EnglishFastModeFlag(Arc<AtomicBool>);

impl EnglishFastModeFlag {
    pub fn new(initial: bool) -> Self {
        Self(Arc::new(AtomicBool::new(initial)))
    }

    pub fn is_enabled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Flip the flag and return the new value.
    pub fn toggle(&self) -> bool {
        !self.0.fetch_xor(true, Ordering::SeqCst)
    }

    pub fn set(&self, enabled: bool) {
        self.0.store(enabled, Ordering::SeqCst);
    }
}

/// Backlog 0012 — user-facing escape hatch for code-switching dictation
/// patterns. When `true`, every press skips the LID router and routes
/// directly to Whisper batch. Trades the speed of Deepgram for the
/// bilingual correctness of Whisper — the right default for users who
/// dictate English + Tagalog (or any code-switched language pair) in
/// the same press. See `.claude/learned/002_deepgram_alone_is_not_enough_for_taglish.md`
/// for the data behind this trade-off.
///
/// Mirrors `EnglishFastModeFlag`'s shape — atomic so the tray menu can
/// flip it without coordinating with the session task; the next press
/// reads the new value. Preferred over `EnglishFastModeFlag` when both
/// are on (correctness wins over speed in `handle_hotkey_pressed`).
#[derive(Clone, Default)]
pub struct BilingualModeFlag(Arc<AtomicBool>);

impl BilingualModeFlag {
    pub fn new(initial: bool) -> Self {
        Self(Arc::new(AtomicBool::new(initial)))
    }

    pub fn is_enabled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn toggle(&self) -> bool {
        !self.0.fetch_xor(true, Ordering::SeqCst)
    }

    pub fn set(&self, enabled: bool) {
        self.0.store(enabled, Ordering::SeqCst);
    }
}

/// True when an `Authorized` AV reading combined with observed silence
/// indicates the AVFoundation cache is lying. Pulled out so the gate
/// is testable without standing up AVFoundation.
pub(crate) fn av_cache_is_lying(av_status: MicrophoneStatus) -> bool {
    matches!(av_status, MicrophoneStatus::Authorized)
}

/// Cadence of `KeepAlive` pings on parked sockets. Deepgram closes idle
/// sockets after ~10–15 s; pinging every 5 s keeps the parked socket alive
/// indefinitely with comfortable margin. Cheap (one tiny text frame).
pub(crate) const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// Budget for the parked-socket liveness probe in [`DeepgramPool::take`]
/// (plan 039 slice 4, task 12). Deliberately far below `deepgram::SEND_TIMEOUT`
/// (1 s): a healthy parked socket answers the tiny KeepAlive frame in well
/// under a millisecond, so 200 ms is generous headroom, while a wedged
/// half-open socket is abandoned in 200 ms instead of stalling press start for
/// a full second.
pub(crate) const PARKED_PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// Hard cap on the synchronous capture-start probe (`AudioCapture::start`:
/// permission check + cpal default-input enumeration + `Ctl::Start` handoff)
/// that the driver now AWAITS before beginning a session (plan 039 task 32).
///
/// A healthy device answers in well under a millisecond; a slow Bluetooth/USB
/// mic in the low hundreds. This bound exists solely to rescue a *wedged*
/// CoreAudio device open — without it, a hung probe would block the driver loop
/// forever and kill EVERY subsequent press until the app is restarted (before
/// task 32 the probe ran on a detached thread, so a hang degraded only that one
/// press). `spawn_blocking` can't be cancelled, so on expiry the driver stops
/// awaiting it and surfaces a capture-start error. Crucially it does NOT drop
/// the probe's `JoinHandle`: a merely-slow (not permanently wedged) probe can
/// still finish and OPEN the microphone *after* the driver has moved on, which
/// would leave a hot mic streaming with no session or HUD (a privacy leak). The
/// handle is instead handed to [`spawn_late_capture_stop`], which stops that
/// late-opened, orphaned mic. Sized generously above any realistic device-init
/// latency so it never fires on the happy path.
const CAPTURE_START_TIMEOUT: Duration = Duration::from_secs(5);

/// Run a parked-socket liveness `probe` future under a hard `budget`.
///
/// Returns `true` only when the probe completed with `Ok(())` inside the
/// budget; a timeout OR an `Err` both return `false`, meaning "discard this
/// parked slot and open inline." Extracted as a free function so the
/// budget-cap behaviour is unit-testable with an injected probe future,
/// without needing a real wedged TCP socket.
pub(crate) async fn probe_within<F>(budget: Duration, probe: F) -> bool
where
    F: std::future::Future<Output = Result<(), MuniError>>,
{
    matches!(tokio::time::timeout(budget, probe).await, Ok(Ok(())))
}

/// Maximum time the driver loop will block on a release event before
/// force-recovering a **press-and-hold (PTT)** press cycle. Backstop
/// for backlog 0003: under bursty `kCGEventFlagsChanged` delivery the
/// OS can drop events such that a press lands without a paired
/// release; without this timeout `wait_for_release` blocks forever,
/// the HUD stays visible without the hotkey held, and the user has to
/// restart the app.
///
/// Sized well above the longest realistic *hold* (manual-QA §9
/// validates a 2-minute monologue) but short enough that the bug
/// scenario auto-resolves before the user concludes Muni is wedged.
/// Tap-to-toggle sessions are NOT bounded by this — they have explicit
/// terminators and use [`TOGGLE_WAIT_FOR_RELEASE_TIMEOUT`] instead.
const WAIT_FOR_RELEASE_TIMEOUT: Duration = Duration::from_secs(180);

/// Maximum time the driver loop will block on a release event before
/// force-recovering a **tap-to-toggle** press cycle.
///
/// A toggle session has explicit terminators — re-tap, Esc, and the
/// 60 s silence watchdog — so the dropped-modifier-release wedge that
/// motivates [`WAIT_FOR_RELEASE_TIMEOUT`] cannot occur here. Applying
/// the 180 s PTT cap to toggle was a bug: it force-committed a
/// continuous hands-free dictation mid-sentence at exactly 3 minutes
/// even though the user was still talking and had not stopped the
/// session. This far higher cap exists only to bound audio-buffer
/// growth and transcription cost if all three terminators somehow
/// fail; it matches the toggle audio ceiling (`MAX_TOGGLE_DURATION_S`
/// in `hotkey.rs`) so a long but legitimate dictation is never cut.
const TOGGLE_WAIT_FOR_RELEASE_TIMEOUT: Duration = Duration::from_secs(600);

/// Select the wait-for-release backstop for a press cycle by gesture.
///
/// Extracted as a pure function so the PTT-vs-toggle distinction is
/// unit-testable without driving the full async driver loop. The
/// invariant that matters for regression safety: a toggle session must
/// get a strictly larger cap than PTT so a long continuous dictation is
/// not force-committed at the PTT backstop.
fn release_timeout_for(mode: HotkeyMode) -> Duration {
    match mode {
        HotkeyMode::Ptt => WAIT_FOR_RELEASE_TIMEOUT,
        HotkeyMode::ToggleLocked => TOGGLE_WAIT_FOR_RELEASE_TIMEOUT,
    }
}

/// Closure invoked to surface Tauri events from the orchestrator. Production
/// wires this to `AppHandle::emit`; tests record `(event, payload)` tuples
/// without dragging Tauri's runtime into the test harness.
pub type EventEmitter = Arc<dyn Fn(&str, String) + Send + Sync + 'static>;

/// Feature 037 — invoked when a completed dictation lands with no editable
/// field focused. Surfaces the HUD notice telling the user which hotkey
/// re-pastes the held dictation. Production closes over the `AppHandle` + store
/// so the copy reflects the live re-paste binding (`lib.rs`); tests pass a
/// recording or no-op closure via [`noop_repaste_notice`].
pub type RepasteNotice = Arc<dyn Fn() + Send + Sync + 'static>;

/// A [`RepasteNotice`] that does nothing — the default for tests and any
/// construction site that doesn't wire the live HUD notice.
pub fn noop_repaste_notice() -> RepasteNotice {
    Arc::new(|| {})
}

/// Feature 033 — operational metrics for the `dictation_completed` PostHog
/// event, threaded into [`DictationSession::deliver_final`] so the metadata-only
/// health event can be emitted at the single paste-success funnel.
///
/// Carries NO transcript content — only timings, the model id, and a degraded
/// flag. The char-count + served-by + target-app come from `deliver_final`'s
/// own arguments at emit time. `cleanup_latency_ms` is `None` on the raw-paste
/// fallback paths (no successful cleanup ran).
#[derive(Debug, Clone)]
pub struct CompletionMetrics {
    /// Press wall-clock in ms — the audio-duration proxy for the event bucket.
    pub press_duration_ms: u64,
    /// Cleanup round-trip in ms; `None` when the press pasted raw (no cleanup).
    pub cleanup_latency_ms: Option<u64>,
    /// Cleanup model id actually used (or the raw-fallback sentinel). Owned
    /// because the resolved per-press model is a runtime `String`.
    pub cleanup_model: String,
    /// `true` when the press only completed via a fallback/raw path.
    pub degraded: bool,
}

impl CompletionMetrics {
    /// Metrics for a raw-paste fallback (cleanup couldn't run / both attempts
    /// failed): no cleanup latency, the raw-fallback model sentinel, degraded.
    fn raw_fallback(press_duration: Duration) -> Self {
        Self {
            press_duration_ms: press_duration.as_millis() as u64,
            cleanup_latency_ms: None,
            cleanup_model: CLEANUP_MODEL_RAW_FALLBACK.to_string(),
            degraded: true,
        }
    }

    /// Throwaway metrics for tests that exercise `deliver_final` directly and
    /// don't assert on the telemetry event (analytics is a no-op in tests —
    /// `init_posthog` never runs, so `emit_event` returns immediately).
    #[cfg(test)]
    fn test_default() -> Self {
        Self {
            press_duration_ms: 0,
            cleanup_latency_ms: None,
            cleanup_model: CLEANUP_MODEL_RAW_FALLBACK.to_string(),
            degraded: false,
        }
    }
}

/// Model-id sentinel recorded on `dictation_completed` when the press pasted the
/// raw transcript because cleanup couldn't run (missing client/prompt/key, or
/// both Groq attempts failed). Distinguishes a degraded paste from a clean one
/// in dashboards without inventing a fake model name.
pub const CLEANUP_MODEL_RAW_FALLBACK: &str = "raw-fallback";

/// Feature 033 — fixed `empty_reason` vocabulary for the `dictation_empty`
/// health event. These name WHY a press produced no paste; they are operational
/// labels, never anything derived from what the user said.
pub const EMPTY_REASON_SILENT_PRESS: &str = "silent_press";
pub const EMPTY_REASON_EMPTY_TRANSCRIPT: &str = "empty_transcript";
pub const EMPTY_REASON_HALLUCINATION: &str = "hallucination";
pub const EMPTY_REASON_TOO_SHORT: &str = "too_short";
pub const EMPTY_REASON_VAD_NO_SPEECH: &str = "vad_no_speech";

/// Closure invoked on every orchestrator state transition. Production wires
/// this to `tray::set_state` (and any future HUD-side hooks); tests use a
/// recording stub.
///
/// The orchestrator deliberately doesn't depend on the `tray` module — the
/// notifier is a function pointer so the tray can be replaced (or removed,
/// per the Phase 6 plan's no-tray fallback) without touching this file.
pub type StateNotifier = Arc<dyn Fn(SessionState) + Send + Sync + 'static>;

/// Coarse-grained orchestrator state. Mirrors the tray icon states that the
/// session pipeline drives.
///
/// The set is intentionally smaller than `tray::TrayState` — `Offline` is a
/// network-reachability concern (Phase 11+) the session itself doesn't
/// observe directly, and `Cleaning` always implies "we have audio in
/// flight" not "Groq specifically is running" so it covers both finalize
/// and Groq cleanup phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionState {
    Idle,
    Listening,
    /// Plan 030 — a tap-to-toggle "locked" listening session is in
    /// flight. Behaviourally identical to `Listening` for the audio +
    /// transcription pipeline; the HUD renders a visibly distinct pill
    /// so the user can tell at a glance that the session is locked and
    /// must be ended deliberately (re-tap, Esc, or 60 s timeout).
    ListeningLocked,
    Cleaning,
    /// Plan 012 — Gladia primary has failed mid-press and the
    /// Whisper-batch recovery path is in flight. Lights up the HUD pill
    /// with the recovery visual so the user knows we're still working
    /// on the press instead of staring at an unchanged screen.
    Recovering,
    Error,
}

impl SessionState {
    /// Wire-format payload used by [`EVENT_SESSION_STATE_CHANGED`] and the
    /// `set_tray_state` IPC command. Kept in lockstep with the serde
    /// representation so both sides agree.
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Listening => "listening",
            Self::ListeningLocked => "listeningLocked",
            Self::Cleaning => "cleaning",
            Self::Recovering => "recovering",
            Self::Error => "error",
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Listening,
            2 => Self::Cleaning,
            3 => Self::Recovering,
            4 => Self::Error,
            // Plan 030 — appended at 5 (NOT inserted) so a hot upgrade
            // can't misread an in-flight `AtomicU8` discriminant.
            5 => Self::ListeningLocked,
            _ => Self::Idle,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Listening => 1,
            Self::Cleaning => 2,
            Self::Recovering => 3,
            Self::Error => 4,
            // Plan 030 — append-only; never renumber.
            Self::ListeningLocked => 5,
        }
    }
}

/// Plan 030 — release-channel payload distinguishing a normal commit
/// (the existing press-and-hold release path and the toggle re-tap +
/// timeout paths) from an explicit cancel (toggle Esc or tray "Cancel
/// current session").
///
/// The driver loop dispatches on this variant; cancel routes through
/// [`DictationSession::handle_hotkey_cancelled`] which drops audio
/// without finalize/paste/history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseKind {
    /// Normal release — run finalize + cleanup + paste + history.
    /// Produced by press-and-hold release, the toggle re-tap, the silence
    /// timeout, and the safety-cap.
    Commit,
    /// Like [`Self::Commit`], but after the paste lands, also inject a
    /// synthetic Enter into the focused app so the text is submitted (e.g.
    /// the chat message is sent). Produced only by the "press Enter to
    /// finish" toggle gesture.
    CommitAndSubmit,
    /// Cancel — discard audio, no paste, no history row.
    Cancel,
}

/// Plan 030 — press-channel payload distinguishing the legacy
/// press-and-hold gesture from a tap-to-toggle "locked" session.
///
/// Behaviourally the audio + transcription pipeline is identical; the
/// only difference is which `SessionState` variant the orchestrator
/// notifies (`Listening` vs `ListeningLocked`) so the HUD can render
/// the locked-mode affordance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyMode {
    /// Press-and-hold — the existing gesture. Hold ≥ debounce, speak,
    /// release to commit.
    Ptt,
    /// Tap-to-toggle — quick tap starts a locked session that runs
    /// until re-tap (commit), Esc (cancel), or the 60 s safety cap
    /// (commit).
    ToggleLocked,
}

/// Authoritative store of the orchestrator's current [`SessionState`].
///
/// `notify_state` emits `session://state-changed` to the React layer, but a
/// WebView that mounts AFTER an emission has no way to recover the event.
/// On macOS, the HUD window is created with `visible: false` (see
/// `tauri.conf.json`), which can leave WKWebView's JS runtime cold until the
/// first `window.show()` — and the first show is triggered by the very same
/// `Listening` transition the HUD needs to observe. Result: on the first
/// press after launch, React's `listen("session://state-changed")` registers
/// AFTER the `listening` event has already fired, the pill stays hidden
/// through the press, then appears mid-Cleaning when the listener finally
/// catches up.
///
/// `SessionStateTracker` is the resync source. The Rust-side
/// `state_notifier` writes to it on every transition; the
/// `get_session_state` IPC command reads from it so React can seed initial
/// state on mount instead of starting from a stale `idle` default.
#[derive(Debug, Default)]
pub struct SessionStateTracker(std::sync::atomic::AtomicU8);

impl SessionStateTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn set(&self, state: SessionState) {
        self.0.store(state.as_u8(), Ordering::SeqCst);
    }

    pub fn get(&self) -> SessionState {
        SessionState::from_u8(self.0.load(Ordering::SeqCst))
    }
}

/// Dependencies the orchestrator owns at runtime.
///
/// `history` (Phase 10) and `present_error` (Phase 10) are wired so a
/// successful paste persists a `DictationRecord` and any failure surfaces
/// through the system notification / quiet-event path. Both are
/// `Option`/no-op tolerant so unit tests don't need a real SQLite file
/// or a live `AppHandle`. `groq` and `prompt` remain `Option` — Phase 4
/// lets each fail gracefully at boot; the orchestrator falls back to
/// raw paste in that case.
pub struct SessionDeps {
    pub deepgram_pool: Arc<DeepgramPool>,
    pub groq: Option<Arc<GroqClient>>,
    pub prompt: Option<Arc<CleanupPrompt>>,
    pub injector: Arc<dyn PlatformInjector>,
    pub emitter: EventEmitter,
    pub state_notifier: StateNotifier,
    /// Phase 10 — invoked on every typed error so the ErrorPresenter
    /// can raise a notification (loud) or emit `error://quiet` (quiet).
    /// Defaults to a no-op via `error_presenter::noop_presenter`.
    pub present_error: PresentError,
    /// Feature 037 — invoked when a completed (non-empty) dictation is delivered
    /// while no editable field is focused. The paste is skipped, the row is
    /// still persisted, and this fires the HUD notice pointing the user at the
    /// re-paste hotkey. Defaults to a no-op via [`noop_repaste_notice`].
    pub show_repaste_notice: RepasteNotice,
    /// Phase 10 — when `Some`, a successful paste is persisted as a
    /// new `DictationRecord`. `None` in tests + when the user has
    /// disabled history retention via Settings → General.
    pub history: Option<Arc<HistoryStore>>,
    /// Phase 11 — set by the orchestrator when silence detection
    /// concludes the mic isn't producing audio. Exposed to the
    /// frontend so the Permissions card can override the (lying)
    /// AVFoundation cache reading and tell the user to restart.
    pub mic_silenced: MicSilencedFlag,
    /// Spike — Groq Whisper client used when [`english_fast_mode`] is
    /// `false`. `None` only in tests / when client init failed; the
    /// orchestrator falls back to the Deepgram path with a logged
    /// warning rather than failing the press.
    pub whisper: Option<Arc<GroqWhisperClient>>,
    /// Parakeet local-ASR sidecar. `Some` only when
    /// `MUNI_ASR_BACKEND=parakeet` AND the sidecar spawned and reported
    /// READY at boot; `None` otherwise (every test, and the default
    /// Deepgram backend). When `Some`, the English arm of
    /// `finalize_auto_detect` transcribes the buffered PCM on-device
    /// instead of finalizing Deepgram; on any Parakeet failure it falls
    /// back to the Deepgram finalize path, so this never fails a press.
    /// Parakeet has no Tagalog — the Whisper arm is untouched.
    pub parakeet: Option<Arc<ParakeetClient>>,
    /// Feature 003 — text-LID classifier driven by the press's
    /// transcribed slice. Production wires Gemini 3.1 Flash-Lite
    /// here; the trait abstraction keeps the orchestrator agnostic
    /// so swapping in a local Gemma client (or any other provider)
    /// is a one-line change in `lib.rs::setup`. `None` → the LID
    /// task logs and defaults the press to Whisper, mirroring the
    /// `whisper: None` graceful-degradation path.
    pub text_lid: Option<Arc<dyn TextLidClassifier>>,
    /// Backlog 0012 — secondary LID classifier for hybrid mode. When
    /// `Some`, the LID task fires this in parallel with the primary
    /// `text_lid` on the pass#2 slice; a Gemini-via-secondary
    /// English verdict can override the primary's Whisper decision
    /// to Deepgram (constrained direction — protects scenario 6
    /// long-Taglish accuracy). `None` in tests + when
    /// `MUNI_LID_HYBRID` is unset, in which case behavior is
    /// identical to the single-classifier path (backlog 0011).
    pub text_lid_secondary: Option<Arc<dyn TextLidClassifier>>,
    /// Feature 020 — local audio-LID classifier. When `Some`, the LID
    /// task dispatches to [`Self::run_audio_lid_pass`] (raw audio →
    /// whisper.cpp encoder + language head, no network call) instead
    /// of the text-LID two-pass protocol. Audio-LID is the new default
    /// at boot; text-LID stays wired as a rollback path
    /// (`MUNI_LID_PROVIDER=groq` or `gemini`). Mutually exclusive in
    /// practice — the factory in `lib.rs` populates exactly one of the
    /// two depending on `MUNI_LID_PROVIDER`. `None` in tests + when
    /// the audio-LID model failed to load at boot.
    pub audio_lid: Option<Arc<dyn AudioLidClassifier>>,
    /// Spike — runtime selector between the Whisper path (default)
    /// and the Deepgram fast path. Defaults to `false` (Whisper).
    pub english_fast_mode: EnglishFastModeFlag,
    /// Backlog 0012 — user-facing accuracy escape hatch. When `true`,
    /// every press skips LID and routes to Whisper for bilingual
    /// correctness (slower paste, but no Tagalog content silently
    /// dropped by Deepgram). Takes precedence over `english_fast_mode`
    /// — correctness wins over speed when the user has opted into
    /// code-switching support.
    pub bilingual_mode: BilingualModeFlag,
    /// Feature 005 — fire-and-forget channel into the
    /// [`crate::usage_writer`] task. The orchestrator pushes one
    /// `UsageRecord` per successful provider call (Deepgram /
    /// Groq cleanup / Groq Whisper / Groq LID / Gemini LID); the
    /// writer freezes a `cost_usd` and inserts an `api_calls` row.
    /// `None` in tests so the harness doesn't have to spin a writer.
    pub usage_tx: Option<mpsc::Sender<UsageRecord>>,
    /// Plan 041 (wave 1) — the cost-tracking store, shared with the
    /// [`crate::usage_writer`]. Reused by the delivery tail to persist
    /// one `press_timings` row per completed press, inside the existing
    /// `persist_history` `spawn_blocking` closure (after paste, off the
    /// hot path). `None` in tests + when the store failed to open at
    /// boot — in that case the `press_timing` log line still fires but
    /// no row is written.
    pub usage_store: Option<Arc<UsageStore>>,
    /// Plan 041 (task 7) — shared Groq activity tracker. The delivery
    /// tail bumps `note_prefix_touch` after a successful real cleanup
    /// (that warms Groq's prompt-prefix cache) and the Whisper path
    /// bumps `note_call` after a successful transcribe, feeding the
    /// keepalive skip-gate and the periodic cache re-warm. `None` in
    /// tests + when the tracker isn't managed (never in production).
    pub groq_activity: Option<Arc<GroqActivity>>,
    /// Feature 013 — deterministic per-user substitution layer applied
    /// to the raw transcript between trim and Groq cleanup. See
    /// [`crate::my_words`] for the matcher semantics. Always present;
    /// when the user has no rules (or has flipped the kill switch) the
    /// `apply` call is a near-free clone of the input.
    pub my_words: Arc<crate::my_words::MyWords>,
    /// Feature 014 — free-form vocabulary context the user authored in
    /// Settings → Cleanup. Snapshot is read per-press inside
    /// [`run_groq_cleanup`] and prepended to the cleanup system prompt.
    /// Empty (the default) is a no-op: the prompt sent to Groq stays
    /// byte-identical to the pre-feature behaviour. Always present.
    pub about_me: Arc<crate::about_me::AboutMe>,
    /// Feature 015 — vocabulary soft-bias word list. Snapshot is read
    /// per-press inside [`run_groq_cleanup`], rendered to a markdown
    /// block, and prepended to the cleanup system prompt above the
    /// bundled body (after any About Me block). Empty list or
    /// disabled is a no-op: the prompt sent to Groq stays
    /// byte-identical to the pre-feature behaviour. Always present.
    pub vocabulary: Arc<crate::vocabulary::Vocabulary>,
    /// User-authored "preferences" prompt. Snapshot is read per-press
    /// inside [`run_groq_cleanup`] and appended AFTER the bundled
    /// cleanup body with a header that explicitly tells the model to
    /// follow these instructions when they conflict with rules above.
    /// Empty (the default) is a no-op: the prompt sent to Groq stays
    /// byte-identical to the pre-feature behaviour.
    pub user_prompt: Arc<crate::user_prompt::UserPrompt>,
    /// Feature 023 (backlog 0040) — content-aware silent-press gate.
    /// When `Some`, the release-path Whisper batch and the audio-LID
    /// hybrid slice classify both gate on
    /// [`VadDetector::predict_speech`] BEFORE firing their respective
    /// ASR calls. `None` (the boot-time disable path,
    /// `MUNI_VAD_GATE=off`, or a Silero load failure absent
    /// `MUNI_VAD_REQUIRED=1`) keeps feature 022's amplitude-only
    /// behavior. Tests pass `MockVad` for deterministic gate
    /// composition.
    pub vad_detector: Option<Arc<dyn crate::vad::VadDetector>>,
    /// Feature 024 (backlog 0042) — constructs per-stream
    /// [`crate::vad::StreamingVadDetector`] instances. `None` when both
    /// `MUNI_VAD_STREAM_HYBRID` and `MUNI_VAD_TRIM_RELEASE_BUFFER` are
    /// off (the default on first ship). Each factory invocation builds
    /// a fresh detector (per-stream ownership, no shared Mutex). Tests
    /// pass `None` (kill switches off → byte-identical behavior) or a
    /// closure returning [`crate::vad::PassThroughStreamingVad`] for
    /// deterministic gate-composition.
    pub streaming_vad_factory: Option<crate::vad::StreamingVadFactory>,
}

/// Push-to-talk dictation orchestrator.
///
/// Construct via [`DictationSession::new`] and either drive directly with
/// [`handle_hotkey_pressed`](Self::handle_hotkey_pressed) /
/// [`handle_hotkey_released`](Self::handle_hotkey_released) (tests) or wire
/// to the live hotkey/audio plumbing via [`spawn_driver`](Self::spawn_driver)
/// (production).
pub struct DictationSession {
    deps: SessionDeps,
    active: TokioMutex<Option<ActiveSession>>,
    /// Monotonic HUD epoch (plan 039 task 25). Bumped once per press when
    /// capture begins. A spawned delivery captures its press's epoch and
    /// suppresses its terminal HUD transitions (`Idle`/`Error`) when a
    /// newer press has since taken the HUD — "recording wins": a press
    /// started while the previous press is still Cleaning must show
    /// Listening, and the older delivery finishing must not stomp it back
    /// to Idle. Paste/history/telemetry are never gated by this — only the
    /// HUD state.
    press_epoch: AtomicU64,
    /// Tail of the in-order delivery chain (plan 039 task 25). Cleanup runs
    /// concurrently off the driver loop, but pastes must land in press
    /// order. Each spawned delivery installs a fresh completion receiver
    /// here and captures the previous one; `deliver_final` awaits its
    /// predecessor right before pasting. Only presses that actually paste
    /// take a slot (empty/aborted presses never spawn a delivery), so the
    /// chain reflects delivered presses exactly. A `std::sync::Mutex`
    /// (not tokio) because the critical section is a single non-async swap.
    delivery_order_tail: std::sync::Mutex<Option<oneshot::Receiver<()>>>,
}

/// Per-press state held between [`handle_hotkey_pressed`](DictationSession::handle_hotkey_pressed)
/// and [`handle_hotkey_released`](DictationSession::handle_hotkey_released).
///
/// Spike-only branching: pressing while `english_fast_mode == true`
/// produces the [`Self::Deepgram`] variant (the original streaming
/// path); pressing while it's `false` produces [`Self::Whisper`] (a
/// buffered batch path that POSTs to Groq Whisper on release). Both
/// variants carry the same `pressed_at` and a forwarder JoinHandle
/// returning the press's peak amplitude so silence detection works
/// identically across both routes.
enum ActiveSession {
    /// Manual override — fast_mode toggled on. Skips LID, streams
    /// directly to Deepgram. Identical to the original press flow.
    Deepgram(DeepgramActive),
    /// Default — Option A router. Streams to Deepgram speculatively
    /// while a parallel LID task decides on the final backend; the
    /// release path collapses to whichever the LID task picked
    /// (Deepgram or Whisper). When the LID task hasn't returned by
    /// release we fall through to the safe Deepgram path.
    AutoDetect(AutoDetectActive),
    /// Plan 039 task 26 — buffer-only capture installed when the Deepgram
    /// pool is down at press start. No streaming socket; the press is
    /// Whisper-committed and its buffered PCM is batch-transcribed on
    /// release via the Groq Whisper → Gladia chain.
    WhisperBatch(WhisperBatchActive),
}

impl ActiveSession {
    fn pressed_at(&self) -> Instant {
        match self {
            Self::Deepgram(s) => s.pressed_at,
            Self::AutoDetect(s) => s.pressed_at,
            Self::WhisperBatch(s) => s.pressed_at,
        }
    }

    fn take_release_tx(&mut self) -> Option<oneshot::Sender<()>> {
        match self {
            Self::Deepgram(s) => s.released_tx.take(),
            Self::AutoDetect(s) => s.released_tx.take(),
            Self::WhisperBatch(s) => s.released_tx.take(),
        }
    }
}

/// Deepgram-streaming branch state.
struct DeepgramActive {
    client: Arc<DeepgramClient>,
    /// Forwarder returns `(buffered_samples, peak_amplitude)`. The peak
    /// drives the silence-detection path (distinguishing a quiet user
    /// from a mic macOS silenced after a runtime TCC toggle, where
    /// AVFoundation lies but cpal honestly delivers zeros). The buffer
    /// is empty unless the Parakeet local backend is active, in which
    /// case `finalize_deepgram` transcribes it on release instead of
    /// finalizing Deepgram.
    forwarder: JoinHandle<(Vec<i16>, i16)>,
    /// Signalled on release so the forwarder transitions into the
    /// post-release drain window. Taken (consumed) on release.
    released_tx: Option<oneshot::Sender<()>>,
    /// Wall-clock instant the press started. The release handler
    /// compares against [`MIN_PRESS_FOR_SILENCE_DETECTION`] so a
    /// genuinely-short press doesn't trip the silence heuristic.
    pressed_at: Instant,
}

/// Spike — auto-detect router branch state.
///
/// Audio is fan-out streamed to Deepgram (optimistic English fast
/// path) AND copied into a buffer the LID task / Whisper fallback
/// path can use. The LID task fires once enough audio has been
/// captured (see `LID_SLICE_SAMPLES`); when it returns, [`decision`]
/// is filled and [`decision_notify`] is signalled. If LID picked
/// non-English, the Deepgram client is closed mid-press so we don't
/// keep streaming bytes the model is mishearing as English.
struct AutoDetectActive {
    deepgram_client: Arc<DeepgramClient>,
    /// Forwarder returns `(buffered_samples, peak_amplitude)`. It
    /// always streams to Deepgram AND copies into the buffer; if the
    /// LID task aborts the Deepgram client, the forwarder stops
    /// trying to send but keeps collecting samples for the Whisper
    /// fallback.
    forwarder: JoinHandle<(Vec<i16>, i16)>,
    /// LID outcome — set by the LID task when it lands. `None` means
    /// LID hasn't completed yet (e.g. press shorter than the slice
    /// window, network slow, or call still in flight). The release
    /// path defaults to Deepgram in that case.
    decision: Arc<TokioMutex<Option<RouterDecision>>>,
    decision_notify: Arc<Notify>,
    /// Flipped `true` once by `finalize_auto_detect` on release so every LID /
    /// hybrid waiter's collection loop can short-circuit instead of hanging on
    /// the never-closing broadcast channel.
    ///
    /// Bug surface (backlog 0011): `AudioCapture::stop()` only sends a
    /// control message — the broadcast `Sender` it owns is not
    /// dropped, so `recv()` after stop returns neither a chunk nor
    /// `RecvError::Closed`. The collection loops would otherwise hang
    /// until `lid_handle.abort()` fires from `finalize_auto_detect`,
    /// stranding short pure-English presses (sub-3.5 s) into Whisper
    /// batch instead of trusting pass#1's English verdict.
    ///
    /// A `watch::Sender<bool>` (plan 039 task 13) — not a `Notify`. Waiters
    /// call [`released`] on a `subscribe()`d receiver: `watch` is both
    /// **broadcast** (a single fire wakes the audio-LID pass AND the hybrid
    /// task, where `notify_one()` woke only one and starved the other for up to
    /// 1 s) and **sticky** (a receiver that first checks after the flip still
    /// sees `true`, covering the "release fires between pass#1 and pass#2"
    /// window the old `notify_one` permit handled). Per-press: every press
    /// allocates a fresh channel.
    release_tx: watch::Sender<bool>,
    /// Cancels the LID task if release happens before LID completes.
    /// Best-effort — the task itself short-circuits on its bounded
    /// timeout regardless.
    lid_handle: tauri::async_runtime::JoinHandle<()>,
    /// Backlog 0012 — sentinel set by [`finalize_auto_detect`] the
    /// moment it has consumed the LID decision. The hybrid-mode
    /// Gemini override task checks this before mutating the cell so
    /// a Gemini reply that races `lid_handle.abort()` cannot flip a
    /// press the orchestrator has already routed.
    committed: Arc<AtomicBool>,
    /// Hybrid-mode Gemini override task handle. Populated by
    /// [`Self::spawn_lid_task`] when `MUNI_LID_HYBRID=true` and the
    /// pass#2 transcript is in hand; aborted by
    /// [`Self::finalize_auto_detect`] right after `committed` flips,
    /// so a Gemini call still in flight on release does not keep
    /// burning a Gemini RPC (and any HTTP/CPU contention with the
    /// downstream Groq cleanup) for a verdict the orchestrator is
    /// already going to discard via the `committed` sentinel.
    gemini_handle: Arc<TokioMutex<Option<tauri::async_runtime::JoinHandle<()>>>>,
    /// Feature 019 — handle for the confidence-trigger task that
    /// monitors per-chunk Deepgram confidence after pass#2 commits
    /// English, fires a mid-press LID re-pass on a run of low scores,
    /// and may flip the route from Deepgram to Whisper. Spawned only
    /// when [`MUNI_LID_CONFIDENCE_TRIGGER_ENV`] is truthy and pass#2
    /// has actually committed Deepgram (no trigger on Whisper presses,
    /// no trigger when feature is disabled, no trigger on pass#1/2
    /// error paths). Aborted by [`Self::finalize_auto_detect`]
    /// immediately after `committed` flips so a re-pass already
    /// in flight cannot overwrite the routed decision.
    confidence_trigger_handle: Arc<TokioMutex<Option<tauri::async_runtime::JoinHandle<()>>>>,
    /// Feature 019 — set `true` by the trigger task just before it
    /// calls `transcribe_for_lid` (the re-pass is in flight) and
    /// cleared when the re-pass returns. Read by
    /// [`Self::finalize_auto_detect`] on release: when `true` we wait
    /// briefly (`TRIGGER_REPASS_WAIT_MS`) for the verdict to land
    /// *before* committing the route, so a code-switched press that
    /// triggers a re-pass close to release doesn't silently drop its
    /// Tagalog tail. Always `false` on pure-English presses (zero
    /// added latency to the common case).
    trigger_inflight: Arc<AtomicBool>,
    /// Feature 021 round-6 (2026-05-18) — inflight counter for
    /// audio-LID-hybrid inner classify tasks. Each call to
    /// [`Self::spawn_audio_hybrid_inner_classify`] increments the
    /// counter at task entry and decrements + `notify_waiters` on
    /// completion (via a Drop guard, so panics + abort cases also
    /// clear the counter cleanly).
    ///
    /// Read by [`Self::finalize_auto_detect`] on release: when route
    /// is Deepgram and inflight > 0, the release path waits up to
    /// `TRIGGER_REPASS_WAIT_MS` for `decision_notify` to fire (either
    /// a successful override flips the cell to Whisper, or all
    /// inflight tasks finish without flipping). This catches the
    /// "classify result lands just after release" race observed in
    /// round-6 dogfood (P01 LEAD/TRAIL `taglish` landed +555 ms,
    /// P06 ROLL `taglish` landed +605 ms — both after committed=true
    /// had been set, so the override no-op'd).
    ///
    /// Mirrors `trigger_inflight`'s structural pattern but is a
    /// counter instead of a bool because the hybrid task can have
    /// multiple inner classifies in flight concurrently (leading +
    /// trailing in parallel, plus rolling on long presses).
    audio_hybrid_inflight: Arc<AtomicUsize>,
    /// Feature 021 — handle for the audio-LID-side Gemini hybrid
    /// task. When `Some`, the task is running in the background
    /// alongside [`Self::run_audio_lid_pass`] and may fire
    /// [`Self::override_or_commit_to_whisper_via_hybrid`] if it lands
    /// with a `tagalog` / `taglish` verdict before press finalisation.
    /// [`Self::finalize_auto_detect`] aborts this on release (after
    /// setting `committed=true`) so a late Gemini reply can't mutate
    /// a routed decision cell.
    ///
    /// Mutually exclusive at runtime with [`Self::gemini_handle`]:
    /// that field is the text-LID-primary side's parallel-Gemini
    /// (backlog 0012); this field is the audio-LID-primary side's
    /// parallel-Gemini (feature 021). The boot-time factory picks
    /// exactly one primary, so only one of the two is ever populated
    /// for a given press.
    audio_hybrid_handle: Arc<TokioMutex<Option<tauri::async_runtime::JoinHandle<()>>>>,
    /// Feature 024 (backlog 0042) — speech-only mirror buffer
    /// populated by [`DictationSession::spawn_audio_hybrid_task`]'s
    /// Site D filter while the hybrid is armed. Read by
    /// [`resolve_trimmed_release_buffer`] at release: if non-empty
    /// AND `MUNI_VAD_TRIM_RELEASE_BUFFER` is on, the mirror is handed
    /// to Whisper batch in place of the untrimmed press buffer.
    ///
    /// `Arc<TokioMutex>` so the hybrid task can extend it concurrently
    /// with the release path; the lock is held only across small
    /// memcpy + drain operations. Always allocated; remains empty when
    /// the hybrid never armed OR when `MUNI_VAD_STREAM_HYBRID` is off
    /// (Site D simply does not append).
    audio_hybrid_speech_mirror: Arc<TokioMutex<Vec<i16>>>,
    /// Backlog 0048 — mirror of `run_audio_lid_pass`'s local
    /// `consecutive_drift` counter. Written by the LID task on every
    /// drift state transition (Commit/Agree/IncrementDrift/Fire/IgnoreNoise);
    /// read by [`Self::finalize_auto_detect`] at release to decide
    /// whether to fire the at-release stale-drift override.
    ///
    /// Lives here (not on `SessionDeps`) because the counter is
    /// per-press state — it resets to 0 every press. `Arc<AtomicUsize>`
    /// because two tasks reach it: the LID task mutates it, the
    /// orchestrator reads it.
    ///
    /// The LID-task release-arm ALSO does its own at-release dispatch
    /// (defensive double-fire), but loses the tokio race against
    /// `finalize_auto_detect`'s synchronous progression to
    /// `committed.store(true)`. The orchestrator-side read+dispatch is
    /// the path that actually flips the route in time.
    audio_lid_drift_counter: Arc<AtomicUsize>,
    /// Backlog 0048 — release-fire floor loaded once at session
    /// construction (matches the value used by `run_audio_lid_pass`).
    /// Stored alongside the drift counter so
    /// [`Self::finalize_auto_detect`] can call
    /// [`audio_lid_decide_release_action`] without re-reading the env.
    audio_lid_release_drift_fire_floor: usize,
    /// Backlog 0048 v2 — `true` if the most recent post-commit audio-LID
    /// verdict was labelled `Other(_)` (whisper-tiny top1 ∉ {en, tl}).
    /// Reset to `false` on every Commit / Agree / IncrementDrift /
    /// FireOverrideToWhisper action; set to `true` on every IgnoreNoise
    /// action. Read at release by [`Self::finalize_auto_detect`].
    ///
    /// This bit captures the "whisper-tiny hallucinated a non-Tagalog
    /// language on a Tagalog tail" failure mode (gotcha #4 in backlog
    /// 0048): when the last classified window before release lands
    /// `top1=id`/`ru`/`es` with `p_tl < TAGLISH_SECONDARY_PROB_FLOOR`,
    /// the verdict gets labelled `Other(_)`, takes `IgnoreNoise`
    /// (which preserves drift but doesn't increment it), and the
    /// drift-counter-based at-release fire has no evidence to consume.
    /// This atomic gives the orchestrator a second signal: "the LAST
    /// thing audio-LID actually classified was non-English".
    audio_lid_last_post_commit_was_other: Arc<AtomicBool>,
    /// Backlog 0048 v2 — resolved value of
    /// [`MUNI_AUDIO_LID_RELEASE_OTHER_AS_TAGLISH_ENV`] at session
    /// construction. When `true`, [`Self::finalize_auto_detect`]
    /// fires the override at release if
    /// `audio_lid_last_post_commit_was_other == true` (in addition to
    /// the existing drift-counter rule).
    audio_lid_release_other_as_taglish: bool,
    /// Backlog 0052 — shared mirror of "the hybrid text-LID has
    /// recently classified a slice of this press as English".
    /// Set by [`DictationSession::spawn_audio_hybrid_inner_classify`] on an
    /// explicit English verdict only (plan 039 task 20 — an `Other` verdict
    /// no longer arms it, matching the `LidLabel::Other` non-English
    /// contract);
    /// read by [`DictationSession::finalize_auto_detect`] and the
    /// mid-press drift decision in [`DictationSession::apply_audio_lid_verdict`].
    ///
    /// Lives here (not on `SessionDeps`) because the bit is per-press
    /// state — it resets to `false` every press. `Arc<AtomicBool>`
    /// because the writer (hybrid task) and readers (orchestrator +
    /// LID task release arm) live on different tokio tasks.
    audio_hybrid_recent_text_lid_english: Arc<AtomicBool>,
    /// Backlog 0052 — env-resolved flag controlling the symmetric
    /// veto. When `false`, behavior matches feat/021's asymmetric
    /// direction (drift override fires regardless of hybrid
    /// verdict). When `true`, the readers AND the env knob both
    /// must agree before the veto applies.
    audio_lid_hybrid_veto_drift: bool,
    released_tx: Option<oneshot::Sender<()>>,
    pressed_at: Instant,
}

/// Buffer-only capture branch state (plan 039 task 26).
///
/// Installed when [`DeepgramPool::take`] fails at press start on the
/// AutoDetect route: rather than aborting the press with nothing pasted,
/// we capture + locally buffer the PCM (no streaming socket, no LID
/// arbitration — the text-LID's Deepgram English path is unavailable, so
/// the route is Whisper-committed up front) and, on release, replay the
/// buffer through the same Groq Whisper → Gladia batch chain the
/// Deepgram-route rescue uses (learned/011: independent infra beats
/// retrying a downed provider). The quiet amber `Recovering` pill
/// (learned/026) — not a terminal error — signals the degraded serve.
struct WhisperBatchActive {
    /// Buffer-only forwarder: collects every chunk into a local `Vec<i16>`
    /// and tracks peak amplitude, with no ASR socket to send to. Returns
    /// `(buffered_samples, peak_amplitude)` at release, exactly like the
    /// streaming forwarders, so the release path is provider-shape-agnostic.
    forwarder: JoinHandle<(Vec<i16>, i16)>,
    /// Signalled on release so the buffer-only forwarder transitions into
    /// its post-release drain window. Taken (consumed) on release.
    released_tx: Option<oneshot::Sender<()>>,
    /// Wall-clock instant the press started, for the silent/short-press
    /// gate shared with the audio-LID Whisper route and Deepgram rescue.
    pressed_at: Instant,
    /// The pool-open error that forced this route, threaded through to
    /// [`DictationSession::rescue_deepgram_route`] for accurate diagnostic
    /// logging + the terminal-failure telemetry `kind_of` (never surfaced
    /// to the user unless the Whisper/Gladia batch also fails).
    take_err: MuniError,
}

/// Outcome of the LID task, written into [`AutoDetectActive::decision`].
///
/// Exposed for integration tests in `tests/confidence_trigger.rs` that
/// drive [`DictationSession::spawn_confidence_trigger_task`] directly.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterDecision {
    /// LID detected English (or LID timed out / failed and we fell
    /// through to the safe default). Use the speculative Deepgram
    /// stream's transcript on release.
    Deepgram,
    /// LID detected a non-English language. The Deepgram client has
    /// already been closed by the LID task; on release we encode the
    /// buffered samples and POST to Whisper.
    Whisper,
}

impl RouterDecision {
    /// Stable short label used in `[lid]` log lines. `"deepgram"` /
    /// `"whisper"` — matches the press-routing log convention.
    pub fn as_log_str(&self) -> &'static str {
        match self {
            Self::Deepgram => "deepgram",
            Self::Whisper => "whisper",
        }
    }
}

/// Three-way press routing classification. Re-derived once at press
/// start from the live `english_fast_mode` flag and the availability
/// of the Whisper client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressRoute {
    /// Manual override: skip LID, stream straight to Deepgram.
    Deepgram,
    /// Default: speculatively stream to Deepgram while the LID task
    /// decides whether to switch to Whisper.
    AutoDetect,
}

impl PressRoute {
    /// Human-readable label for the `press routing:` log line. The
    /// `AutoDetect` enum variant is also the path the orchestrator
    /// uses when `bilingual_mode` is on (LID is skipped immediately
    /// and Whisper takes the press), so the label has to consider
    /// `bilingual_mode` to avoid the misleading `auto-detect` reading
    /// in that case — see `handle_hotkey_pressed` for the routing
    /// rules.
    fn as_log_str(&self, bilingual_mode: bool) -> &'static str {
        match self {
            Self::Deepgram => "deepgram (forced)",
            Self::AutoDetect if bilingual_mode => "whisper (forced, bilingual)",
            Self::AutoDetect => "auto-detect",
        }
    }
}

/// First-pass LID slice — 1.5 s of audio at 16 kHz. Short enough that
/// even a 2 s press sees an early decision; long enough that pure
/// Tagalog at the start clearly signals "switch to Whisper" before
/// the user finishes speaking.
const LID_SLICE_SAMPLES: usize = 24_000;

/// Second-pass LID slice — 3.5 s of audio. Only consulted when the
/// first pass returned English, to catch code-switched Taglish where
/// the speaker starts in English ("Actually, I think mas okay yung
/// first option…") and the 1.5 s slice was too English-heavy for
/// Whisper's LID to flag the Tagalog grammar particles. Sized so the
/// second slice contains enough Tagalog to swing the classification
/// (typically 2+ Tagalog words appear by 3.5 s in real Taglish
/// dictation).
///
/// 16 kHz × 3.5 s = 56 000 samples.
const LID_SLICE_SAMPLES_SECOND: usize = 56_000;

// ---- feature 020: local audio-LID windowing ----------------------------------

/// Sliding-window length for audio-LID — 2.0 s of 16 kHz mono PCM
/// (32 000 samples). Shorter than the text-LID `LID_SLICE_SAMPLES_SECOND`
/// (3.5 s) because local audio-LID classify is ~67 ms (M2 Pro, Metal)
/// vs text-LID's ~620 ms total; committing earlier protects the speed
/// budget. Longer than the 1.5 s pass#1 slice because the encoder
/// benefits from a little more context — the spike measured 2.0 s as
/// the sweet spot (≥95 % accuracy on clean monolingual).
const AUDIO_LID_WINDOW_SAMPLES: usize = 32_000;

/// How much fresh audio must accumulate between successive windows.
/// 1.0 s @ 16 kHz. Combined with `AUDIO_LID_WINDOW_SAMPLES`, the
/// effective stride is 1 s of new audio with 1 s of overlap from the
/// previous window — enough to smooth single-window noise without
/// hammering the encoder.
const AUDIO_LID_WINDOW_ADVANCE_SAMPLES: usize = 16_000;

/// Cap the rolling audio buffer at ~5 s of samples; we only ever read
/// the last `AUDIO_LID_WINDOW_SAMPLES` (2 s), so anything older is
/// dead weight. Keeps long presses from growing unbounded memory.
const AUDIO_LID_ROLLING_BUFFER_CAP_SAMPLES: usize = 80_000;

/// Env var overriding [`DEFAULT_AUDIO_LID_DRIFT_CONSECUTIVE`]. Number
/// of consecutive post-commit windows whose label disagrees with the
/// committed route before the windowing-drift detector fires
/// [`DictationSession::override_decision_deepgram_to_whisper`].
///
/// After feature 020 dogfood (2026-05-18) the override only fires in
/// the `deepgram → whisper` direction; the inverse was found to
/// silently lose press content (the Deepgram socket is already torn
/// down at commit time) and is permanently disabled.
pub const MUNI_AUDIO_LID_DRIFT_CONSECUTIVE_ENV: &str = "MUNI_AUDIO_LID_DRIFT_CONSECUTIVE";

/// Default number of consecutive disagreeing post-commit windows
/// required before the windowing-drift detector flips the route.
///
/// Restored from 1 → 2 after the 2026-05-18 round-2 dogfood.
/// History: this was originally 2, then briefly lowered to 1 after
/// round-1 dogfood (which showed real Taglish presses often had
/// only 1–2 windows of Tagalog signal before release, so requiring
/// 2 *consecutive* missed roughly half the affected presses).
/// Round 2 showed the bigger problem with `1` was the inverse:
/// whisper-tiny-q5_1 occasionally classifies a *mid-press English
/// window* as `taglish` (`p_tl ∈ [0.10, 0.25]`), and a threshold of
/// 1 flips clean English presses to Whisper on a single false
/// reading. Examples from round 2: "So I was thinking, maybe we
/// should hold off on the migration..." (pure English) and
/// "Let's schedule the team meeting..." both DRIFTed to Whisper on
/// one noisy mid-press window.
///
/// Since the feature 021 hybrid override now also catches the
/// late-Taglish recovery case (via the rolling-classify task —
/// independent of DRIFT), raising the threshold back to 2 is no
/// longer load-bearing for correctness: real Taglish presses get
/// caught either by audio-LID's first-window classification or by
/// the hybrid Groq classify. Drift is now a backstop, not the
/// primary signal — and a threshold of 2 trades a small accuracy
/// loss on edge-Taglish presses for eliminating the false-flip
/// latency penalty on clean English long presses.
const DEFAULT_AUDIO_LID_DRIFT_CONSECUTIVE: usize = 2;

/// Env var overriding [`DEFAULT_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR`].
/// Minimum value of the drift counter at hotkey release that fires the
/// `deepgram → whisper` override.
///
/// Backlog 0048 — bilingual presses shaped *English (≥3 s) → mid-press
/// silence (≥6 s) → short Tagalog tail (≤3 s) → release* produce only
/// 1–2 audio-LID windows before release. The drift counter reaches
/// `1/2` within ~160 ms of the release watch (`release_tx`) firing, so the mid-press
/// override (controlled by [`MUNI_AUDIO_LID_DRIFT_CONSECUTIVE_ENV`])
/// never lands its second consecutive disagreement. The release-time
/// fire floor is an independent knob that consumes partial drift
/// evidence at press end.
///
/// Default `1` fires the override on any non-zero drift counter at
/// release. Raising the floor (e.g. to a value above
/// [`DEFAULT_AUDIO_LID_DRIFT_CONSECUTIVE`]) effectively disables the
/// feature because the mid-press override would have already fired
/// before that drift count accumulated.
pub const MUNI_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR_ENV: &str =
    "MUNI_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR";

/// Default minimum drift counter value at hotkey release that fires
/// the `deepgram → whisper` override.
///
/// `1` means "any non-zero drift at press end fires the override" —
/// consuming the partial drift evidence that would otherwise be
/// discarded when the press terminates before the mid-press threshold
/// ([`DEFAULT_AUDIO_LID_DRIFT_CONSECUTIVE`] = 2) is reached.
///
/// Independence from mid-press calibration: this floor is read only by
/// the `released(&mut release_rx)` arm of [`DictationSession::run_audio_lid_pass`].
/// Mid-press drift behavior is unaffected — backlog 0047's false-positive
/// trade-off (which lives entirely on the mid-press axis) is not
/// pulled against by this knob.
const DEFAULT_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR: usize = 1;

/// Backlog 0048 v2 — env knob controlling whether the orchestrator's
/// at-release dispatch treats a post-commit `Other(_)` verdict (whisper-tiny
/// top1 ∉ {en, tl}) as evidence to fire the `deepgram → whisper`
/// override.
///
/// Default ON for the dogfood-2026-05-21 use case: the user is a
/// Filipino speaker who only dictates English or Taglish, so any
/// non-English audio-LID verdict at the tail of a press is almost
/// certainly mis-classified Tagalog (the language whisper-tiny-q5_1
/// hallucinates most often as `id`/`ru`/`es`). Setting to `off`
/// reverts to v1 behavior (drift-counter rule only).
///
/// Scope is intentionally release-time only: mid-press, `IgnoreNoise`
/// continues to preserve the drift counter so a single cough/breath
/// doesn't accumulate evidence. The new rule only consumes the LAST
/// classified post-commit verdict's label, read at release time.
pub const MUNI_AUDIO_LID_RELEASE_OTHER_AS_TAGLISH_ENV: &str =
    "MUNI_AUDIO_LID_RELEASE_OTHER_AS_TAGLISH";

/// Default value of the [`MUNI_AUDIO_LID_RELEASE_OTHER_AS_TAGLISH_ENV`]
/// knob. `true` enables the rule, matching the personal-dictation
/// use case where the user only speaks English or Taglish.
const DEFAULT_AUDIO_LID_RELEASE_OTHER_AS_TAGLISH: bool = true;

/// Backlog 0052 — env knob enabling the symmetric hybrid text-LID
/// veto over audio-LID drift overrides. When `on` (default), a
/// recent hybrid text-LID verdict of `English` (or `Other`) blocks
/// both the mid-press drift `FireOverrideToWhisper` action and the
/// at-release stale-drift `FireOverrideToWhisper` action. When `off`,
/// behavior degenerates to pre-feat/028 (drift overrides fire
/// regardless of hybrid verdict, matching feat/021's original
/// asymmetric design).
///
/// Parses `on`/`off`/`true`/`false`/`1`/`0` case-insensitively.
pub const MUNI_AUDIO_LID_HYBRID_VETO_DRIFT_ENV: &str = "MUNI_AUDIO_LID_HYBRID_VETO_DRIFT";

/// Default value of [`MUNI_AUDIO_LID_HYBRID_VETO_DRIFT_ENV`]. `true`
/// enables the symmetric veto — the dogfood-validated behavior from
/// backlog 0052. Setting the env knob to `off` rolls back to feat/021's
/// asymmetric direction without a rebuild.
const DEFAULT_AUDIO_LID_HYBRID_VETO_DRIFT: bool = true;

// ---- feature 021: hybrid audio + text-LID secondary -----------------------
//
// Provider-agnostic naming: the secondary text-LID classifier is
// chosen at boot by `build_audio_hybrid_secondary_classifier` in
// `lib.rs`. The original 2026-05-18 plan was Gemini Flash-Lite but
// post-implementation dogfood showed Gemini's classify latency tail
// (~1.7 s median, 4 s p95) was too slow for the audio-LID-side
// budget, so the default flipped to Groq (`openai/gpt-oss-120b`,
// ~300 ms median). These constants don't know or care which
// provider is wired — keep them generic so a future provider swap
// doesn't drag a churn of renames.

/// Length of the rolling audio slice the hybrid task classifies on
/// each interval. 3.0 s @ 16 kHz = 48 000 samples. Sized longer than
/// [`AUDIO_LID_WINDOW_SAMPLES`] (2 s) because Whisper transcribe is
/// more reliable on slightly longer slices, and the text-LID
/// classifier's accuracy on the resulting transcript improves with
/// more bilingual content per slice.
const AUDIO_HYBRID_SLICE_SAMPLES: usize = 48_000;

/// Re-classify cadence — how much fresh audio must accumulate before
/// the hybrid task fires another transcribe+classify pass. 3.0 s @
/// 16 kHz. Matches the slice length so the rolling window has zero
/// overlap — each press's audio is observed exactly once by the
/// hybrid task (modulo the always-fresh tail of the most-recent
/// slice).
const AUDIO_HYBRID_CLASSIFY_INTERVAL_SAMPLES: usize = 48_000;

/// Cap the hybrid task's rolling buffer. 6 s @ 16 kHz. One slice
/// plus a little slack for the boundary case where the press
/// just-released as a new slice is being snapshotted.
const AUDIO_HYBRID_BUFFER_CAP_SAMPLES: usize = 96_000;

/// Feature 021 round-4 fix 2026-05-18 — silence padding (in samples)
/// appended to each hybrid Whisper slice. 0.5 s @ 16 kHz = 8 000
/// zero samples.
///
/// Dogfood found Whisper-large-v3 truncating 3 s Taglish slices to
/// just the leading English prefix (e.g. "The thing is" from
/// "The thing is, hindi pa fully tested yung change."). The slice
/// boundary cuts mid-word and Whisper appears to interpret that as
/// an incomplete utterance and stops decoding early. Appending a
/// short trailing silence signals "the speech is complete" — empirical
/// finding from other open-source Whisper deployments — and reduces
/// the truncation rate. Padding is hybrid-path-only: the main batch
/// transcribe on release uses the full press's audio and doesn't
/// need padding.
const AUDIO_HYBRID_TRAILING_SILENCE_SAMPLES: usize = 8_000;

/// First-window `p_en` floor at or above which audio-LID is treated
/// as confident enough to skip the hybrid task. Below this floor
/// (or when the label is `Other` / `Tagalog` / `Taglish`) the
/// hybrid task spawns.
///
/// Raised from 0.70 → 0.90 after the 2026-05-18 dogfood round 2.
/// Round 1 calibration set it at 0.70 against `p_en ∈ [0.30, 0.85]`
/// failures, but round 2 surfaced two specific phrases — "The thing
/// is, hindi pa fully tested yung change." (`p_en=0.76`) and "Yung
/// deadline ay next Friday, tama ba?" (`p_en=0.66`-`0.77`
/// depending on take) — that audio-LID misread as confident English
/// while still being real Taglish. The previous 0.70 threshold
/// blocked the hybrid from spawning, the press committed Deepgram,
/// and the Tagalog content was dropped. Lifting the floor to 0.90
/// gives the hybrid a chance to spawn on the `[0.70, 0.90)` band
/// where audio-LID's confidence is real-but-not-overwhelming.
///
/// Cost: clean strong-English presses with `p_en ∈ [0.70, 0.90)`
/// now spawn the hybrid (one extra Groq Whisper + Groq LID call
/// each). About half of clean English presses fall in this band in
/// observed dogfood — so the per-press cost rises by roughly half a
/// `(transcribe + classify)` call on average over clean English, or
/// about +$0.0001 averaged. Truly confident English (`p_en ≥ 0.90`)
/// still skips the hybrid entirely.
const CONFIDENCE_TO_SKIP_HYBRID_TASK: f32 = 0.90;

/// Press-duration threshold beyond which the hybrid task is spawned
/// even if audio-LID's first window was confident English. Catches
/// the late-Tagalog case (long press starting in English then
/// switching to Tagalog) which the first-window-only heuristic
/// misses entirely. 5 s @ 16 kHz.
const MIN_PRESS_DURATION_FOR_LATE_TAGLISH_RECOVERY_SAMPLES: usize = 80_000;

// ---- feature 019: confidence-triggered mid-press LID re-pass ---------------

/// Env var that gates the entire feature-019 mid-press LID re-pass.
/// When unset or falsy, the trigger task is never spawned and the
/// Deepgram client is never asked to install a confidence continuation.
pub const MUNI_LID_CONFIDENCE_TRIGGER_ENV: &str = "MUNI_LID_CONFIDENCE_TRIGGER";

/// Env var overriding [`DEFAULT_CONFIDENCE_TRIGGER_THRESHOLD`].
pub const MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD_ENV: &str = "MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD";

/// Env var overriding [`DEFAULT_CONFIDENCE_TRIGGER_CONSECUTIVE`].
pub const MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE_ENV: &str =
    "MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE";

/// Env var overriding the slice length in seconds; converted to
/// samples at 16 kHz before reaching the trigger task.
pub const MUNI_LID_CONFIDENCE_TRIGGER_SLICE_SECONDS_ENV: &str =
    "MUNI_LID_CONFIDENCE_TRIGGER_SLICE_SECONDS";

/// Env var overriding the release-drain window in milliseconds. The
/// release path sends Deepgram a `Finalize` control message and waits
/// this long for pending confidence events to flow through the trigger
/// task before committing the route. Set to `0` to disable the drain
/// entirely (then the trigger can only fire on events that arrived
/// before release, which is timing-dependent on Deepgram's
/// `endpointing` silence-gap behavior).
pub const MUNI_LID_CONFIDENCE_TRIGGER_DRAIN_MS_ENV: &str = "MUNI_LID_CONFIDENCE_TRIGGER_DRAIN_MS";

/// Default per-chunk confidence threshold below which a chunk counts
/// as "low confidence". Overridable via
/// [`MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD_ENV`].
const DEFAULT_CONFIDENCE_TRIGGER_THRESHOLD: f32 = 0.70;

/// Default number of consecutive low-confidence chunks that must
/// arrive before the trigger fires the re-pass. Overridable via
/// [`MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE_ENV`].
const DEFAULT_CONFIDENCE_TRIGGER_CONSECUTIVE: usize = 1;

/// Default re-pass slice size in samples — 3.0 s at 16 kHz.
/// Overridable as seconds via
/// [`MUNI_LID_CONFIDENCE_TRIGGER_SLICE_SECONDS_ENV`].
const DEFAULT_CONFIDENCE_TRIGGER_SLICE_SAMPLES: usize = 48_000;

/// Hard cap on the rolling buffer; never grow beyond this (~4.0 s at
/// 16 kHz). Sized slightly larger than the default slice so we can
/// snapshot a full slice with a little slack for chunk boundaries.
const CONFIDENCE_TRIGGER_ROLLING_BUFFER_CAP_SAMPLES: usize = 64_000;

/// Maximum time [`DictationSession::finalize_auto_detect`] will wait
/// for an in-flight confidence-trigger re-pass to land before
/// committing the route. Only applies when the trigger has set
/// [`AutoDetectActive::trigger_inflight`] — pure-English presses
/// (the common case) skip this wait entirely.
///
/// Sized to cover the p95 of `transcribe_for_lid` + `classify_text_only`
/// for the 3 s re-pass slice (≈700 ms median, ≈1300 ms p95 from
/// existing pass#1/pass#2 logs). The cost is a worst-case +1500 ms on
/// a press where release lands exactly as the trigger fires;
/// alternative is silently dropping the post-switch Tagalog tail.
const TRIGGER_REPASS_WAIT_MS: u64 = 1_500;

/// Default release-drain window — wait this long after sending Deepgram a
/// `Finalize` for any pending low-confidence finals to flow through the
/// trigger task before committing the route.
///
/// Dogfood measurements (2026-05-15) showed Deepgram's Finalize-to-final
/// latency at ~460–520 ms when flushing buffered continuous speech (the
/// case where the user is mid-sentence at release, no `endpointing=300`
/// silence gap detected yet). Sized at 600 ms to cover that p95 with
/// ~80–140 ms margin; if a future variance run shows the upper bound
/// drifting higher, bump via env var without recompiling.
///
/// Overridable via [`MUNI_LID_CONFIDENCE_TRIGGER_DRAIN_MS_ENV`]; set to
/// `0` to disable the drain entirely (then trigger fires only on
/// chunks that arrived *before* release, which is timing-flaky on
/// continuous speech — see `docs/qa/016_*` Scenario 3 retry data).
const DEFAULT_CONFIDENCE_TRIGGER_DRAIN_MS: u64 = 600;

/// Sanity cap on the drain override — values above this are rejected and
/// fall back to default. Prevents a mistyped env var from adding seconds
/// of latency to every armed press.
const CONFIDENCE_TRIGGER_DRAIN_MS_MAX: u64 = 2_000;

/// Minimum samples the rolling buffer must contain before the trigger
/// will attempt a re-pass. 1.0 s — below this a Whisper transcribe
/// returns essentially nothing useful and the call is wasted.
const CONFIDENCE_TRIGGER_MIN_REPASS_SAMPLES: usize = 16_000;

/// Lower / upper bounds for `MUNI_LID_CONFIDENCE_TRIGGER_SLICE_SECONDS`.
/// Anything outside these bounds is clamped back to default with a
/// warning so a mistyped env var can't degenerate into a useless
/// re-pass (too short → empty transcript; too long → blows past the
/// buffer cap).
const CONFIDENCE_TRIGGER_SLICE_SECONDS_MIN: f32 = 0.5;
const CONFIDENCE_TRIGGER_SLICE_SECONDS_MAX: f32 = 10.0;

/// Bounded ring-style buffer of i16 audio samples used by the
/// confidence-trigger task. Drops oldest samples when `push` would
/// exceed `cap_samples`. Snapshots return a contiguous `Vec<i16>` so
/// callers can hand the slice to [`GroqWhisperClient::transcribe`].
struct RollingBuffer {
    buf: std::collections::VecDeque<i16>,
    cap_samples: usize,
}

impl RollingBuffer {
    fn new(cap_samples: usize) -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(cap_samples),
            cap_samples,
        }
    }

    fn push(&mut self, chunk: &[i16]) {
        // If a single push exceeds the cap, retain only the tail. This
        // keeps the buffer bounded even when the broadcast subscriber
        // delivers a backlog burst that's individually larger than
        // `cap_samples` (extremely rare in production, but the
        // alternative — unbounded growth — would leak memory).
        if chunk.len() >= self.cap_samples {
            self.buf.clear();
            let start = chunk.len() - self.cap_samples;
            self.buf.extend(chunk[start..].iter().copied());
            return;
        }
        // Trim leading samples to make room.
        let overflow = (self.buf.len() + chunk.len()).saturating_sub(self.cap_samples);
        for _ in 0..overflow {
            self.buf.pop_front();
        }
        self.buf.extend(chunk.iter().copied());
    }

    fn snapshot_last_n_samples(&self, n: usize) -> Vec<i16> {
        let take = n.min(self.buf.len());
        let start = self.buf.len() - take;
        self.buf.iter().skip(start).copied().collect()
    }

    fn len(&self) -> usize {
        self.buf.len()
    }
}

// ----------------------------------------------------------------------------

/// Maximum time `handle_hotkey_released` waits for the LID task to
/// complete after release. If LID hasn't decided by then the press
/// defaults to the **Whisper** path (per feature 003's failure
/// rule: Whisper handles both English and Tagalog correctly, so a
/// stuck LID call should not silently send a possibly-Tagalog press
/// to Deepgram). The earlier behaviour (default → Deepgram) was
/// safe only when the user was guaranteed to be speaking English,
/// which is exactly the assumption feature 003 invalidates.
///
/// Sized below the LID classifier's own timeout so an in-flight LID
/// call has a chance to return. 1000 ms is sized to cover the
/// Whisper-transcribe-turbo floor (~0.95 s wall after press start)
/// on short presses; with Groq classify ~150 ms median the LID floor
/// is dominated by transcribe, not classify. Bumped from 500 ms in
/// backlog 0009 so short English presses ("sure", "sure, go ahead")
/// land on Deepgram instead of timing out into Whisper batch.
const RELEASE_LID_WAIT: Duration = Duration::from_millis(1000);

/// Wait up to `grace` for the LID task to commit a decision, then
/// return whether the notify fired plus the current snapshot of the
/// decision cell.
///
/// Snapshot fast-path: re-reads the decision cell on entry. If the
/// LID task already committed (typical for short non-English presses
/// where pass#1 fires during the press), `notify_waiters` had no
/// registered waiters and the notification was lost — without the
/// fast-path the caller would block the full grace window for a
/// notification that already fired. Returns `(true, snapshot)` to
/// preserve the caller's "notified ⇒ result is ready" invariant.
///
/// Re-reads the snapshot after the timeout fires so a decision that
/// races the deadline (set just as the timer expires) is still picked
/// up. The caller treats `(notified=false, snapshot=None)` as the
/// "LID not ready" path.
///
/// Extracted so the 500–1000 ms grace-window fix from backlog 0009
/// can be regression-tested without spinning up the full session
/// orchestrator (`finalize_auto_detect_grace_window_admits_700ms_decision`).
async fn wait_for_decision(
    notify: &Notify,
    decision: &TokioMutex<Option<RouterDecision>>,
    grace: Duration,
) -> (bool, Option<RouterDecision>) {
    {
        let snapshot = *decision.lock().await;
        if snapshot.is_some() {
            return (true, snapshot);
        }
    }
    let notified = tokio::time::timeout(grace, notify.notified()).await.is_ok();
    let snapshot = *decision.lock().await;
    (notified, snapshot)
}

/// Per-press shared coordination state (plan 039 task 13).
///
/// Bundles the `Arc`/`watch` handles that were previously passed as a long
/// stack of individual arguments (and forced a `#[allow(clippy::too_many_arguments)]`)
/// through the audio-LID spawn chain: [`DictationSession::run_audio_lid_pass`]
/// → [`DictationSession::try_spawn_audio_hybrid`] →
/// [`DictationSession::spawn_audio_hybrid_task`]. Passing one bundle removes the
/// duplicated arg stacks and the whole class of "args silently reordered" bugs.
///
/// `Clone` is cheap — every field is an `Arc` or a `watch::Sender` (both
/// reference-counted). Consumers clone only the fields they use out of the
/// bundle at function entry, leaving the bodies unchanged.
#[derive(Clone)]
struct PressShared {
    decision: Arc<TokioMutex<Option<RouterDecision>>>,
    decision_notify: Arc<Notify>,
    release_tx: watch::Sender<bool>,
    committed: Arc<AtomicBool>,
    aborted: Arc<AtomicBool>,
    audio_hybrid_inflight: Arc<AtomicUsize>,
    audio_hybrid_recent_text_lid_english: Arc<AtomicBool>,
    audio_lid_drift_counter: Arc<AtomicUsize>,
    audio_lid_last_post_commit_was_other: Arc<AtomicBool>,
}

/// Coordinates a spawned delivery (plan 039 task 25) with the driver loop
/// and its sibling deliveries.
///
/// After finalize produces a transcript, `handle_hotkey_released` runs
/// cleanup + paste + history in a detached task so the driver loop can
/// dequeue the next press and start capture immediately. Two things must
/// still be coordinated across those concurrent tasks, and this bundle
/// threads both from `handle_hotkey_released` through `run_groq_cleanup`
/// into `deliver_final`:
///
/// * `order` — the previous delivery's completion signal. `deliver_final`
///   awaits it right before pasting, so pastes land in strict press order
///   even though the (slow) Groq cleanup runs concurrently. A dropped
///   sender (predecessor panicked/aborted) resolves immediately — the
///   chain can never deadlock.
/// * `epoch` — the owning press's HUD epoch. Terminal HUD transitions are
///   suppressed when a newer press has taken the HUD (recording wins).
///
/// `immediate()` is the direct/unit-test shape: no predecessor to wait on
/// and no supersede guard (a single press in flight).
struct DeliveryContext {
    order: Option<oneshot::Receiver<()>>,
    epoch: Option<u64>,
    /// Plan 041 (wave 1) — release timestamp (`press_t0`) so the delivery
    /// tail can compute `total_ms = press_t0.elapsed()` at paste-delivered.
    press_t0: Instant,
    /// Plan 041 (wave 1) — the release-time timing skeleton, filled with
    /// `cleanup_ms` by `run_groq_cleanup` and `inject_ms`/`total_ms` by
    /// `deliver_final`, then logged + persisted.
    timing: PressTiming,
}

impl DeliveryContext {
    /// The direct/unit-test shape: no predecessor to wait on and no
    /// supersede guard (a single press in flight). Production always builds
    /// a real context in `handle_hotkey_released`.
    #[cfg(test)]
    fn immediate() -> Self {
        Self {
            order: None,
            epoch: None,
            press_t0: Instant::now(),
            timing: PressTiming::default(),
        }
    }

    /// Block until the predecessor delivery has finished (or its sender was
    /// dropped). Consumes the gate so a second call is a no-op.
    async fn await_turn(&mut self) {
        if let Some(rx) = self.order.take() {
            // Err == predecessor's guard dropped without an explicit send
            // (task completed or was dropped); either way it's done.
            let _ = rx.await;
        }
    }
}

/// Fires the successor delivery's order gate when a delivery task ends —
/// on the normal return path AND across a panic or a dropped future
/// (learned/013 drop-guard discipline). Without the `Drop` guarantee a
/// delivery that panicked mid-cleanup would strand every later press's
/// paste behind a gate that never opens.
struct DeliveryDoneGuard {
    tx: Option<oneshot::Sender<()>>,
    epoch: u64,
}

impl Drop for DeliveryDoneGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            // Send fails only if the successor was dropped before it began
            // waiting — harmless; there's no one left to unblock.
            let _ = tx.send(());
            log::debug!(
                target: "session",
                "delivery epoch {} released the paste-order gate",
                self.epoch
            );
        }
    }
}

/// Await the per-press release signal (plan 039 task 13).
///
/// Backed by a `tokio::sync::watch<bool>` seeded `false` and flipped `true`
/// once by `finalize_auto_detect`. This replaces the old `Notify` +
/// `notify_one()`, which could only wake ONE of the concurrent LID/hybrid
/// waiters — the other starved until the 1 s `RELEASE_LID_WAIT` default. `watch`
/// gives both properties at once:
///
/// * **broadcast** — every subscribed receiver observes the flip, so the
///   audio-LID pass AND the hybrid task both wake on a single fire.
/// * **sticky** — a receiver that subscribes (or first checks) *after* the flip
///   still sees `true` immediately, covering the "release fires between pass#1
///   and pass#2 registration" window the old `notify_one` permit handled.
///
/// A dropped sender (torn-down press) resolves as released so a waiter can
/// never hang here.
async fn released(rx: &mut watch::Receiver<bool>) {
    if *rx.borrow_and_update() {
        return;
    }
    loop {
        if rx.changed().await.is_err() {
            return; // sender dropped — treat as released
        }
        if *rx.borrow_and_update() {
            return;
        }
    }
}

/// Longest a single `decision_notify` wait blocks before the hybrid-inflight
/// loop re-checks the counter. Bounds the worst-case stall from a lost wakeup
/// (`Notify::notify_waiters` drops a signal fired in the gap between the
/// counter check and the next await registration) to one slice, while a signal
/// fired *during* the await still wakes the loop immediately.
const HYBRID_INFLIGHT_POLL: Duration = Duration::from_millis(250);

/// Wait up to `budget` for an in-flight audio-LID-hybrid inner classify to flip
/// the route to Whisper (plan 039 task 14).
///
/// A single `decision_notify` firing only signals ONE inner-classify
/// completion — and the english/Other `InflightGuard::drop` guards fire it too,
/// not just a flipping override. Waiting on it exactly once therefore abandons a
/// *trailing* `taglish` classify still in flight whenever a *leading* english
/// classify wakes the waiter first (dogfood round-6: trailing verdicts landed
/// 200–700 ms after the leading one). This loops the wait within the budget,
/// re-checking after every wake, and returns as soon as the route flips to
/// Whisper, all inner classifies drain (`inflight == 0`), or the budget is
/// exhausted. Returns `true` iff the decision cell is `Whisper` on return.
async fn await_hybrid_inflight_flip(
    decision: &TokioMutex<Option<RouterDecision>>,
    decision_notify: &Notify,
    inflight: &AtomicUsize,
    budget: Duration,
) -> bool {
    let started = Instant::now();
    loop {
        // A flip that already landed (or lands in a prior gap) wins immediately.
        if matches!(*decision.lock().await, Some(RouterDecision::Whisper)) {
            return true;
        }
        // Every inner classify finished without flipping — nothing left to await.
        if inflight.load(Ordering::SeqCst) == 0 {
            return false;
        }
        let remaining = match budget.checked_sub(started.elapsed()) {
            Some(r) if !r.is_zero() => r,
            _ => return false,
        };
        // Cap the wait at one poll slice so a lost wakeup can stall at most
        // `HYBRID_INFLIGHT_POLL`; a notify fired during the await wakes early.
        let slice = remaining.min(HYBRID_INFLIGHT_POLL);
        let _ = tokio::time::timeout(slice, decision_notify.notified()).await;
    }
}

/// After this many **consecutive timeout-flavored** send failures the
/// forwarder treats the socket as wedged (a half-open connection whose
/// kernel TCP buffer is full) and abandons sending — far sooner than the
/// 30-failure fast-error cap.
///
/// Rationale (plan 039 slice 4, task 9): each timeout-flavored failure is a
/// bounded write that already burned a full `deepgram::SEND_TIMEOUT` (1 s)
/// blocked on the wedged buffer. Riding the 30-cap here would stall the
/// forwarder for ~30 s before the Whisper/Gladia rescue could run. Two
/// consecutive 1 s timeouts (~2 s) is enough signal that the socket is dead;
/// we keep buffering locally so the rescue still has the full press audio.
/// The 30-cap stays for *fast* (instant) non-timeout errors, where 30
/// failures is only ~0.5 s of a confirmed-dead socket at cpal's ~60 Hz.
const MAX_CONSECUTIVE_SEND_TIMEOUTS: usize = 2;

/// True when a forwarder send failure came from the bounded-write **timeout**
/// (a wedged half-open socket) rather than a fast synchronous failure
/// (closed stream, tungstenite protocol error).
///
/// [`asr_stream::send_frame_timed`] stamps the timeout branch's reason with
/// `"… timed out after …"`; fast failures carry the tungstenite error string
/// or `"stream closed"` instead. Classifying by that marker lets the forwarder
/// bail after [`MAX_CONSECUTIVE_SEND_TIMEOUTS`] wedged writes while keeping the
/// larger fast-error cap for genuinely instant failures.
fn is_send_timeout(err: &MuniError) -> bool {
    matches!(
        err,
        MuniError::DeepgramConnectionFailed { reason }
        | MuniError::GladiaConnectionFailed { reason }
            if reason.contains("timed out")
    )
}

/// Minimal audio-sink abstraction the release forwarders stream through.
///
/// Production presses use [`DeepgramClient`]; the forwarder unit tests inject
/// a mock that returns timeout- vs fast-flavored errors on demand, so the
/// consecutive-failure caps (and the buffer-before-send guarantee) are
/// exercised deterministically without a wedged TCP socket. The trait is
/// monomorphised at every call site — no dynamic dispatch on the hot path.
pub(crate) trait ReleaseSink {
    /// Stream one PCM chunk to the ASR socket. Mirrors
    /// [`DeepgramClient::send`]'s bounded-write contract: `Ok(())` on a
    /// successful (possibly buffered) write, a timeout-flavored `Err` when the
    /// bounded write elapsed, or a fast `Err` when the socket is already dead.
    fn send_chunk(
        &self,
        chunk: &[i16],
    ) -> impl std::future::Future<Output = Result<(), MuniError>> + Send;
}

impl ReleaseSink for DeepgramClient {
    fn send_chunk(
        &self,
        chunk: &[i16],
    ) -> impl std::future::Future<Output = Result<(), MuniError>> + Send {
        self.send(chunk)
    }
}

/// Forwarder used by the AutoDetect path. Mirrors
/// [`forward_chunks_until_release`] (streams to Deepgram, tracks peak,
/// honours the post-release drain) BUT additionally copies every
/// chunk into a local buffer and respects an `aborted` flag — once
/// the LID task signals "switch to Whisper" the forwarder stops
/// sending to Deepgram but keeps collecting samples so the Whisper
/// path has the full press's audio.
async fn forward_and_buffer_until_release<S: ReleaseSink + Send + Sync>(
    client: Arc<S>,
    mut chunks_rx: broadcast::Receiver<Vec<i16>>,
    mut released_rx: oneshot::Receiver<()>,
    aborted: Arc<AtomicBool>,
) -> (Vec<i16>, i16) {
    const MAX_CONSECUTIVE_SEND_FAILURES: usize = 30;
    let mut release_seen = false;
    let mut peak: i16 = 0;
    let mut buffer: Vec<i16> = Vec::new();
    let mut consecutive_send_failures = 0usize;
    let mut consecutive_send_timeouts = 0usize;
    loop {
        tokio::select! {
            biased;
            c = chunks_rx.recv() => match c {
                Ok(chunk) => {
                    for &sample in &chunk {
                        let mag = sample.saturating_abs();
                        if mag > peak {
                            peak = mag;
                        }
                    }
                    // Buffer BEFORE the network send: `client.send` can block
                    // (and ultimately time out) on a wedged socket, and a chunk
                    // lost to that backpressure can never be replayed to the
                    // Whisper/Gladia rescue. Local buffering is the source of
                    // truth for the press audio; the socket is best-effort.
                    buffer.extend_from_slice(&chunk);

                    if aborted.load(Ordering::SeqCst) {
                        // LID picked Whisper — keep collecting but
                        // don't send to a socket we've already torn
                        // down.
                        continue;
                    }

                    match client.send_chunk(&chunk).await {
                        Ok(()) => {
                            consecutive_send_failures = 0;
                            consecutive_send_timeouts = 0;
                        }
                        Err(err) => {
                            consecutive_send_failures += 1;
                            if is_send_timeout(&err) {
                                consecutive_send_timeouts += 1;
                            } else {
                                consecutive_send_timeouts = 0;
                            }
                            log::warn!(
                                target: "deepgram",
                                "send failed ({}/{} fast, {}/{} timeout): {}",
                                consecutive_send_failures,
                                MAX_CONSECUTIVE_SEND_FAILURES,
                                consecutive_send_timeouts,
                                MAX_CONSECUTIVE_SEND_TIMEOUTS,
                                err.user_message()
                            );
                            if consecutive_send_timeouts >= MAX_CONSECUTIVE_SEND_TIMEOUTS
                                || consecutive_send_failures >= MAX_CONSECUTIVE_SEND_FAILURES
                            {
                                // Give up on the socket but keep
                                // buffering — Whisper fallback may
                                // still salvage the press. A wedged
                                // half-open socket trips the timeout cap
                                // (~2 s); an instant-fail socket trips
                                // the fast-error cap (~0.5 s).
                                aborted.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    log::warn!(target: "asr", "audio chunks lagged by {skipped}");
                }
                Err(RecvError::Closed) => break,
            },
            r = &mut released_rx, if !release_seen => {
                let _ = r;
                release_seen = true;
            },
            () = tokio::time::sleep(Duration::from_millis(POST_RELEASE_DRAIN_MS)),
                if release_seen => break,
        }
    }
    (buffer, peak)
}

/// Buffer-only forwarder for the pool-outage capture fallback (plan 039
/// task 26). Collects every chunk into a local buffer and tracks peak
/// amplitude, honouring the same release + [`POST_RELEASE_DRAIN_MS`] drain
/// contract as [`forward_and_buffer_until_release`] — but with NO ASR
/// socket, because the Deepgram pool was down at press start. The buffer is
/// the sole source of truth for the press audio; on release it is replayed
/// through the Groq Whisper → Gladia batch chain.
async fn buffer_until_release(
    mut chunks_rx: broadcast::Receiver<Vec<i16>>,
    mut released_rx: oneshot::Receiver<()>,
) -> (Vec<i16>, i16) {
    let mut release_seen = false;
    let mut peak: i16 = 0;
    let mut buffer: Vec<i16> = Vec::new();
    loop {
        tokio::select! {
            biased;
            c = chunks_rx.recv() => match c {
                Ok(chunk) => {
                    for &sample in &chunk {
                        let mag = sample.saturating_abs();
                        if mag > peak {
                            peak = mag;
                        }
                    }
                    buffer.extend_from_slice(&chunk);
                }
                Err(RecvError::Lagged(skipped)) => {
                    log::warn!(target: "asr", "audio chunks lagged by {skipped}");
                }
                Err(RecvError::Closed) => break,
            },
            r = &mut released_rx, if !release_seen => {
                let _ = r;
                release_seen = true;
            },
            () = tokio::time::sleep(Duration::from_millis(POST_RELEASE_DRAIN_MS)),
                if release_seen => break,
        }
    }
    (buffer, peak)
}

impl DictationSession {
    pub fn new(deps: SessionDeps) -> Arc<Self> {
        Arc::new(Self {
            deps,
            active: TokioMutex::new(None),
            press_epoch: AtomicU64::new(0),
            delivery_order_tail: std::sync::Mutex::new(None),
        })
    }

    /// Wire the orchestrator to the live hotkey + audio sources.
    ///
    /// Spawns one long-running task that drives press → release → press in
    /// strict sequence. Audio capture is started/stopped by this driver so
    /// the orchestrator's per-press methods stay free of cpal threads.
    ///
    /// Plan 030 — `press_rx` is parameterised on [`HotkeyMode`] so the
    /// state machine signals which gesture started the session
    /// (`Ptt` vs `ToggleLocked`). `release_rx` is parameterised on
    /// [`ReleaseKind`] so the driver dispatches a normal commit vs an
    /// explicit cancel (Esc) onto the matching orchestrator path. The
    /// 1:1 press:release invariant — each iteration consumes exactly
    /// one release event regardless of payload — is preserved.
    pub fn spawn_driver(
        self: Arc<Self>,
        audio: Arc<AudioCapture>,
        debug_dir: Option<PathBuf>,
        mut press_rx: broadcast::Receiver<HotkeyMode>,
        mut release_rx: broadcast::Receiver<ReleaseKind>,
        silence_threshold: Duration,
        silence_signaler: Arc<dyn Fn() + Send + Sync>,
    ) {
        tauri::async_runtime::spawn(async move {
            // Monotonic per-capture-attempt counter. Bumped every time the driver
            // spawns an `AudioCapture::start` probe; a late-completing wedged
            // probe compares its stamped value against the live counter to decide
            // whether the mic it just opened is orphaned (stop it) or belongs to a
            // newer press that already took over (leave it). See
            // [`spawn_late_capture_stop`].
            let capture_generation = Arc::new(AtomicU64::new(0));
            // True when the previous iteration's `wait_for_release`
            // timed out instead of observing a real release (plan 039
            // task 27). The OS may still deliver that orphaned release
            // later (the user lifted the modifier eventually); when it
            // does we must ignore it so it doesn't satisfy the next
            // press's release-wait ~50 ms after start. Rather than
            // draining the broadcast at the iteration boundary — which
            // misses a release the OS redelivers a beat *after* the drain
            // — we tag the orphan as owed debt and let the realigning wait
            // discard it by identity whenever it arrives (backlog 0003 +
            // manual-QA §12 stale-release pathology).
            //
            // `stale_release_deadline` bounds how long we keep discarding:
            // a genuinely-lost release never arrives, so past the catch-up
            // window we stop discarding and later presses wait normally. The
            // deadline is passed *into* the realigning wait rather than
            // consulted here, so that expiry can never fire before any
            // already-buffered orphan has been burned against the debt (a
            // catch-up expiry that cleared the debt while a synthetic Commit
            // was still queued would let that orphan collapse the next press —
            // plan 039 task 27).
            //
            // The debt is a count, not a per-press identity — see
            // `STALE_RELEASE_CATCHUP` for the two accepted residuals this
            // leaves (a re-press within the window can have its real release
            // swallowed; an orphan arriving after the window can satisfy a
            // later press). Both need the rare dropped-OS-event pathology.
            // `press_generation` is a monotonic tag used only to name the
            // orphaned press in logs; closing the residuals by identity would
            // mean plumbing it through the release payload (deferred).
            let mut stale_release_debt: u32 = 0;
            let mut stale_release_deadline: Option<Instant> = None;
            let mut press_generation: u64 = 0;
            loop {
                match press_rx.recv().await {
                    Ok(mode) => {
                        press_generation += 1;

                        // Audio start is synchronous (a cpal device probe). Run
                        // it on a blocking task and AWAIT the result: a
                        // capture-start failure (mic denied, no input device, or
                        // a cpal stream-build error) must short-circuit the press
                        // and surface the typed error, instead of proceeding to a
                        // fake `Listening` pill over a dead mic (plan 039 task
                        // 32). Task 32 requires knowing the capture result BEFORE
                        // the pill, so this probe is necessarily on the press
                        // critical path now (before task 32 it ran detached, in
                        // parallel with the pill + Deepgram checkout); on a slow
                        // Bluetooth/USB mic the user pays the device-init latency
                        // before Listening shows — the deliberate cost of not
                        // flashing a pill over a mic that never opened. The await
                        // is bounded by `CAPTURE_START_TIMEOUT` so a *wedged*
                        // CoreAudio open can never permanently stall the driver
                        // loop (which would kill every later press).
                        let audio_clone = audio.clone();
                        let debug_dir_clone = debug_dir.clone();
                        // Stamp this capture attempt so a late-completing wedged
                        // probe can tell whether it is still the current press (and
                        // its mic must be stopped) or has been superseded by a
                        // newer press that legitimately re-opened capture.
                        let capture_gen = capture_generation.fetch_add(1, Ordering::SeqCst) + 1;
                        let mut start_task = tauri::async_runtime::spawn_blocking(move || {
                            audio_clone.start(debug_dir_clone)
                        });
                        // Bind the timeout result in its OWN statement so the
                        // `&mut start_task` borrow is released before the match
                        // arms run — the timeout arm needs to MOVE `start_task`
                        // into the late-stop watcher (see below).
                        let start_outcome =
                            tokio::time::timeout(CAPTURE_START_TIMEOUT, &mut start_task).await;
                        let capture_started = match start_outcome {
                            Ok(Ok(Ok(()))) => true,
                            Ok(Ok(Err(err))) => {
                                // Surface (notification/HUD per the taxonomy) and
                                // drop the HUD back to Idle. No session begins.
                                self.present_capture_start_error(&err);
                                false
                            }
                            Ok(Err(join_err)) => {
                                log::error!(
                                    target: "audio",
                                    "audio-start task panicked: {join_err}"
                                );
                                // Defensive: a panic mid-open could have armed the
                                // device before unwinding. `stop()` is idempotent
                                // and cheap, and we are still synchronously inside
                                // this press iteration (no newer press can have
                                // opened capture yet), so an unconditional stop
                                // here can only ever close a mic THIS press left
                                // hot — never a subsequent press's live mic.
                                audio.stop();
                                // Surface the failure the same way the sibling
                                // `Err`/timeout arms do — a panicked probe must not
                                // fail the press silently (task 32: no log-and-drop
                                // on capture-start failure). The detailed `join_err`
                                // stays in the log line above; the user sees the HUD
                                // drop back to Idle with the standard notification.
                                self.present_capture_start_error(&MuniError::AudioStreamFailed {
                                    reason: "audio start task panicked".into(),
                                });
                                false
                            }
                            Err(_elapsed) => {
                                // The probe is wedged (hung device open).
                                // `spawn_blocking` can't be cancelled, so we can't
                                // reclaim the thread — but we must NOT drop the
                                // `JoinHandle`: a merely-slow probe may still finish
                                // and open the mic AFTER we give up here, leaving a
                                // hot mic with no session (privacy leak). Hand the
                                // handle to a detached watcher that stops that
                                // orphaned mic if the probe ever completes.
                                log::error!(
                                    target: "audio",
                                    "audio-start timed out after {CAPTURE_START_TIMEOUT:?}; abandoning wedged probe (a late completion will be stopped)"
                                );
                                spawn_late_capture_stop(
                                    start_task,
                                    capture_generation.clone(),
                                    capture_gen,
                                    audio.clone(),
                                );
                                self.present_capture_start_error(&MuniError::AudioStreamFailed {
                                    reason: "audio device probe timed out".into(),
                                });
                                false
                            }
                        };

                        // Only drive the press into a session when capture
                        // actually started. On failure we do NOT start a session,
                        // but we must still return the hotkey manager to a clean
                        // state: a `ToggleLocked` press has already armed the
                        // toggle and registered Esc/Enter/NumpadEnter as consuming
                        // global shortcuts, so an explicit teardown is required
                        // (see `tear_down_toggle_after_capture_failure`). Either
                        // way we then fall through to the unconditional
                        // wait-for-release + dispatch below, which consumes this
                        // press's paired release — the teardown's synthesised
                        // `Commit` for a failed toggle, or the real
                        // modifier-release for a failed PTT — preserving the 1:1
                        // press:release invariant, while
                        // `handle_hotkey_released`/`_cancelled` no-op gracefully
                        // because no active session exists.
                        if capture_started {
                            // Subscribe afresh so the forwarder never sees stale
                            // chunks from the previous press.
                            let chunks_rx = audio.subscribe_chunks();
                            self.handle_hotkey_pressed(chunks_rx, mode).await;
                        } else {
                            tear_down_toggle_after_capture_failure(mode, &silence_signaler);
                        }

                        // Toggle-only silence watchdog, spawned ONLY once capture
                        // is live: it commits a running toggle session after
                        // `silence_threshold` of continuous silence. PTT presses
                        // already have an explicit stop signal (modifier release),
                        // and a failed capture-start has no session to watch — its
                        // armed toggle (if any) was already torn down just above,
                        // so a watchdog here would be dead weight.
                        let silence_watchdog =
                            if capture_started && matches!(mode, HotkeyMode::ToggleLocked) {
                                Some(spawn_silence_watchdog(
                                    audio.subscribe_amplitude(),
                                    silence_threshold,
                                    silence_signaler.clone(),
                                ))
                            } else {
                                None
                            };

                        let release_timeout = release_timeout_for(mode);
                        let outcome = wait_for_release_or_recover_realigning(
                            &mut release_rx,
                            release_timeout,
                            &mut stale_release_debt,
                            stale_release_deadline,
                        )
                        .await;
                        if let Some(handle) = silence_watchdog {
                            handle.abort();
                        }
                        let kind = match outcome {
                            ReleaseWaitOutcome::Released(kind) => {
                                // Caught up: any owed orphans were consumed by
                                // the realigning wait, so the debt window can
                                // close.
                                if stale_release_debt == 0 {
                                    stale_release_deadline = None;
                                }
                                kind
                            }
                            ReleaseWaitOutcome::TimedOut => {
                                log::warn!(
                                    target: "session",
                                    "no release event within {}s ({mode:?}, gen {press_generation}); force-recovering press cycle",
                                    release_timeout.as_secs()
                                );
                                // This press force-recovered without its
                                // release — it now owes one. Tag it so the
                                // next press's realigning wait discards the
                                // orphaned release whenever the OS redelivers
                                // it, buffered or not (plan 039 task 27).
                                stale_release_debt += 1;
                                stale_release_deadline =
                                    Some(Instant::now() + STALE_RELEASE_CATCHUP);
                                // A toggle session leaves Esc/Enter registered
                                // as global shortcuts and `toggle_active` set
                                // in the hotkey manager. A local commit alone
                                // would orphan them — a later Esc/Enter would
                                // then be captured system-wide (this is what
                                // produced the spurious "committed via Enter"
                                // seconds after a force-recovery). Drive the
                                // manager's teardown via the silence signaler:
                                // it unregisters the shortcuts and emits its
                                // own Commit, which the realigning wait discards
                                // on the next press. PTT has no such locked
                                // state to clean up.
                                if matches!(mode, HotkeyMode::ToggleLocked) {
                                    silence_signaler();
                                }
                                // Force-recovery is treated as a Commit so we
                                // ship whatever audio we captured rather than
                                // discarding it; matches the pre-feature
                                // behaviour where the timeout still ran the
                                // post-release pipeline.
                                ReleaseKind::Commit
                            }
                        };
                        audio.stop();
                        match kind {
                            // `auto_submit` rides through the cleanup pipeline to
                            // `deliver_final`, which presses Enter after a
                            // successful paste only for the `CommitAndSubmit`
                            // (press-Enter-to-finish) gesture.
                            //
                            // The delivery (cleanup + paste + history) runs
                            // detached (plan 039 task 25): `handle_hotkey_released`
                            // finalizes inline — freeing the audio buffer so the
                            // single-capture invariant holds — then spawns the
                            // cleanup+paste task and returns its handle. Dropping
                            // the handle detaches the task; it runs to completion
                            // independently while this loop dequeues the next
                            // press and starts capture during the Cleaning phase.
                            ReleaseKind::Commit => {
                                drop(self.handle_hotkey_released(false).await);
                            }
                            ReleaseKind::CommitAndSubmit => {
                                drop(self.handle_hotkey_released(true).await);
                            }
                            ReleaseKind::Cancel => self.handle_hotkey_cancelled().await,
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        log::warn!(target: "session", "press handler lagged by {skipped}");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
    }

    /// Begin a press: pick the ASR backend (Whisper batch or Deepgram
    /// streaming) per the live `english_fast_mode` flag, spawn a
    /// per-press worker that consumes audio chunks until release.
    ///
    /// Concurrent re-entry is rejected (the broadcast subscription naturally
    /// serializes presses; this is a defensive guard). `chunks_rx` is owned
    /// by the spawned forwarder; callers do not need to drive it themselves.
    pub async fn handle_hotkey_pressed(
        &self,
        chunks_rx: broadcast::Receiver<Vec<i16>>,
        mode: HotkeyMode,
    ) {
        let mut active_guard = self.active.lock().await;
        if active_guard.is_some() {
            log::warn!(target: "session", "press received while session active — ignoring");
            return;
        }

        // Notify Listening (or ListeningLocked for plan 030 toggle
        // sessions) eagerly — BEFORE the Deepgram pool checkout — so
        // the HUD overlay and tray icon surface the press even when the
        // checkout fails (missing key, network down). Without this, a press
        // with no API key configured leaves the user staring at an
        // unchanged screen wondering whether the hotkey registered. If the
        // checkout below fails, `emit_error` immediately transitions us to
        // `Error`, which the HUD treats as a hide trigger — net effect is
        // a brief flash of the pill, which is the right UX cue.
        //
        // Race note: the active slot is still empty at this point, but
        // `handle_hotkey_released` is serialised against this method by
        // `self.active`'s tokio mutex, so a racing release will queue
        // behind us and read the post-fill state.
        // Bump the HUD epoch now that a real press is starting (plan 039
        // task 25). Any delivery still cleaning from an earlier press
        // captured a lower epoch and will suppress its terminal HUD
        // transition so it can't stomp the Listening pill we raise below.
        self.press_epoch.fetch_add(1, Ordering::SeqCst);

        let listening_state = match mode {
            HotkeyMode::Ptt => SessionState::Listening,
            HotkeyMode::ToggleLocked => SessionState::ListeningLocked,
        };
        self.notify_state(listening_state);

        // No AVFoundation probe here. macOS caches
        // `authorizationStatusForMediaType` per-process — when the user
        // toggles the Microphone TCC switch and dismisses the "Quit &
        // Reopen" sheet with *Later*, the cached value lies in both
        // directions (revoke → still Authorized; grant → still Denied).
        // The honest signal is the audio stream itself: revoke + Later
        // makes CoreAudio deliver silence, which we detect in
        // `handle_hotkey_released` via per-press peak amplitude. See
        // `SILENCED_PEAK_THRESHOLD` for the threshold rationale.
        let pressed_at = Instant::now();

        // Routing decision read once at press start. Order matters —
        // bilingual_mode (backlog 0012 escape hatch) takes precedence
        // over fast_mode because the user has explicitly opted into
        // bilingual correctness over speed:
        //   bilingual + whisper ok        → AutoDetect (LID task will
        //                                    short-circuit to Whisper
        //                                    immediately — see
        //                                    spawn_lid_task)
        //   fast_mode=true                → Deepgram (manual override)
        //   neither + whisper ok          → AutoDetect (LID router)
        //   neither + no whisper          → Deepgram (graceful fallback)
        // The next press picks up the new value (atomic load); a
        // mid-press toggle does not retroactively change the routing.
        let bilingual_mode = self.deps.bilingual_mode.is_enabled();
        let fast_mode = self.deps.english_fast_mode.is_enabled();
        let whisper_available = self.deps.whisper.is_some();
        let route = if bilingual_mode && whisper_available {
            PressRoute::AutoDetect
        } else if fast_mode {
            PressRoute::Deepgram
        } else if whisper_available {
            PressRoute::AutoDetect
        } else {
            log::warn!(
                target: "asr",
                "english_fast_mode=false but Whisper client unavailable — falling back to Deepgram"
            );
            PressRoute::Deepgram
        };
        log::info!(
            target: "asr",
            "press routing: {} (bilingual_mode={bilingual_mode}, english_fast_mode={fast_mode})",
            route.as_log_str(bilingual_mode)
        );

        if matches!(route, PressRoute::AutoDetect) {
            self.spawn_auto_detect_press(active_guard, chunks_rx, pressed_at)
                .await;
            return;
        }

        let client = match self.deps.deepgram_pool.take().await {
            Ok(c) => c,
            Err(err) => {
                log::error!(target: "deepgram", "open failed: {}", err.user_message());
                // Feature 033 — terminal failure of the primary (Deepgram) path:
                // the press aborts with nothing pasted. Emit `dictation_failed`
                // so primary-path failures (offline, Deepgram outage, bad key)
                // aren't a telemetry blind spot — the Whisper route has its own
                // funnel in `rescue_via_gladia_or_emit_terminal`. Metadata only:
                // the stable error kind, never the reason string. No fallback runs
                // here, so this can't double-count with that funnel.
                crate::telemetry::emit_event(crate::telemetry::events::dictation_failed(
                    crate::error_presenter::kind_of(&err),
                ));
                self.emit_error(&err);
                return;
            }
        };

        let (released_tx, released_rx) = oneshot::channel();
        let forwarder_client = client.clone();
        // Buffer the PCM only when Parakeet is the active backend; the
        // forwarder hands it to `finalize_deepgram` to transcribe on release.
        let buffer_pcm = self.deps.parakeet.is_some();
        let forwarder: JoinHandle<(Vec<i16>, i16)> = tauri::async_runtime::spawn(async move {
            forward_chunks_until_release(
                forwarder_client.as_ref(),
                chunks_rx,
                released_rx,
                buffer_pcm,
            )
            .await
        });

        *active_guard = Some(ActiveSession::Deepgram(DeepgramActive {
            client,
            forwarder,
            released_tx: Some(released_tx),
            pressed_at,
        }));
    }

    /// Set up the AutoDetect press: take a Deepgram socket, start the
    /// dual-purpose forwarder, and spawn the LID task that watches the
    /// shared buffer for the [`LID_SLICE_SAMPLES`] threshold.
    ///
    /// Pulled into its own method to keep [`handle_hotkey_pressed`]
    /// readable. Caller holds `active_guard` so we can install the
    /// resulting [`ActiveSession::AutoDetect`] in one place.
    async fn spawn_auto_detect_press(
        &self,
        mut active_guard: tokio::sync::MutexGuard<'_, Option<ActiveSession>>,
        chunks_rx: broadcast::Receiver<Vec<i16>>,
        pressed_at: Instant,
    ) {
        let client = match self.deps.deepgram_pool.take().await {
            Ok(c) => c,
            Err(err) => {
                // Plan 039 task 26 — pool outage at press start on the
                // AutoDetect route. Rather than aborting the press with
                // nothing pasted, capture + locally buffer the audio and
                // route the release through the Whisper/Gladia batch path
                // (learned/011: independent infra beats retrying a downed
                // provider). Audio-LID is moot — with no Deepgram English
                // stream to arbitrate, the only viable route is the Whisper
                // batch, so we commit to it up front. The quiet amber
                // `Recovering` pill (learned/026) is raised in finalize; no
                // terminal error, no `dictation_failed` — the press is
                // rescued, not lost. If the buffer-only fallback can't run
                // (no Whisper client), `rescue_deepgram_route` surfaces the
                // terminal error itself on release.
                log::warn!(
                    target: "deepgram",
                    "pool open failed at press start ({}) — capturing buffer-only, routing release to Whisper batch",
                    err.user_message()
                );
                self.spawn_whisper_batch_press(active_guard, chunks_rx, pressed_at, err);
                return;
            }
        };

        let aborted = Arc::new(AtomicBool::new(false));
        let (released_tx, released_rx) = oneshot::channel();
        let forwarder_client = client.clone();
        let forwarder_aborted = aborted.clone();

        // Subscribe an additional receiver dedicated to the LID slice.
        // The forwarder consumes the original `chunks_rx`; the LID
        // task can't share it without races, so a fresh subscription
        // is the cleanest split. Both receivers see the same upstream
        // chunks.
        //
        // NOTE: we'd prefer to read the slice directly out of the
        // forwarder's accumulated buffer, but Tauri-managed
        // `Arc<TokioMutex<Vec<i16>>>` would force the forwarder hot
        // path to re-acquire a lock on every chunk. The dedicated
        // subscriber keeps the forwarder lock-free.
        // TODO(spike): replace with a shared MPMC buffer once Option A
        // graduates from spike to product feature.
        // The audio module exposes `subscribe_chunks` via the
        // `AudioCapture` Arc, but the press path doesn't have a
        // direct handle to it from inside the orchestrator. Instead
        // we pass an explicit second receiver through the press
        // dispatch — see the LID task below for the consumer.
        let lid_chunks_rx = chunks_rx.resubscribe();
        let forwarder = tauri::async_runtime::spawn(async move {
            forward_and_buffer_until_release(
                forwarder_client,
                chunks_rx,
                released_rx,
                forwarder_aborted,
            )
            .await
        });

        let decision = Arc::new(TokioMutex::new(None::<RouterDecision>));
        let decision_notify = Arc::new(Notify::new());
        // Per-press release signal (plan 039 task 13). Only the sender is kept;
        // each waiter `subscribe()`s its own receiver on demand. `subscribe`
        // works for the lifetime of the sender even though the initial receiver
        // is dropped here.
        let release_tx = watch::channel(false).0;
        let committed = Arc::new(AtomicBool::new(false));
        let gemini_handle: Arc<TokioMutex<Option<tauri::async_runtime::JoinHandle<()>>>> =
            Arc::new(TokioMutex::new(None));
        let confidence_trigger_handle: Arc<
            TokioMutex<Option<tauri::async_runtime::JoinHandle<()>>>,
        > = Arc::new(TokioMutex::new(None));
        let trigger_inflight = Arc::new(AtomicBool::new(false));
        // Feature 021 — slot for the audio-LID-side Gemini hybrid
        // task. Populated by `run_audio_lid_pass` when selective
        // triggering decides the press is uncertain enough to warrant
        // the parallel-Gemini pass; aborted by `finalize_auto_detect`
        // after the `committed` sentinel flips on release.
        let audio_hybrid_handle: Arc<TokioMutex<Option<tauri::async_runtime::JoinHandle<()>>>> =
            Arc::new(TokioMutex::new(None));
        // Feature 021 round-6 — counter tracking in-flight inner
        // classify tasks spawned by the hybrid. Read by
        // `finalize_auto_detect` on release to decide whether to
        // wait briefly for late-arriving verdicts.
        let audio_hybrid_inflight = Arc::new(AtomicUsize::new(0));
        // Feature 024 (backlog 0042) — speech-only mirror buffer.
        // Populated by Site D inside `spawn_audio_hybrid_task` when
        // `MUNI_VAD_STREAM_HYBRID` is on; read at release by
        // `resolve_trimmed_release_buffer` when
        // `MUNI_VAD_TRIM_RELEASE_BUFFER` is on. Empty otherwise.
        let audio_hybrid_speech_mirror: Arc<TokioMutex<Vec<i16>>> =
            Arc::new(TokioMutex::new(Vec::new()));
        // Backlog 0048 — shared drift counter + release-fire floor. The
        // LID task mirrors its local `consecutive_drift` into the
        // atomic on every state change; `finalize_auto_detect` reads
        // both at release to decide whether to fire the at-release
        // stale-drift override BEFORE flipping `committed` (the LID
        // task's release-arm loses the tokio race on this path).
        let audio_lid_drift_counter = Arc::new(AtomicUsize::new(0));
        let audio_lid_release_drift_fire_floor = load_audio_lid_release_drift_fire_floor();
        // Backlog 0048 v2 — shared "last post-commit verdict was Other"
        // atomic. Set by `apply_audio_lid_verdict` on every IgnoreNoise
        // action; cleared on every other post-commit action. Read by
        // `finalize_auto_detect` at release.
        let audio_lid_last_post_commit_was_other = Arc::new(AtomicBool::new(false));
        let audio_lid_release_other_as_taglish = load_audio_lid_release_other_as_taglish();
        // Backlog 0052 — per-press symmetric veto bit + resolved env knob.
        // Bit is set by `spawn_audio_hybrid_inner_classify` on an explicit
        // English verdict only (plan 039 task 20 — `Other` no longer arms
        // it); read by `apply_audio_lid_verdict` and `finalize_auto_detect`
        // to block drift overrides.
        let audio_hybrid_recent_text_lid_english = Arc::new(AtomicBool::new(false));
        let audio_lid_hybrid_veto_drift = load_audio_lid_hybrid_veto_drift();

        let lid_handle = self.spawn_lid_task(
            client.clone(),
            lid_chunks_rx,
            aborted,
            decision.clone(),
            decision_notify.clone(),
            release_tx.clone(),
            committed.clone(),
            gemini_handle.clone(),
            confidence_trigger_handle.clone(),
            trigger_inflight.clone(),
            audio_hybrid_handle.clone(),
            audio_hybrid_inflight.clone(),
            audio_hybrid_speech_mirror.clone(),
            audio_lid_drift_counter.clone(),
            audio_lid_last_post_commit_was_other.clone(),
            audio_lid_release_other_as_taglish,
            audio_hybrid_recent_text_lid_english.clone(),
            audio_lid_hybrid_veto_drift,
        );

        *active_guard = Some(ActiveSession::AutoDetect(AutoDetectActive {
            deepgram_client: client,
            forwarder,
            decision,
            decision_notify,
            release_tx,
            lid_handle,
            committed,
            gemini_handle,
            confidence_trigger_handle,
            trigger_inflight,
            audio_hybrid_inflight,
            audio_hybrid_handle,
            audio_hybrid_speech_mirror,
            audio_lid_drift_counter,
            audio_lid_release_drift_fire_floor,
            audio_lid_last_post_commit_was_other,
            audio_lid_release_other_as_taglish,
            audio_hybrid_recent_text_lid_english,
            audio_lid_hybrid_veto_drift,
            released_tx: Some(released_tx),
            pressed_at,
        }));
    }

    /// Set up a buffer-only press when the Deepgram pool is down at press
    /// start (plan 039 task 26).
    ///
    /// No streaming socket and no LID task: with Deepgram unavailable there
    /// is no English fast path to arbitrate, so the press is Whisper-committed
    /// up front. A [`buffer_until_release`] forwarder collects the PCM; on
    /// release [`Self::finalize_whisper_batch`] replays it through the Groq
    /// Whisper → Gladia batch chain. Mirrors [`Self::spawn_auto_detect_press`]'s
    /// shape (takes the caller's `active_guard` so the resulting session is
    /// installed in one place).
    fn spawn_whisper_batch_press(
        &self,
        mut active_guard: tokio::sync::MutexGuard<'_, Option<ActiveSession>>,
        chunks_rx: broadcast::Receiver<Vec<i16>>,
        pressed_at: Instant,
        take_err: MuniError,
    ) {
        let (released_tx, released_rx) = oneshot::channel();
        let forwarder: JoinHandle<(Vec<i16>, i16)> = tauri::async_runtime::spawn(async move {
            buffer_until_release(chunks_rx, released_rx).await
        });

        *active_guard = Some(ActiveSession::WhisperBatch(WhisperBatchActive {
            forwarder,
            released_tx: Some(released_tx),
            pressed_at,
            take_err,
        }));
    }

    /// End a press: signal the active backend's worker, finalize the
    /// transcript via whichever path is in flight, run Groq cleanup with
    /// raw fallback on failure, paste the result.
    /// `auto_submit` is `true` only when the press finished via the
    /// "press Enter to finish" gesture; it rides through cleanup down to
    /// [`Self::deliver_final`], which presses Enter in the focused app
    /// after the paste lands (submitting the message). Every other commit
    /// path (re-tap, silence timeout, safety cap, force-recovery) passes
    /// `false`.
    ///
    /// Plan 039 task 25 — finalize runs inline (it drains the press's audio
    /// buffer, so the single-capture invariant holds), then cleanup, paste,
    /// and history run in a **detached** task and this returns that task's
    /// [`JoinHandle`] so the driver loop can dequeue the next press and
    /// start capture during this press's Cleaning phase. Returns `None`
    /// when the press produced nothing to deliver (no active session,
    /// finalize yielded nothing, or an empty/hallucination transcript) — no
    /// delivery task is spawned in those cases, so they take no slot in the
    /// paste-order chain. The driver drops the handle (detach); tests await
    /// it to observe the delivered paste deterministically.
    pub async fn handle_hotkey_released(
        self: &Arc<Self>,
        auto_submit: bool,
    ) -> Option<JoinHandle<()>> {
        // Plan 041 (wave 1) — t₀ for the per-press timing ledger. Stamped
        // at the very entry so `total_ms` covers the whole release path.
        // Pure `Instant::now()`; nothing here awaits before the branch.
        let press_t0 = Instant::now();
        let state = {
            let mut g = self.active.lock().await;
            g.take()
        };
        let Some(mut state) = state else {
            log::debug!(
                target: "session",
                "release received with no active session — ignoring"
            );
            return None;
        };

        if let Some(tx) = state.take_release_tx() {
            let _ = tx.send(());
        }
        // Note: the release watch (`release_tx`) is no longer fired here
        // for the AutoDetect path. It moved inside [`Self::finalize_auto_detect`]
        // so the feature-019 drain window can let the trigger task
        // process pending Deepgram finals before it sees the release
        // signal and exits. For the case the original notification
        // was load-bearing (backlog 0011 Bug 1 — pass#1/2 collection
        // loops hanging on the never-closing broadcast), the trigger
        // is by construction not yet armed (collection happens before
        // pass#2 commit), so `finalize_auto_detect` fires the release
        // watch immediately in that branch with no added
        // latency.
        let press_duration = state.pressed_at().elapsed();
        // Cleaning covers finalize() AND any subsequent Groq pass — both
        // are "we're working on the press" from the user's POV. Set it
        // here so the brief gap between release and final delivery shows
        // the right tray state without a flicker.
        self.notify_state(SessionState::Cleaning);

        // Plan 012 / 034 / 039 — the served-by tag is authored by the finalize
        // methods below. Non-default branches flip it away from the streaming
        // default: `whisper-fallback` (Groq Whisper batch — the routine audio-LID
        // Whisper route AND the Deepgram-route rescue both tag this),
        // `gladia-rescue` (the cross-provider rescue), `deepgram-partial`
        // (recovered `is_final` chunks), or `parakeet-local` (on-device English).
        // Every other branch keeps `gladia-primary` — the legacy streaming-primary
        // tag, also the
        // migration default for pre-v2 rows; the legacy LID routes never had a
        // fallback tag and live on as "primary".
        // Plan 041 (wave 1) — `lid_wait` is the summed release-path
        // LID-settle wait. `None` stays `None` on routes that never enter
        // `finalize_auto_detect` (Deepgram/english-fast, Whisper batch),
        // so those presses record `lid_wait_ms = NULL`, not 0.
        let mut lid_wait: Option<Duration> = None;
        let (raw_transcript, peak_amplitude, served_by) = match state {
            ActiveSession::Deepgram(active) => match self.finalize_deepgram(active).await {
                Some((raw, peak, served_by)) => (raw, peak, served_by),
                None => return None,
            },
            ActiveSession::AutoDetect(active) => {
                // `finalize_auto_detect` returns its own `served_by` tag so the
                // Parakeet local arm can be distinguished from Deepgram/Whisper.
                // It also accumulates the three LID-settle waits into `lid_wait`.
                match self
                    .finalize_auto_detect(active, press_duration, &mut lid_wait)
                    .await
                {
                    Some((raw, peak, served_by)) => (raw, peak, served_by),
                    None => return None,
                }
            }
            ActiveSession::WhisperBatch(active) => {
                // Plan 039 task 26 — Deepgram pool was down at press start;
                // batch-transcribe the locally-buffered PCM via Whisper/Gladia.
                match self.finalize_whisper_batch(active, press_duration).await {
                    Some((raw, peak, served_by)) => (raw, peak, served_by),
                    None => return None,
                }
            }
        };
        // ASR span: release → raw transcript ready (measured across whichever
        // finalize route ran). `audio_ms` mirrors the telemetry
        // `audio_duration_ms` source (the press's wall-clock duration).
        let asr_ms = press_t0.elapsed().as_millis() as i64;

        let trimmed = raw_transcript.trim();
        // Feature 023 (backlog 0040) — known Whisper hallucination
        // phrases survive feat/022's noise-only widen because their
        // shape (`Thank you.`, `Thanks for watching!`, `はい`) carries
        // alphanumeric content. Hoisted once so the predicate cost
        // (9-entry normalize+compare) is paid at most once per press.
        let is_hallucination = matches_known_hallucination(trimmed);
        if trimmed.is_empty() || is_noise_only_transcript(trimmed) || is_hallucination {
            // feat/022 — silent idle on empty OR noise-only transcript
            // (the post-Whisper widen catches `.`, `...`, `-`, etc.
            // hallucinations on silent presses that crept just above
            // the peak threshold). The `MicrophoneDenied` toast used
            // to fire here is removed — the heuristic was unreliable
            // (every silent press tripped it, not just real mic-mute
            // events). Backlog 0037 owns any future mic-mute UX.
            //
            // `MicSilencedFlag::mark_silenced` is preserved — separate
            // UX feature surfacing the "Stale — restart Muni" pill on
            // the Permissions card when AVFoundation's cache lies.
            // Feature 033 — classify WHY the press produced no output for the
            // `dictation_empty` health event (a small fixed vocabulary, never
            // content). Mirrors the branch the log line takes below.
            let empty_reason = if is_hallucination {
                EMPTY_REASON_HALLUCINATION
            } else if is_silent_press(peak_amplitude, press_duration) {
                EMPTY_REASON_SILENT_PRESS
            } else {
                EMPTY_REASON_EMPTY_TRANSCRIPT
            };
            if is_hallucination {
                // Feature 023 — distinct log line so dogfood can
                // attribute the gate fire to the allowlist (vs. the
                // empty-transcript or noise-only paths).
                log::info!(
                    target: "asr",
                    "Whisper transcript matched known hallucination (\"{trimmed}\") — silent-idle"
                );
            } else if is_silent_press(peak_amplitude, press_duration) {
                log::info!(
                    target: "session",
                    "silent press detected (peak={peak_amplitude}, duration={press_duration:?}) — skipping Groq cleanup"
                );
                // Only a *dead* capture stream (digital silence, peak ~0)
                // is evidence of a mid-session TCC revoke behind a lying AV
                // cache. A merely-quiet press is a live mic and must not
                // flip the pill to Stale. See `is_dead_capture_stream`.
                if is_dead_capture_stream(peak_amplitude, press_duration)
                    && av_cache_is_lying(permissions::microphone_status())
                {
                    self.deps.mic_silenced.mark_silenced();
                }
            } else {
                log::info!(target: "session", "empty transcript — skipping Groq cleanup");
            }
            self.emit(EVENT_TRANSCRIPT_FINAL, String::new());
            crate::telemetry::emit_event(crate::telemetry::events::dictation_empty(empty_reason));
            self.notify_state(SessionState::Idle);
            return None;
        }

        // Reaching here means the press produced real, audible content —
        // positive proof the AVFoundation cache is not lying. Clear any
        // prior "Stale" latch so the Permissions pill self-heals without a
        // restart (a benign silent hold earlier this session should not pin
        // the pill while dictation plainly works). See `MicSilencedFlag`.
        self.deps.mic_silenced.clear_silenced();

        // Feature 013 — apply My Words substitution on the trimmed
        // transcript before cleanup. EVENT_TRANSCRIPT_RAW upstream
        // already emitted the pre-substitution text for the debug
        // overlay; only the cleanup-input string is rewritten here.
        let substituted = self.deps.my_words.apply(trimmed);
        let final_input: String = if substituted.as_str() == trimmed {
            trimmed.to_string()
        } else {
            log::info!(
                target: "my_words",
                "applied substitution(s) (raw={} chars, out={} chars)",
                trimmed.len(),
                substituted.len()
            );
            substituted
        };

        // Plan 039 task 25 — hand cleanup + paste + history to a detached
        // task so the driver loop unblocks the moment finalize is done.
        // Two coordination concerns are threaded through `DeliveryContext`:
        //
        //  * paste order — install this delivery's completion receiver as
        //    the new chain tail and capture the predecessor's; the spawned
        //    task's drop guard fires ours when it finishes. `deliver_final`
        //    awaits the predecessor right before pasting, so pastes land in
        //    press order even though cleanups overlap.
        //  * HUD epoch — captured now (still this press's epoch); the task
        //    suppresses its terminal HUD transition if a newer press has
        //    since taken the HUD.
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let predecessor = {
            let mut tail = self
                .delivery_order_tail
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tail.replace(done_rx)
        };
        let epoch = self.press_epoch.load(Ordering::SeqCst);
        // Plan 041 (wave 1) — release-time timing skeleton. `route` is the
        // committed served-by label; `audio_ms` mirrors the telemetry
        // press-duration source; `asr_ms` and `lid_wait_ms` are already
        // measured above. `cleanup_ms`/`inject_ms`/`total_ms` are filled
        // by the delivery tail as those phases complete.
        let timing = PressTiming::new(
            served_by,
            Some(press_duration.as_millis() as i64),
            Some(asr_ms),
            lid_wait.map(|d| d.as_millis() as i64),
        );
        let ctx = DeliveryContext {
            order: predecessor,
            epoch: Some(epoch),
            press_t0,
            timing,
        };

        let this = Arc::clone(self);
        let handle = tauri::async_runtime::spawn(async move {
            // Drop guard fires the successor's paste-order gate on every
            // exit path — normal return, panic, or a dropped future
            // (learned/013) — so a delivery that dies mid-cleanup can never
            // strand later presses behind a gate that never opens.
            let _order_done = DeliveryDoneGuard {
                tx: Some(done_tx),
                epoch,
            };
            this.run_groq_cleanup(&final_input, served_by, press_duration, auto_submit, ctx)
                .await;
        });
        Some(handle)
    }

    /// Plan 030 — cancel an active toggle session.
    ///
    /// Strict subset of [`Self::handle_hotkey_released`]: tear the
    /// active session down without running finalize, cleanup, paste,
    /// or history. Audio captured so far is discarded; the orchestrator
    /// transitions straight to `Idle`. Called by the driver loop when
    /// the release payload is [`ReleaseKind::Cancel`] (toggle Esc).
    ///
    /// Idempotent: if no session is active the call is a no-op with a
    /// debug log — Esc may race the natural release path (e.g. landing
    /// the same instant as a 60 s timeout) and the driver must not panic.
    pub async fn handle_hotkey_cancelled(&self) {
        let state = {
            let mut g = self.active.lock().await;
            g.take()
        };
        let Some(mut state) = state else {
            log::debug!(
                target: "session",
                "cancel received with no active session — ignoring"
            );
            return;
        };

        // Signal release to the forwarder. For the cancel path the
        // released_tx oneshot still has to fire — without it the
        // forwarder's `tokio::select!` keeps waiting and the join below
        // never resolves. The receiver doesn't care that the press is
        // ending in a cancel rather than a commit.
        if let Some(tx) = state.take_release_tx() {
            let _ = tx.send(());
        }

        match state {
            ActiveSession::Deepgram(active) => {
                // Best-effort drain so the forwarder task isn't left
                // dangling. We deliberately do NOT call `finalize` —
                // the cancel contract is "discard audio".
                let _ = active.forwarder.await;
                active.client.close().await;
            }
            ActiveSession::AutoDetect(active) => {
                // Mark the LID + forwarder paths as committed so any
                // late hybrid/Gemini reply can't flip a decision we've
                // already abandoned. Mirrors the abort discipline used
                // by `finalize_auto_detect` when the press routes to a
                // single backend.
                active.committed.store(true, Ordering::SeqCst);
                active.lid_handle.abort();
                if let Some(handle) = active.gemini_handle.lock().await.take() {
                    handle.abort();
                }
                if let Some(handle) = active.confidence_trigger_handle.lock().await.take() {
                    handle.abort();
                }
                if let Some(handle) = active.audio_hybrid_handle.lock().await.take() {
                    handle.abort();
                }
                active.forwarder.abort();
                active.deepgram_client.close().await;
            }
            ActiveSession::WhisperBatch(active) => {
                // Buffer-only capture (plan 039 task 26): no socket, no LID
                // machinery — just abort the buffering forwarder so its task
                // isn't left dangling. The cancel contract is "discard audio",
                // so we never run the Whisper batch transcribe.
                active.forwarder.abort();
            }
        }

        log::info!(
            target: "session",
            "toggle session cancelled by user — audio discarded"
        );
        self.notify_state(SessionState::Idle);
    }

    /// Finalize a Deepgram-streaming press. Returns the raw transcript +
    /// peak amplitude, or `None` if the finalize errored (in which case
    /// the error has already been surfaced to the user).
    async fn finalize_deepgram(
        &self,
        active: DeepgramActive,
    ) -> Option<(String, i16, &'static str)> {
        // Wait for the forwarder to finish its drain window so finalize() runs
        // against a quiesced socket. Drop the forwarder's `Arc<DeepgramClient>`
        // first by joining; only this method's Arc remains. `samples` is empty
        // unless Parakeet is active (the forwarder only buffers then).
        let (samples, peak) = active.forwarder.await.unwrap_or((Vec::new(), 0));

        // Parakeet local backend (English Fast Mode path): transcribe the
        // buffered PCM on-device. Try Parakeet BEFORE closing/finalizing
        // Deepgram so a failure falls through to the intact finalize path.
        if let Some(parakeet) = self.deps.parakeet.as_deref() {
            match parakeet.transcribe(&samples).await {
                Ok((text, infer_ms)) => {
                    log::info!(
                        target: "asr",
                        "parakeet served press (fast-mode): {} samples, infer_ms={infer_ms}, {} chars",
                        samples.len(),
                        text.len()
                    );
                    self.emit(EVENT_TRANSCRIPT_RAW, text.clone());
                    active.client.close().await;
                    return Some((text, peak, SERVED_BY_PARAKEET_LOCAL));
                }
                Err(err) => {
                    log::warn!(
                        target: "asr",
                        "parakeet transcribe failed ({}) — falling back to Deepgram finalize",
                        err.user_message()
                    );
                    // Fall through to the Deepgram finalize path.
                }
            }
        }

        let raw = match active.client.finalize().await {
            Ok(FinalizeOutcome::Complete(transcript)) => {
                log::info!(
                    target: "deepgram",
                    "raw transcript: {} chars",
                    transcript.len()
                );
                self.emit(EVENT_TRANSCRIPT_RAW, transcript.clone());
                self.record_deepgram_usage(&active.client).await;
                transcript
            }
            // Finalize handshake failed, but Deepgram had already streamed
            // `is_final` chunks during the press — recover them as a degraded
            // success: emit the raw overlay event, record usage, raise the
            // amber recovering pill + quiet toast (see `signal_partial_recovered`),
            // and tag the row `deepgram-partial` so downstream (deliver_final)
            // suppresses auto-submit on a possibly-truncated thought.
            Ok(FinalizeOutcome::Partial(transcript)) => {
                log::warn!(
                    target: "deepgram",
                    "finalize handshake failed — recovered partial transcript: {} chars",
                    transcript.len()
                );
                self.emit(EVENT_TRANSCRIPT_RAW, transcript.clone());
                self.record_deepgram_usage(&active.client).await;
                self.signal_partial_recovered();
                active.client.close().await;
                return Some((transcript, peak, SERVED_BY_DEEPGRAM_PARTIAL));
            }
            Err(err) => {
                log::warn!(target: "deepgram", "finalize failed: {}", err.user_message());
                self.emit_error(&err);
                active.client.close().await;
                return None;
            }
        };
        active.client.close().await;
        Some((raw, peak, SERVED_BY_GLADIA_PRIMARY))
    }

    /// Emit a [`UsageRecord`] for a successful Deepgram press.
    ///
    /// `audio_seconds` comes from the server's `Metadata.duration`
    /// when present; on its absence we still record the row but mark
    /// `status="partial"` so the dev pane can flag the discrepancy.
    /// Cost computation runs inside the writer task using the
    /// `nova-3` price for the call's UTC month.
    async fn record_deepgram_usage(&self, client: &DeepgramClient) {
        let Some(tx) = self.deps.usage_tx.as_ref() else {
            return;
        };
        let duration = client.last_metadata_duration().await;
        let status = if duration.is_some() { "ok" } else { "partial" };
        try_send_drop_oldest(
            tx,
            UsageRecord {
                provider: crate::pricing::PROVIDER_DEEPGRAM.into(),
                model: "nova-3".into(),
                call_kind: crate::pricing::CALL_KIND_ASR.into(),
                audio_seconds: duration,
                input_tokens: None,
                output_tokens: None,
                latency_ms: None,
                status: status.into(),
                request_id: None,
                session_id: None,
                created_at_unix: unix_seconds_now(),
            },
        );
    }

    /// Feature 010 — emit a [`UsageRecord`] for a successful Gladia
    /// press. `audio_seconds` comes from the server-reported audio
    /// duration on the final transcript frame; on its absence we still
    /// record the row but mark `status="partial"` so the dev pane can
    /// flag the discrepancy. Cost computation runs in the writer task
    /// using the `solaria-1` price for the call's UTC month.
    async fn record_gladia_usage(&self, client: &GladiaClient, press_duration: Duration) {
        let Some(tx) = self.deps.usage_tx.as_ref() else {
            return;
        };
        let server_reported = client.last_metadata_duration().await;
        // Prefer server-reported `audio_duration` (matches Gladia's
        // own meter — what we want for dashboard cross-checks).
        // Fall back to the hotkey-hold duration when Gladia omits
        // the field on its final-transcript frame. The fallback is
        // an over-estimate by typically <100 ms (audio capture
        // startup + scheduling overhead between hotkey-press and
        // first chunk) — acceptable for cost-tracking precision.
        let (audio_seconds, status, source) = match server_reported {
            Some(secs) => (Some(secs), "ok", "server"),
            None => (
                Some(press_duration.as_secs_f64()),
                "ok_fallback_press_duration",
                "fallback_press_duration",
            ),
        };
        log::debug!(
            target: "gladia",
            "usage audio_seconds={:.3} source={}",
            audio_seconds.unwrap_or(0.0),
            source
        );
        try_send_drop_oldest(
            tx,
            UsageRecord {
                provider: crate::pricing::PROVIDER_GLADIA.into(),
                model: crate::gladia::MODEL.into(),
                call_kind: crate::pricing::CALL_KIND_ASR.into(),
                audio_seconds,
                input_tokens: None,
                output_tokens: None,
                latency_ms: None,
                status: status.into(),
                request_id: Some(client.session_id().to_string()),
                session_id: None,
                created_at_unix: unix_seconds_now(),
            },
        );
    }

    /// Finalize an auto-detect press. Awaits the LID decision (with a
    /// bounded wait) and dispatches to either the Deepgram-finalize
    /// path or the Whisper batch path.
    ///
    /// Default-on-uncertainty (feature 003 rule): if LID hasn't
    /// returned by [`RELEASE_LID_WAIT`] we route to **Whisper**, not
    /// Deepgram. Whisper handles both English and Tagalog correctly,
    /// so the worst case is a slow English press; routing
    /// to Deepgram on uncertainty risks confidently-wrong English
    /// output for any Tagalog content. The Deepgram socket is closed
    /// here so we stop streaming bytes the user may not want sent.
    async fn finalize_auto_detect(
        &self,
        active: AutoDetectActive,
        press_duration: Duration,
        lid_wait_out: &mut Option<Duration>,
    ) -> Option<(String, i16, &'static str)> {
        // Plan 041 (wave 1) — this route DOES pay a LID-settle wait (the
        // `wait_for_decision` below always runs), so mark the phase as
        // occurring: `Some(0)` rather than `None`. The three settle sites
        // accumulate into this in place, so every early return carries the
        // waits measured so far; `None` is reserved for routes that never
        // reach this function.
        *lid_wait_out = Some(Duration::ZERO);
        // Feature 019 — release-drain window. If the confidence trigger
        // was armed (i.e. pass#2 committed Deepgram and the trigger task
        // is monitoring per-chunk confidence), Deepgram may still be
        // holding the most recent Tagalog audio waiting for an
        // `endpointing=300` silence gap that never came. Send `Finalize`
        // to force a flush *without* closing the WS, then briefly let
        // the trigger task process the resulting low-confidence finals
        // before signaling the release watch (which would otherwise tell
        // the trigger to exit cleanly via its `select!` branch and lose
        // those late events).
        //
        // Skipped when the trigger wasn't armed — in that case
        // the release watch is fired immediately and behavior is
        // byte-identical to pre-feature.
        let trigger_was_armed = active.confidence_trigger_handle.lock().await.is_some();
        let drain_ms = if trigger_was_armed {
            load_confidence_trigger_drain_ms()
        } else {
            0
        };
        if trigger_was_armed && drain_ms > 0 {
            log::info!(
                target: "lid",
                "release: draining Deepgram for {drain_ms} ms before commit (trigger was armed)"
            );
            match active.deepgram_client.flush().await {
                Ok(()) => {
                    tokio::time::sleep(Duration::from_millis(drain_ms)).await;
                }
                Err(err) => {
                    log::warn!(
                        target: "lid",
                        "release: Deepgram Finalize failed: {} — proceeding without drain",
                        err.user_message()
                    );
                }
            }
        }
        // Fire the release signal (plan 039 task 13). One `watch` send wakes
        // EVERY waiter — the LID task's pass#1 / pass#2 collection loops, the
        // audio-LID window loop, the hybrid task, and the confidence-trigger
        // task — where the old `notify_one()` woke only one and starved the
        // rest until the 1 s default. Moved here from `handle_hotkey_released`
        // so the drain above runs before the trigger sees release.
        //
        // Use `send_replace`, NOT `send`: `watch::Sender::send` short-circuits
        // to `Err` WITHOUT storing the value when the receiver count is 0, and
        // there are real zero-receiver windows here (a press released before
        // its `run_lid_task` first polls; the text-LID handoff where the pass
        // task returned pre-release and the confidence-trigger task hasn't
        // subscribed yet). If we dropped the value in those windows the sticky
        // guarantee would break and a waiter that subscribes afterwards would
        // block until the full `RELEASE_LID_WAIT`. `send_replace` stores `true`
        // unconditionally, so a late subscriber still observes the flip.
        active.release_tx.send_replace(true);

        // Wait briefly for the LID task to land. If it already has,
        // `notified()` returns immediately; if not we cap at
        // `RELEASE_LID_WAIT`.
        let settle_started = Instant::now();
        let (notified, snapshot) =
            wait_for_decision(&active.decision_notify, &active.decision, RELEASE_LID_WAIT).await;
        // Plan 041 (wave 1) — first settle site: the primary decision wait.
        if let Some(w) = lid_wait_out.as_mut() {
            *w += settle_started.elapsed();
        }

        let mut chosen = match snapshot {
            Some(d) => d,
            None => {
                if !notified {
                    log::info!(
                        target: "asr",
                        "LID not ready after {} ms — defaulting to Whisper",
                        RELEASE_LID_WAIT.as_millis()
                    );
                }
                // Close the Deepgram socket so we stop streaming bytes
                // we no longer care about — mirrors what the LID task
                // does on its own switch-to-Whisper path.
                active.deepgram_client.close().await;
                RouterDecision::Whisper
            }
        };

        // Feature 019 — if the confidence trigger has a re-pass in
        // flight at release time, wait briefly for its verdict before
        // committing the route. Without this, a code-switched press
        // released near the moment the trigger fires has its Tagalog
        // tail silently dropped — the trigger's
        // `override_decision_deepgram_to_whisper` no-ops once
        // `committed=true` flips below.
        //
        // Skipped entirely on pure-English presses (the common case):
        // the trigger never fires, `trigger_inflight` stays `false`,
        // and the wait is bypassed → zero added latency.
        if matches!(chosen, RouterDecision::Deepgram)
            && active.trigger_inflight.load(Ordering::SeqCst)
        {
            // First, re-read the cell — the override may have already
            // completed in the gap between `wait_for_decision`
            // returning and us getting here.
            let recheck = *active.decision.lock().await;
            if matches!(recheck, Some(RouterDecision::Whisper)) {
                chosen = RouterDecision::Whisper;
                log::info!(
                    target: "lid",
                    "release: confidence trigger had already flipped route before wait registered"
                );
            } else {
                let started = Instant::now();
                let wait_result = tokio::time::timeout(
                    Duration::from_millis(TRIGGER_REPASS_WAIT_MS),
                    active.decision_notify.notified(),
                )
                .await;
                let elapsed = started.elapsed();
                let elapsed_ms = elapsed.as_millis();
                // Plan 041 (wave 1) — second settle site: confidence-trigger wait.
                if let Some(w) = lid_wait_out.as_mut() {
                    *w += elapsed;
                }
                let new_snapshot = *active.decision.lock().await;
                if matches!(new_snapshot, Some(RouterDecision::Whisper)) {
                    chosen = RouterDecision::Whisper;
                    log::info!(
                        target: "lid",
                        "release: confidence trigger flipped route to Whisper after waiting {} ms (notify_fired={})",
                        elapsed_ms,
                        wait_result.is_ok()
                    );
                } else {
                    log::info!(
                        target: "lid",
                        "release: confidence trigger re-pass wait elapsed {} ms (notify_fired={}) — keeping {:?}",
                        elapsed_ms,
                        wait_result.is_ok(),
                        chosen
                    );
                }
            }
        }

        // Feature 021 round-6 fix 2026-05-18 (B3) — if the audio-LID
        // hybrid has an in-flight inner classify at release time,
        // wait briefly for it to land. Same TRIGGER_REPASS_WAIT_MS
        // budget as feat/019's confidence-trigger wait (1500 ms,
        // bounded by Tokio timeout, short-circuits early if
        // `decision_notify` fires).
        //
        // Dogfood evidence (round 6): the hybrid's leading/trailing
        // parallel classifies frequently land 200–700 ms AFTER
        // release on short presses (P01: +555 ms, P06: +605 ms).
        // Without this wait, the `committed=true` flag below would
        // be set first and the in-flight override would no-op even
        // when the verdict was `taglish`.
        //
        // Skipped on:
        // - Presses where hybrid never spawned (counter stays 0 → no wait).
        // - Routes already on Whisper (no override needed).
        // - Routes already flipped to Whisper during the gap (re-read
        //   short-circuits before paying the wait budget).
        //
        // The `decision_notify` is fired both by:
        // - A successful override (`override_or_commit_to_whisper_via_hybrid`).
        // - The `InflightGuard::drop` on every inner-classify
        //   completion — so a non-flipping verdict (english/Other)
        //   wakes the waiter early instead of blocking the full
        //   1500 ms budget.
        if matches!(chosen, RouterDecision::Deepgram)
            && active.audio_hybrid_inflight.load(Ordering::SeqCst) > 0
        {
            let inflight_at_start = active.audio_hybrid_inflight.load(Ordering::SeqCst);
            let started = Instant::now();
            // Loop the wait within the budget (plan 039 task 14): a leading
            // english classify's notify must not abandon a trailing taglish
            // classify still in flight. The helper short-circuits the moment the
            // route flips or all inner classifies drain.
            let flipped = await_hybrid_inflight_flip(
                &active.decision,
                &active.decision_notify,
                &active.audio_hybrid_inflight,
                Duration::from_millis(TRIGGER_REPASS_WAIT_MS),
            )
            .await;
            let elapsed = started.elapsed();
            let elapsed_ms = elapsed.as_millis();
            // Plan 041 (wave 1) — third settle site: audio-LID hybrid wait.
            if let Some(w) = lid_wait_out.as_mut() {
                *w += elapsed;
            }
            let inflight_at_end = active.audio_hybrid_inflight.load(Ordering::SeqCst);
            if flipped {
                chosen = RouterDecision::Whisper;
                log::info!(
                    target: "lid",
                    "release: audio-LID hybrid flipped route to Whisper after waiting {} ms (inflight {}→{})",
                    elapsed_ms,
                    inflight_at_start,
                    inflight_at_end
                );
            } else {
                log::info!(
                    target: "lid",
                    "release: audio-LID hybrid wait elapsed {} ms (inflight {}→{}) — keeping {:?}",
                    elapsed_ms,
                    inflight_at_start,
                    inflight_at_end,
                    chosen
                );
            }
        }

        // Backlog 0048 — at-release stale-drift commit. Sits HERE,
        // not in the LID task's `released(&mut release_rx)` arm,
        // because `wait_for_decision` above short-circuits the moment
        // the decision cell is populated. The cell is typically
        // populated long before release (by the first audio-LID
        // window's `Commit` action), so finalize_auto_detect runs
        // straight from `release_tx.send(true)` through to
        // `committed.store(true)` without yielding to the LID task —
        // any at-release fire in the LID task loses the race and
        // no-ops on the `committed.load() == true` pre-check inside
        // `override_decision_deepgram_to_whisper`.
        //
        // Reading the drift counter from the shared
        // `audio_lid_drift_counter` (mirrored by every state change in
        // `apply_audio_lid_verdict`) lets the orchestrator make the
        // same decision the LID task would have made, but synchronously
        // before the kill switch flips. The override helper is
        // idempotent, so a defensive double-fire from the LID task's
        // release arm is safe (it will no-op once we set `committed`).
        if matches!(chosen, RouterDecision::Deepgram) {
            let drift = active.audio_lid_drift_counter.load(Ordering::SeqCst);
            let last_was_other = active
                .audio_lid_last_post_commit_was_other
                .load(Ordering::SeqCst);
            // Backlog 0052 — symmetric veto. AND with the env knob so
            // a future regression can be rolled back without rebuild.
            let hybrid_recent_english = active.audio_lid_hybrid_veto_drift
                && active
                    .audio_hybrid_recent_text_lid_english
                    .load(Ordering::SeqCst);
            // Log the veto event when the unvetoed decision WOULD have
            // fired but the hybrid bit blocks it. The probe call is a
            // pure function so it costs nothing beyond a few branches;
            // only the log line is conditional. Complements the
            // mid-press veto log in `apply_audio_lid_verdict`.
            if hybrid_recent_english
                && audio_lid_decide_release_action(
                    Some(chosen),
                    drift,
                    active.audio_lid_release_drift_fire_floor,
                    last_was_other,
                    active.audio_lid_release_other_as_taglish,
                    false,
                ) == AudioLidReleaseAction::FireOverrideToWhisper
            {
                log::info!(
                    target: "lid",
                    "release: audio-LID stale-drift fire vetoed by hybrid text-LID (recent_english=true, drift={}, floor={}, last_was_other={}) — keeping route deepgram",
                    drift,
                    active.audio_lid_release_drift_fire_floor,
                    last_was_other,
                );
            }
            if matches!(
                audio_lid_decide_release_action(
                    Some(chosen),
                    drift,
                    active.audio_lid_release_drift_fire_floor,
                    last_was_other,
                    active.audio_lid_release_other_as_taglish,
                    hybrid_recent_english,
                ),
                AudioLidReleaseAction::FireOverrideToWhisper
            ) {
                log::info!(
                    target: "lid",
                    "release: audio-LID stale drift ({}/floor={}, last_was_other={}) — firing override deepgram → whisper",
                    drift,
                    active.audio_lid_release_drift_fire_floor,
                    last_was_other,
                );
                let flipped = Self::override_decision_deepgram_to_whisper(
                    &active.decision,
                    &active.decision_notify,
                    &active.committed,
                )
                .await;
                if flipped {
                    // Mirror the existing LID-not-ready fallback path
                    // at the top of this function: close the Deepgram
                    // socket so the forwarder stops streaming bytes we
                    // no longer care about. The forwarder fail-softs
                    // on subsequent send errors.
                    active.deepgram_client.close().await;
                    chosen = RouterDecision::Whisper;
                    log::info!(
                        target: "lid",
                        "release: audio-LID stale-drift override applied: deepgram → whisper"
                    );
                }
            }
        }

        // Backlog 0012 — set the committed sentinel BEFORE aborting
        // the LID handle so a Gemini override task that races the
        // abort sees `committed=true` and bails before mutating the
        // (already-routed) decision cell. Without this, a Gemini
        // reply that lands between `wait_for_decision` returning and
        // `lid_handle.abort()` could flip the verdict after the
        // orchestrator has already started Whisper transcribe.
        active.committed.store(true, Ordering::SeqCst);
        // The LID task is either done (its handle resolves
        // immediately) or still running on a doomed timeout — let it
        // finish in the background so we don't pay another await
        // here.
        active.lid_handle.abort();
        // Abort an in-flight Gemini override RPC. The result would
        // be discarded via the `committed` sentinel anyway; aborting
        // here releases the Gemini API call (and any HTTP/CPU
        // contention with the downstream cleanup) instead of letting
        // it run to completion in the background. Logged at INFO so
        // dogfood log scans can see the abort fire — pairs with the
        // `gemini-override text-LID = …` line at the same level.
        if let Some(h) = active.gemini_handle.lock().await.take() {
            h.abort();
            log::info!(target: "lid", "gemini override aborted on release");
        }
        // Feature 019 — abort the confidence-trigger task. Sits next
        // to the Gemini abort because it has the same race-protection
        // shape: `committed.store(true)` is already up the function
        // (line ~2404), so a trigger task mid-re-pass that survives
        // the abort will still no-op when its
        // `override_decision_deepgram_to_whisper` call checks
        // `committed`. The abort just releases the in-flight Whisper
        // + Groq calls that would otherwise burn API quota for a
        // verdict the orchestrator has already discarded.
        if let Some(h) = active.confidence_trigger_handle.lock().await.take() {
            h.abort();
            log::info!(target: "lid", "confidence trigger aborted on release");
        }
        // Feature 021 — abort an in-flight audio-LID hybrid
        // task on release. The `committed` sentinel above gates the
        // override path so a late Gemini reply can't mutate the
        // routed decision cell, but aborting here releases the
        // in-flight Whisper transcribe + Gemini classify HTTP
        // requests instead of letting them run to completion.
        if let Some(h) = active.audio_hybrid_handle.lock().await.take() {
            h.abort();
            log::info!(target: "lid", "audio-LID hybrid aborted on release");
        }
        // Feature 024 (backlog 0042) — clear the speech-only mirror
        // AFTER `resolve_trimmed_release_buffer` has had a chance to
        // consume it. Site E's read site is downstream (inside the
        // Whisper branch below), so we DO NOT clear here — the clear
        // happens at the start of the next press, via the
        // construction of a fresh `audio_hybrid_speech_mirror` `Arc`
        // in `arm_auto_detect`. (The mirror's `Arc` is per-press,
        // so the next press's mirror is by definition fresh.)

        log::info!(
            target: "asr",
            "auto-detect resolved: route={:?}",
            chosen
        );

        let (samples, peak) = active.forwarder.await.unwrap_or((Vec::new(), 0));

        match chosen {
            RouterDecision::Deepgram => {
                // Parakeet local backend (MUNI_ASR_BACKEND=parakeet): transcribe
                // the buffered PCM on-device instead of finalizing the cloud
                // Deepgram stream. Try Parakeet BEFORE closing Deepgram so a
                // failure falls through to the still-intact finalize path below.
                if let Some(parakeet) = self.deps.parakeet.as_deref() {
                    match parakeet.transcribe(&samples).await {
                        Ok((text, infer_ms)) => {
                            log::info!(
                                target: "asr",
                                "parakeet served press: {} samples, infer_ms={infer_ms}, {} chars",
                                samples.len(),
                                text.len()
                            );
                            self.emit(EVENT_TRANSCRIPT_RAW, text.clone());
                            active.deepgram_client.close().await;
                            return Some((text, peak, SERVED_BY_PARAKEET_LOCAL));
                        }
                        Err(err) => {
                            log::warn!(
                                target: "asr",
                                "parakeet transcribe failed ({}) — falling back to Deepgram finalize",
                                err.user_message()
                            );
                            // Fall through to the Deepgram finalize path.
                        }
                    }
                }

                // Deepgram has been streaming throughout the press.
                // Standard finalize → transcript.
                let raw = match active.deepgram_client.finalize().await {
                    Ok(FinalizeOutcome::Complete(t)) => {
                        log::info!(
                            target: "deepgram",
                            "raw transcript: {} chars",
                            t.len()
                        );
                        self.emit(EVENT_TRANSCRIPT_RAW, t.clone());
                        self.record_deepgram_usage(&active.deepgram_client).await;
                        t
                    }
                    // Finalize handshake failed but accumulated `is_final`
                    // chunks survive — recover them as a degraded success and
                    // tag `deepgram-partial` (mirrors `finalize_deepgram`,
                    // including the amber recovering pill via
                    // `signal_partial_recovered`).
                    Ok(FinalizeOutcome::Partial(t)) => {
                        log::warn!(
                            target: "deepgram",
                            "finalize handshake failed — recovered partial transcript: {} chars",
                            t.len()
                        );
                        self.emit(EVENT_TRANSCRIPT_RAW, t.clone());
                        self.record_deepgram_usage(&active.deepgram_client).await;
                        self.signal_partial_recovered();
                        active.deepgram_client.close().await;
                        return Some((t, peak, SERVED_BY_DEEPGRAM_PARTIAL));
                    }
                    Err(err) => {
                        log::warn!(
                            target: "deepgram",
                            "finalize failed: {} — attempting cross-provider rescue",
                            err.user_message()
                        );
                        // Plan 039 slice 4 (task 10) — the primary Deepgram
                        // stream produced no usable transcript (dead socket,
                        // zero finals). Rather than dropping the press, replay
                        // the locally-buffered PCM through the SAME Groq
                        // Whisper → Gladia cross-provider chain the audio-LID
                        // Whisper route uses (learned/011: independent infra
                        // beats retry-same-provider). The Partial branch above
                        // already returned, so this arm is the Failed/empty
                        // case. `rescue_deepgram_route` surfaces the terminal
                        // error itself when the rescue can't run or also fails.
                        //
                        // Detach the primary-stream teardown: a wedged half-open
                        // socket's close handshake can block up to Deepgram's
                        // CLOSE_TIMEOUT (~500 ms), and awaiting it inline here would
                        // delay the rescue of an already-degraded press — the same
                        // teardown stall task 12 removed from `DeepgramPool::take`.
                        // The cloned `Arc` keeps the client alive until close finishes.
                        let dg_teardown = active.deepgram_client.clone();
                        tauri::async_runtime::spawn(async move {
                            dg_teardown.close().await;
                        });
                        return self
                            .rescue_deepgram_route(&samples, peak, press_duration, &err)
                            .await;
                    }
                };
                active.deepgram_client.close().await;
                Some((raw, peak, SERVED_BY_DEEPGRAM))
            }
            RouterDecision::Whisper => {
                // LID switched mid-press; the Deepgram client is
                // already closed by the LID task in non-hybrid mode.
                // Backlog 0012 — in hybrid mode the LID task defers
                // the close so a late Gemini override can finalize on
                // the still-open WS; if no override fired the close
                // lands here. `close()` is idempotent, so this is
                // also safe in non-hybrid mode where the LID task
                // already closed.
                active.deepgram_client.close().await;
                let Some(client) = self.deps.whisper.as_deref() else {
                    log::error!(
                        target: "asr",
                        "WhisperClient unavailable on auto-routed Whisper press"
                    );
                    self.emit_error(&MuniError::GroqConnectionFailed {
                        reason: "Whisper client not initialized".into(),
                    });
                    return None;
                };
                // Hot release-path read — cached to skip a per-press keychain
                // IPC on the LID-routed Whisper finalize branch (plan 039 task
                // 17). Env override stays live; keychain layer is cached and
                // invalidated on `secrets://changed`.
                let api_key = match secrets::get_cached(secrets::GROQ_ACCOUNT) {
                    Ok(k) => k,
                    Err(err) => {
                        log::error!(target: "groq_whisper", "{}", err.user_message());
                        self.emit_error(&err);
                        return None;
                    }
                };

                // Silent/short-press gates (too-short → silent → VAD). Shared
                // with the Deepgram-route rescue so an accidental graze resolves
                // to silent idle identically on both paths, never reaching Groq.
                if self
                    .resolve_silent_press_idle(&samples, peak, press_duration)
                    .await
                {
                    return None;
                }

                // Feature 024 (backlog 0042) Site E — release-path
                // Whisper batch buffer trim. If the audio-LID hybrid
                // ran with Site D enabled, the speech-only mirror is
                // handed to Whisper; otherwise (Tagalog-leading
                // committed-fast presses) the one-shot fallback path
                // runs a fresh streaming-VAD pass over the full
                // buffer. No-op when `MUNI_VAD_TRIM_RELEASE_BUFFER` is
                // off (the default on first ship).
                let trim_buffer = resolve_trimmed_release_buffer(
                    &self.deps,
                    Some(&active.audio_hybrid_speech_mirror),
                    &samples,
                )
                .await;

                // Groq Whisper batch transcribe → Gladia rescue on failure.
                // Shared with the Deepgram-route rescue (task 10) so both paths
                // record identical provenance/telemetry and stay in lockstep.
                self.transcribe_via_whisper_or_rescue(client, &trim_buffer, &api_key, peak)
                    .await
            }
        }
    }

    /// Finalize a buffer-only press (plan 039 task 26 — Deepgram pool was
    /// down at press start).
    ///
    /// Awaits the buffered PCM and replays it through
    /// [`Self::rescue_deepgram_route`] — the same Groq Whisper → Gladia
    /// cross-provider batch chain the Deepgram-route rescue uses, which
    /// already encapsulates the silent/short-press gate, the amber
    /// `Recovering` pill (learned/026), and the terminal-error surfacing
    /// when neither provider can serve. There was never a streaming socket
    /// or partial transcript to prefer here, so the whole press collapses to
    /// that rescue chain. The pool-open error is threaded through only for
    /// diagnostics + terminal telemetry.
    async fn finalize_whisper_batch(
        &self,
        active: WhisperBatchActive,
        press_duration: Duration,
    ) -> Option<(String, i16, &'static str)> {
        let (samples, peak) = active.forwarder.await.unwrap_or((Vec::new(), 0));
        self.rescue_deepgram_route(&samples, peak, press_duration, &active.take_err)
            .await
    }

    /// Release-path silent/short-press gate shared by the audio-LID Whisper
    /// route and the Deepgram-route rescue.
    ///
    /// Returns `true` when the press is empty enough to resolve as **silent
    /// idle** — no paste, no toast — having already emitted the empty final,
    /// the `dictation_empty` telemetry (with the specific reason), and the
    /// `Idle` state. Both callers batch through Groq Whisper, which rejects a
    /// sub-0.01 s clip with an HTTP 400 and hallucinates punctuation on
    /// near-silent audio; running these gates first means an accidental
    /// sub-50 ms hotkey graze (or a silent press) never reaches Groq or the
    /// terminal "transcription unavailable" notification — it just resolves
    /// quietly the same way it would on the primary path.
    ///
    /// Gates, in order (cheapest / most specific first):
    /// 1. buffer too short for Groq Whisper's 0.01 s floor,
    /// 2. amplitude+duration silence gate,
    /// 3. content-aware VAD (only when the detector built at boot).
    async fn resolve_silent_press_idle(
        &self,
        samples: &[i16],
        peak: i16,
        press_duration: Duration,
    ) -> bool {
        // Gate 0 — buffer is too short for Groq Whisper's 0.01 s minimum
        // (typically a sub-50 ms accidental hotkey graze where cpal never
        // delivered its first callback). Sits before `is_silent_press` because
        // that gate's 500 ms duration floor was designed to NOT trip on short
        // accidental presses — it cannot catch this case.
        if audio_too_short_for_groq_whisper(samples) {
            log::info!(
                target: "asr",
                "press too short for Groq Whisper (samples={}, duration={press_duration:?}) — silent idle",
                samples.len()
            );
            self.emit(EVENT_TRANSCRIPT_FINAL, String::new());
            crate::telemetry::emit_event(crate::telemetry::events::dictation_empty(
                EMPTY_REASON_TOO_SHORT,
            ));
            self.notify_state(SessionState::Idle);
            return true;
        }

        // Gate 1 — silent presses hallucinate `.` etc. on near-zero audio.
        // Skip the Whisper batch + cleanup entirely and fall through to idle.
        if is_silent_press(peak, press_duration) {
            log::info!(
                target: "asr",
                "silent press detected (peak={peak}, duration={press_duration:?}) — skipping Whisper batch"
            );
            // Only a *dead* capture stream (digital silence, peak ~0) is
            // evidence of a mid-session TCC revoke behind a lying AV cache.
            // A merely-quiet press is a live mic and must not flip the pill
            // to Stale. See `is_dead_capture_stream`.
            if is_dead_capture_stream(peak, press_duration)
                && av_cache_is_lying(permissions::microphone_status())
            {
                self.deps.mic_silenced.mark_silenced();
            }
            self.emit(EVENT_TRANSCRIPT_FINAL, String::new());
            crate::telemetry::emit_event(crate::telemetry::events::dictation_empty(
                EMPTY_REASON_SILENT_PRESS,
            ));
            self.notify_state(SessionState::Idle);
            return true;
        }

        // Gate 2 — content-aware VAD catches ambient-silent presses whose peak
        // slipped past the amplitude gate. Fails open per the trait contract;
        // only fires when the detector built successfully at boot.
        if let Some(vad) = self.deps.vad_detector.as_ref() {
            if !vad.predict_speech(samples).await {
                log::info!(
                    target: "asr",
                    "VAD detected no speech in press (duration={}ms, vad={}) — skipping Whisper batch",
                    press_duration.as_millis(),
                    vad.provider_label()
                );
                // Deliberately does NOT mark the mic "Stale": VAD finding no
                // *speech* in an audible press (peak already above the
                // amplitude gate, so the stream carried real sound) is proof
                // the mic is alive, not that the AV cache is lying. Marking
                // here was the dominant false-positive source — a user
                // tapping the hotkey without speaking pinned the pill to
                // Stale for the whole session. Only a dead capture stream
                // (Gate 1, `is_dead_capture_stream`) is a valid stale signal.
                self.emit(EVENT_TRANSCRIPT_FINAL, String::new());
                crate::telemetry::emit_event(crate::telemetry::events::dictation_empty(
                    EMPTY_REASON_VAD_NO_SPEECH,
                ));
                self.notify_state(SessionState::Idle);
                return true;
            }
        }

        false
    }

    /// Batch-transcribe `trim_buffer` via Groq Whisper and, on failure, fall
    /// through to the cross-provider Gladia rescue.
    ///
    /// Shared by the audio-LID Whisper route (its normal batch path) and the
    /// Deepgram-route rescue (a Deepgram finalize that returned no usable
    /// transcript — plan 039 slice 4, task 10). Records the Groq attempt
    /// (`ok`/`error`) in usage telemetry so the cost pane shows fallback fires,
    /// emits the raw-transcript overlay on success, and tags provenance
    /// (task 11): [`SERVED_BY_WHISPER_FALLBACK`] when Groq Whisper served the
    /// press, [`SERVED_BY_GLADIA_RESCUE`] when the Gladia rescue did.
    ///
    /// Returns `None` only when the press is terminally lost — in which case
    /// [`rescue_via_gladia_or_emit_terminal`] has already surfaced the
    /// user-facing terminal error.
    async fn transcribe_via_whisper_or_rescue(
        &self,
        client: &GroqWhisperClient,
        trim_buffer: &[i16],
        api_key: &str,
        peak: i16,
    ) -> Option<(String, i16, &'static str)> {
        let started = Instant::now();
        let result = client.transcribe(trim_buffer, api_key).await;
        let elapsed = started.elapsed();

        match result {
            Ok(transcript) => {
                log::info!(
                    target: "asr",
                    "groq_whisper transcribed: {} samples in {} ms ({} chars)",
                    trim_buffer.len(),
                    elapsed.as_millis(),
                    transcript.len()
                );
                self.emit(EVENT_TRANSCRIPT_RAW, transcript.clone());
                self.record_groq_whisper_usage(trim_buffer.len(), elapsed, "ok");
                // Plan 041 (task 7) — a successful Groq Whisper call kept
                // the shared pool warm; note it so the keepalive can skip
                // a redundant ping. Not a prefix-touch: Whisper doesn't
                // warm the cleanup prompt cache.
                if let Some(activity) = self.deps.groq_activity.as_ref() {
                    activity.note_call();
                }
                Some((transcript, peak, SERVED_BY_WHISPER_FALLBACK))
            }
            Err(err) => {
                log::warn!(
                    target: "groq_whisper",
                    "transcribe failed after {} ms: {}",
                    elapsed.as_millis(),
                    err.user_message()
                );
                // Record the failed Groq attempt so the cost pane shows when
                // the fallback path fires.
                self.record_groq_whisper_usage(trim_buffer.len(), elapsed, "error");
                // Feature 021 — attempt the Gladia rescue, or emit the
                // user-facing terminal error if it can't run / also fails.
                self.rescue_via_gladia_or_emit_terminal(trim_buffer, &err)
                    .await
                    .map(|text| (text, peak, SERVED_BY_GLADIA_RESCUE))
            }
        }
    }

    /// Write a Groq Whisper batch-transcribe usage row (cost telemetry).
    /// Extracted so the success and failure legs of
    /// [`transcribe_via_whisper_or_rescue`] record identical shapes — only
    /// `status` (`ok`/`error`) differs.
    fn record_groq_whisper_usage(&self, samples_len: usize, elapsed: Duration, status: &str) {
        let Some(tx) = self.deps.usage_tx.as_ref() else {
            return;
        };
        try_send_drop_oldest(
            tx,
            UsageRecord {
                provider: crate::pricing::PROVIDER_GROQ.into(),
                model: crate::groq_whisper::DEFAULT_MODEL.into(),
                call_kind: crate::pricing::CALL_KIND_ASR.into(),
                audio_seconds: Some(
                    samples_len as f64 / crate::groq_whisper::PCM_SAMPLE_RATE as f64,
                ),
                input_tokens: None,
                output_tokens: None,
                latency_ms: Some(elapsed.as_millis() as i64),
                status: status.into(),
                request_id: None,
                session_id: None,
                created_at_unix: unix_seconds_now(),
            },
        );
    }

    /// Deepgram-route rescue (plan 039 slice 4, task 10). The primary
    /// streaming path returned no usable transcript (dead socket, zero
    /// finals); replay the locally-buffered PCM through the same Groq
    /// Whisper → Gladia cross-provider chain the audio-LID Whisper route
    /// uses (learned/011: independent infra beats retry-same-provider).
    ///
    /// Prefers Groq Whisper (independent of Deepgram) first, then Gladia; if
    /// neither the Whisper client nor a Groq key is available, goes straight to
    /// the Gladia rescue so a Deepgram outage still lands the press. Returns
    /// the rescued transcript + provenance, or `None` when the press is
    /// terminally lost (the terminal error is surfaced by the rescue helper).
    async fn rescue_deepgram_route(
        &self,
        samples: &[i16],
        peak: i16,
        press_duration: Duration,
        original_err: &MuniError,
    ) -> Option<(String, i16, &'static str)> {
        // Silent/short-press gates FIRST — same as the audio-LID Whisper route.
        // A sub-50 ms graze whose Deepgram socket also died would otherwise hit
        // Groq with a <0.01 s buffer (guaranteed HTTP 400), then Gladia, then a
        // terminal "transcription unavailable" toast — user-visible error noise
        // for an accidental press the primary path resolves silently. Gating
        // here also avoids flashing the amber "recovering" pill below for a
        // press that carries no speech to rescue.
        if self
            .resolve_silent_press_idle(samples, peak, press_duration)
            .await
        {
            return None;
        }

        // Amber "recovering" pill: the primary path is dead and a multi-second
        // cross-provider rescue is running (learned/026), not a routine
        // post-release cleanup wait.
        self.notify_state(SessionState::Recovering);

        let (Some(client), Ok(api_key)) = (
            self.deps.whisper.clone(),
            secrets::get(secrets::GROQ_ACCOUNT),
        ) else {
            log::info!(
                target: "asr",
                "deepgram-route rescue: Groq Whisper unavailable (client or key) — trying Gladia directly (original={})",
                original_err.user_message()
            );
            return self
                .rescue_via_gladia_or_emit_terminal(samples, original_err)
                .await
                .map(|text| (text, peak, SERVED_BY_GLADIA_RESCUE));
        };
        self.transcribe_via_whisper_or_rescue(client.as_ref(), samples, &api_key, peak)
            .await
    }

    /// Rescue path for a failed Groq Whisper transcribe: try the
    /// cross-provider Gladia fallback; on `None` (no key, or Gladia
    /// also failed), emit the backend-agnostic [`MuniError::TranscriptionUnavailable`]
    /// so the user sees a native macOS notification instead of a
    /// silent empty paste. Returns `Some(text)` only when Gladia
    /// rescued the press; the caller can treat `None` as "press is
    /// terminal, user already notified."
    ///
    /// Promotes the failure mode from a Quiet HUD pill flash (the
    /// severity of the per-provider `GroqConnectionFailed` /
    /// `GroqServerError` variants) to a Loud system notification.
    /// Rationale: Muni's main window is almost always backgrounded
    /// when the user dictates, so the HUD pill is easy to miss.
    /// `TranscriptionUnavailable` is classified `Loud` and routes
    /// through `osascript display notification` — visible regardless
    /// of which app has focus.
    ///
    /// The original Groq error is logged for diagnostics and already
    /// recorded as a failed usage row at the call site.
    async fn rescue_via_gladia_or_emit_terminal(
        &self,
        samples: &[i16],
        original_err: &MuniError,
    ) -> Option<String> {
        // Feature 033 — the primary ASR path failed and we're attempting the
        // cross-provider Gladia rescue. Emit the operational fallback signal
        // (metadata only: the provider + the original error's stable kind).
        crate::telemetry::emit_event(crate::telemetry::events::fallback_fired(
            "gladia",
            crate::error_presenter::kind_of(original_err),
        ));
        if let Some(text) = self
            .attempt_gladia_fallback_transcribe(samples, original_err)
            .await
        {
            return Some(text);
        }
        log::warn!(
            target: "asr",
            "transcription unavailable — Groq Whisper failed and Gladia fallback didn't rescue (original={})",
            original_err.user_message()
        );
        // Feature 033 — terminal transcription failure (no paste lands). This is
        // the single terminal-failure funnel for the auto-detect Whisper route.
        // Don't double-count: `fallback_fired` above is the fallback event;
        // `dictation_failed` records the press as terminally lost.
        let terminal = MuniError::TranscriptionUnavailable;
        crate::telemetry::emit_event(crate::telemetry::events::dictation_failed(
            crate::error_presenter::kind_of(&terminal),
        ));
        self.emit_error(&terminal);
        None
    }

    /// Feature 021 fix 2026-05-18 — one-shot Gladia transcribe used as
    /// a cross-provider fallback when the auto-detect Whisper-route
    /// batch call against Groq fails (timeout, connection refused,
    /// 5xx, etc.). Opens a fresh `GladiaClient` on demand.
    ///
    /// Returns `Some(transcript)` if Gladia succeeds; `None` on any
    /// failure (missing key, open failure, send failure, finalize
    /// failure). This helper does NOT emit error events itself — only
    /// logs. The terminal error emit lives in
    /// [`rescue_via_gladia_or_emit_terminal`], which is the sole
    /// production caller.
    ///
    /// On success, the transcript is emitted to `EVENT_TRANSCRIPT_RAW`
    /// and a Gladia usage row is written so the cost dashboard shows
    /// the fallback fire and its cost.
    async fn attempt_gladia_fallback_transcribe(
        &self,
        samples: &[i16],
        original_err: &MuniError,
    ) -> Option<String> {
        // Surface the amber "recovering" pill so the user sees a
        // distinct rescue state during the multi-second cross-provider
        // fallback instead of the dimmed Cleaning pill, which is
        // indistinguishable from a routine post-release cleanup wait.
        self.notify_state(SessionState::Recovering);
        if samples.is_empty() {
            log::debug!(
                target: "asr",
                "gladia fallback skipped: empty audio buffer (original={})",
                original_err.user_message()
            );
            return None;
        }
        let gladia_key = match secrets::get(secrets::GLADIA_ACCOUNT) {
            Ok(k) => k,
            Err(key_err) => {
                log::info!(
                    target: "asr",
                    "gladia fallback unavailable: no Gladia key configured (original={}, key_err={})",
                    original_err.user_message(),
                    key_err.user_message()
                );
                return None;
            }
        };

        let started = Instant::now();
        log::info!(
            target: "asr",
            "attempting gladia fallback transcribe ({} samples) — Groq failed: {}",
            samples.len(),
            original_err.user_message()
        );

        let client = match GladiaClient::open(&gladia_key).await {
            Ok(c) => c,
            Err(open_err) => {
                log::warn!(
                    target: "asr",
                    "gladia fallback open failed after {} ms: {} (original={})",
                    started.elapsed().as_millis(),
                    open_err.user_message(),
                    original_err.user_message()
                );
                return None;
            }
        };

        // Chunk the buffer so we don't oversaturate the TCP send
        // buffer with one big binary frame. macOS default SO_SNDBUF is
        // ~128 KB; a 10 s press is ~325 KB, so a single-frame send
        // leaves residual bytes draining when `finalize()` fires its
        // tiny `stop_recording` text frame, blowing the 1 s
        // `SEND_TIMEOUT` (observed 2026-05-25: 10.4 s press →
        // "stop_recording send timed out after 1s"). 1 s of audio per
        // chunk (32 KB) stays well below SO_SNDBUF so each send drains
        // before the next, and the final `stop_recording` flushes
        // behind a small frame.
        const FALLBACK_CHUNK_SAMPLES: usize = crate::groq_whisper::PCM_SAMPLE_RATE as usize;
        for chunk in samples.chunks(FALLBACK_CHUNK_SAMPLES) {
            if let Err(send_err) = client.send(chunk).await {
                log::warn!(
                    target: "asr",
                    "gladia fallback send failed after {} ms: {} (original={})",
                    started.elapsed().as_millis(),
                    send_err.user_message(),
                    original_err.user_message()
                );
                client.close().await;
                return None;
            }
        }

        // This is a rescue-mode replay: the whole buffer was just dumped at
        // once (vs the primary route's real-time stream), so widen the
        // finalize budget proportionally to the replayed audio (task 7b) —
        // otherwise a long press blows the tight 3 s cap before Gladia flushes
        // its terminal frame and the rescue is lost.
        let replay_seconds = samples.len() as f64 / crate::groq_whisper::PCM_SAMPLE_RATE as f64;
        client.mark_rescue_replay(replay_seconds);

        match client.finalize().await {
            // Both a clean `Complete` and a recovered `Partial` carry usable
            // text — the rescue path already lives in a degraded fallback, so
            // a possibly-truncated partial is strictly better than nothing
            // (task 7 gotcha: treat Partial as usable text).
            Ok(outcome) => {
                let transcript = outcome.into_text();
                let elapsed = started.elapsed();
                log::info!(
                    target: "asr",
                    "gladia fallback served {} chars in {} ms (original={})",
                    transcript.len(),
                    elapsed.as_millis(),
                    original_err.user_message()
                );
                self.emit(EVENT_TRANSCRIPT_RAW, transcript.clone());
                // Record the Gladia usage row. We use a press_duration
                // of `Duration::ZERO` as a sentinel; `record_gladia_usage`
                // prefers `last_metadata_duration` (server-reported)
                // when available — Gladia populates it from the final
                // frame metadata for completed sessions.
                self.record_gladia_usage(&client, Duration::ZERO).await;
                client.close().await;
                Some(transcript)
            }
            Err(finalize_err) => {
                log::warn!(
                    target: "asr",
                    "gladia fallback finalize failed after {} ms: {} (original={})",
                    started.elapsed().as_millis(),
                    finalize_err.user_message(),
                    original_err.user_message()
                );
                client.close().await;
                None
            }
        }
    }

    fn emit(&self, event: &str, payload: String) {
        (self.deps.emitter)(event, payload);
    }

    /// Plan 039 task 32 — surface a capture-start failure and return the HUD to
    /// Idle. Called from the driver when `AudioCapture::start` fails at press
    /// start (mic denied, no input device, or the cpal stream couldn't build):
    /// the press never becomes a session, so we route the typed error through
    /// the presenter and drop straight back to `Idle` — instead of the old
    /// log-and-drop that showed a fake `Listening` pill over a dead mic and let
    /// a doomed session run until silence detection caught it.
    ///
    /// Deliberately notifies `Idle` (not `Error`): no session was in flight, so
    /// there is no amber pill to raise — the presenter owns the visible surface
    /// (a mic-denied notification / HUD notice), and the HUD should simply rest.
    fn present_capture_start_error(&self, err: &MuniError) {
        log::error!(
            target: "audio",
            "capture start failed: {} ({:?})",
            err.user_message(),
            err.severity()
        );
        (self.deps.present_error)(err);
        self.notify_state(SessionState::Idle);
    }

    fn emit_error(&self, err: &MuniError) {
        self.emit(EVENT_TRANSCRIPT_ERROR, err.user_message());
        // Notify SessionState::Error so the HUD can flash its amber pill
        // and the tray tooltip updates ("Muni — last dictation failed").
        // The tray icon itself stays fixed — Phase 11 §11 pivot retired
        // the per-state icon swap; the HUD is the sole quiet-error
        // visual surface.
        self.notify_state(SessionState::Error);
        // Phase 10 — surface to the user via system notification (loud)
        // or `error://quiet` event (quiet). The presenter decides which
        // path to take from `err.severity()`.
        (self.deps.present_error)(err);
    }

    /// Surface a recovered Deepgram partial as a degraded *success*.
    ///
    /// Deliberately does NOT go through [`emit_error`](Self::emit_error):
    /// that helper sets [`SessionState::Error`], which the HUD treats as
    /// "hide the pill" — so routing a partial through it stomps the amber
    /// pill one frame after we raise it (the quiet-error toast, the other
    /// surface, lives in the hidden main window and is invisible while the
    /// user is focused on another app). Instead we raise the amber
    /// `Recovering` pill — the one error-class surface visible during a
    /// press — and fire the informational quiet toast straight through the
    /// presenter. The pill then persists through the subsequent Groq
    /// cleanup (which does not re-set `Cleaning`) until `deliver_final`
    /// returns the session to `Idle`.
    fn signal_partial_recovered(&self) {
        self.notify_state(SessionState::Recovering);
        (self.deps.present_error)(&MuniError::DeepgramPartialRecovered);
    }

    fn notify_state(&self, state: SessionState) {
        self.emit(EVENT_SESSION_STATE_CHANGED, state.as_wire().to_string());
        (self.deps.state_notifier)(state);
    }

    /// Emit a terminal HUD state from a spawned delivery task, unless a
    /// newer press has since taken over the HUD (plan 039 task 25 — "recording
    /// wins"). Paste, history, and telemetry all still run regardless — only
    /// the HUD state is suppressed, so a finished older delivery can't stomp
    /// the Listening pill of a press already recording. `epoch == None`
    /// disables the guard (direct/unit-test calls with a single press in
    /// flight always transition).
    ///
    /// Accepted TOCTOU window: the epoch load and the `notify_state` emit are
    /// not one atomic step. If a new press's `press_epoch` bump + Listening
    /// notify interleaves between the load and the emit, this stale terminal
    /// state can still land after the newer Listening and hide the HUD pill
    /// until the next transition (Cleaning on the new press's release). The
    /// window is sub-microsecond and needs a press in exactly that instant, so
    /// it is left unguarded rather than paying a shared lock on the notify hot
    /// path; it self-corrects at the new press's next state change.
    fn deliver_notify_state(&self, epoch: Option<u64>, state: SessionState) {
        if let Some(e) = epoch {
            let current = self.press_epoch.load(Ordering::SeqCst);
            if current != e {
                log::debug!(
                    target: "session",
                    "HUD {state:?} from delivery epoch {e} suppressed — press epoch {current} owns the HUD"
                );
                return;
            }
        }
        self.notify_state(state);
    }

    /// [`emit_error`](Self::emit_error) for a spawned delivery task. The
    /// user-facing error event and the presenter (toast/notification) always
    /// fire — the press failed and the user must be told — but the
    /// `SessionState::Error` HUD transition is epoch-guarded so a failed older
    /// delivery can't stomp a newer press's Listening pill (task 25).
    fn deliver_emit_error(&self, epoch: Option<u64>, err: &MuniError) {
        self.emit(EVENT_TRANSCRIPT_ERROR, err.user_message());
        self.deliver_notify_state(epoch, SessionState::Error);
        (self.deps.present_error)(err);
    }
}

/// Forward audio chunks into Deepgram until release+drain expires.
///
/// Returns the peak |i16| sample observed across every forwarded chunk.
/// `handle_hotkey_released` reads it for silence detection (a press
/// that produced essentially-zero audio across a multi-hundred-ms hold
/// is almost certainly a mic that's been silenced by macOS after a
/// runtime TCC toggle).
///
/// Branches under `biased`:
/// 1. `chunks_rx.recv()` — always polled first so the tail of an utterance
///    never starves to a release event.
/// 2. `released_rx` — flips a flag (does NOT break) so the loop keeps
///    draining whatever audio is still queued or in flight.
/// 3. `tokio::time::sleep(...)` — only enabled after release; resets each
///    iteration, so any new chunk that lands during the grace window pushes
///    the break further out. When no chunk arrives for [`POST_RELEASE_DRAIN_MS`],
///    the sleep wins and the loop breaks.
async fn forward_chunks_until_release<S: ReleaseSink + Send + Sync + ?Sized>(
    client: &S,
    mut chunks_rx: broadcast::Receiver<Vec<i16>>,
    mut released_rx: oneshot::Receiver<()>,
    buffer_pcm: bool,
) -> (Vec<i16>, i16) {
    /// After this many consecutive send failures, give up and let
    /// `finalize` surface the typed error. cpal delivers chunks at
    /// ~60 Hz on macOS, so 30 failures ≈ 0.5 s of confirmed-dead
    /// socket — past which there's nothing useful to keep retrying.
    /// QA repro: WiFi drop mid-press flooded the log with hundreds
    /// of "Sending after closing is not allowed" warnings before
    /// the forwarder hit its release.
    const MAX_CONSECUTIVE_SEND_FAILURES: usize = 30;
    let mut release_seen = false;
    let mut peak: i16 = 0;
    // Parakeet (local backend) transcribes the full clip on release, so we
    // accumulate the PCM here when asked. When the backend is Deepgram, this
    // stays empty — the fast path keeps its zero-buffer behavior.
    let mut buffer: Vec<i16> = Vec::new();
    let mut consecutive_send_failures = 0usize;
    let mut consecutive_send_timeouts = 0usize;
    // Once the socket is confirmed dead we STOP sending but keep draining
    // `chunks_rx` into the local buffer (when `buffer_pcm`) so a Parakeet
    // backend's release still has the full clip. Returning early here would
    // truncate that buffer at the failure point.
    let mut send_aborted = false;
    loop {
        tokio::select! {
            biased;
            c = chunks_rx.recv() => match c {
                Ok(chunk) => {
                    // i16::abs panics on i16::MIN; saturating_abs returns
                    // i16::MAX in that case, which is fine for a peak
                    // tracker.
                    for &sample in &chunk {
                        let mag = sample.saturating_abs();
                        if mag > peak {
                            peak = mag;
                        }
                    }
                    // Buffer BEFORE sending so a wedged socket's backpressure
                    // can't punch a hole in the Parakeet-backend PCM.
                    if buffer_pcm {
                        buffer.extend_from_slice(&chunk);
                    }
                    if send_aborted {
                        continue;
                    }
                    match client.send_chunk(&chunk).await {
                        Ok(()) => {
                            consecutive_send_failures = 0;
                            consecutive_send_timeouts = 0;
                        }
                        Err(err) => {
                            consecutive_send_failures += 1;
                            if is_send_timeout(&err) {
                                consecutive_send_timeouts += 1;
                            } else {
                                consecutive_send_timeouts = 0;
                            }
                            log::warn!(
                                target: "deepgram",
                                "send failed ({}/{} fast, {}/{} timeout): {}",
                                consecutive_send_failures,
                                MAX_CONSECUTIVE_SEND_FAILURES,
                                consecutive_send_timeouts,
                                MAX_CONSECUTIVE_SEND_TIMEOUTS,
                                err.user_message()
                            );
                            if consecutive_send_timeouts >= MAX_CONSECUTIVE_SEND_TIMEOUTS
                                || consecutive_send_failures >= MAX_CONSECUTIVE_SEND_FAILURES
                            {
                                log::warn!(
                                    target: "deepgram",
                                    "send failure cap hit — abandoning sends (still buffering), finalize will surface the error"
                                );
                                send_aborted = true;
                            }
                        }
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    log::warn!(target: "deepgram", "audio chunks lagged by {skipped}");
                }
                Err(RecvError::Closed) => break,
            },
            r = &mut released_rx, if !release_seen => {
                // Treat any outcome as "release happened" — Lagged/Closed/Err
                // all mean we won't get a second chance at the event.
                let _ = r;
                release_seen = true;
            },
            () = tokio::time::sleep(Duration::from_millis(POST_RELEASE_DRAIN_MS)),
                if release_seen => break,
        }
    }
    (buffer, peak)
}

/// RMS amplitude (in `[0, 1]`) at-or-above which an audio chunk
/// counts as speech for the toggle-session silence watchdog. Set well
/// above quiet-room ambient (~0.001-0.003 RMS) and well below normal
/// dictation levels (~0.05-0.3 RMS) so background hum doesn't keep a
/// forgotten session alive while real speech reliably resets the
/// timer. The watchdog is best-effort: in genuinely noisy rooms a
/// re-tap stays the explicit stop signal.
const SILENCE_WATCHDOG_RMS_THRESHOLD: f32 = 0.01;

/// Restore the hotkey manager after a capture-start failure (plan 039 task 32).
///
/// A `ToggleLocked` press arms the toggle and registers
/// Esc/Enter/NumpadEnter as *consuming* global shortcuts — swallowing those
/// keys system-wide until the session ends — and it does so BEFORE the driver
/// learns that capture failed (the state machine sets `toggle_active` and
/// registers the shortcuts, then emits the press; see
/// `hotkey::start_toggle_session`). Because no session ever begins, no user
/// gesture will produce the release that tears those shortcuts down. Left
/// alone, the armed toggle silently eats Esc/Enter in every app until the
/// silence watchdog fires ~`silence_threshold` (60 s) later.
///
/// So for a failed `ToggleLocked` press we invoke `silence_signaler`
/// immediately: it routes `HotkeyMsg::SilenceTimeout` into the state machine,
/// which (while `toggle_active`) unregisters the shortcuts and emits its own
/// `ReleaseKind::Commit`. The driver's unconditional
/// `wait_for_release_or_recover` then consumes that release, preserving the
/// 1:1 press:release invariant. A failed `Ptt` press armed no locked state and
/// its real modifier-release is still pending, so it needs no teardown here.
fn tear_down_toggle_after_capture_failure(
    mode: HotkeyMode,
    silence_signaler: &Arc<dyn Fn() + Send + Sync>,
) {
    if matches!(mode, HotkeyMode::ToggleLocked) {
        silence_signaler();
    }
}

/// Spawn a detached watcher over a capture-start probe the driver ABANDONED on
/// timeout (plan 039 round-2 finding, slice 11 privacy fix).
///
/// A `spawn_blocking` probe can't be cancelled, so when `AudioCapture::start`
/// wedges past [`CAPTURE_START_TIMEOUT`] the driver stops awaiting it and moves
/// on. But a merely-slow (not permanently hung) probe can still finish and open
/// the microphone afterwards — with no session watching it, that is a hot mic
/// streaming audio with no HUD indication. This watcher awaits the abandoned
/// handle and stops that orphaned mic, guarded by the capture generation so it
/// never stops a mic a *newer* press legitimately re-opened.
fn spawn_late_capture_stop(
    start_task: JoinHandle<Result<(), MuniError>>,
    capture_generation: Arc<AtomicU64>,
    generation_at_spawn: u64,
    audio: Arc<AudioCapture>,
) {
    tauri::async_runtime::spawn(stop_late_capture_if_orphaned(
        start_task,
        capture_generation,
        generation_at_spawn,
        move || audio.stop(),
    ));
}

/// Await an abandoned capture-start probe and stop the mic it may have opened,
/// but only while this press is still the current capture attempt.
///
/// `stop` is injected (rather than taking `Arc<AudioCapture>` directly) so the
/// orphaned-vs-superseded decision is unit-testable without a real audio
/// device. The generation guard is the key safety property: if a newer press
/// has advanced `capture_generation` past `generation_at_spawn`, a late probe
/// completion belongs to a superseded attempt and stopping now would kill the
/// newer press's live mic — so we leave it to that press's own lifecycle.
async fn stop_late_capture_if_orphaned<F: FnOnce()>(
    start_task: JoinHandle<Result<(), MuniError>>,
    capture_generation: Arc<AtomicU64>,
    generation_at_spawn: u64,
    stop: F,
) {
    match start_task.await {
        Ok(Ok(())) => {
            let current = capture_generation.load(Ordering::SeqCst);
            if current == generation_at_spawn {
                // No newer press took over — the probe opened a mic nobody is
                // watching. Stop it so it can't stream audio silently.
                log::warn!(
                    target: "audio",
                    "wedged capture-start probe completed AFTER its timeout (gen {generation_at_spawn}); stopping the orphaned late-opened mic"
                );
                stop();
            } else {
                log::warn!(
                    target: "audio",
                    "wedged capture-start probe completed late (gen {generation_at_spawn}) but a newer press already took over capture (gen {current}); leaving the live mic alone"
                );
            }
        }
        Ok(Err(err)) => {
            // The probe eventually failed — no mic was opened, nothing to stop.
            log::info!(
                target: "audio",
                "abandoned capture-start probe finished with an error (no mic opened): {err}"
            );
        }
        Err(join_err) => {
            // A panic could have armed the device before unwinding — best-effort
            // stop, still gated so a newer press's mic is never touched.
            log::error!(
                target: "audio",
                "abandoned capture-start probe panicked: {join_err}"
            );
            if capture_generation.load(Ordering::SeqCst) == generation_at_spawn {
                stop();
            }
        }
    }
}

/// Spawn the per-press silence watchdog used for tap-to-toggle sessions.
///
/// Subscribes to the live amplitude stream and tracks the time since
/// the last speech-grade frame (RMS ≥ [`SILENCE_WATCHDOG_RMS_THRESHOLD`]).
/// When the gap exceeds `threshold`, fires `signaler` exactly once and
/// returns. Callers abort the returned [`JoinHandle`] on any other
/// release path (re-tap, Esc, driver shutdown) so the watchdog never
/// outlives its press.
fn spawn_silence_watchdog(
    mut amp_rx: tokio::sync::watch::Receiver<f32>,
    threshold: Duration,
    signaler: Arc<dyn Fn() + Send + Sync>,
) -> JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let mut last_speech = Instant::now();
        loop {
            tokio::select! {
                changed = amp_rx.changed() => {
                    if changed.is_err() {
                        // Audio bridge dropped the sender (shutdown).
                        // No more amplitude updates will arrive; let the
                        // press end via its normal release path.
                        return;
                    }
                    let rms = *amp_rx.borrow();
                    if rms >= SILENCE_WATCHDOG_RMS_THRESHOLD {
                        last_speech = Instant::now();
                    } else if last_speech.elapsed() >= threshold {
                        log::info!(
                            target: "session",
                            "toggle session silence watchdog fired after {:.0}s of continuous silence",
                            last_speech.elapsed().as_secs_f32()
                        );
                        signaler();
                        return;
                    }
                }
                () = tokio::time::sleep_until(
                    tokio::time::Instant::from_std(last_speech + threshold)
                ) => {
                    // Belt-and-suspenders: if the amplitude stream ever
                    // stops delivering ticks while still alive (the
                    // audio bridge is supposed to publish every chunk
                    // even during silence), the sleep arm guarantees
                    // the watchdog still fires on schedule.
                    log::info!(
                        target: "session",
                        "toggle session silence watchdog fired after {:.0}s with no amplitude updates",
                        last_speech.elapsed().as_secs_f32()
                    );
                    signaler();
                    return;
                }
            }
        }
    })
}

async fn wait_for_release(release_rx: &mut broadcast::Receiver<ReleaseKind>) -> ReleaseKind {
    loop {
        match release_rx.recv().await {
            Ok(kind) => return kind,
            // A closed sender means we'll never observe a real release;
            // surface that as a Commit so the post-release pipeline runs
            // exactly once (matches the pre-feature behaviour where the
            // unit-payload helper just returned on Closed).
            Err(RecvError::Closed) => return ReleaseKind::Commit,
            Err(RecvError::Lagged(_)) => continue,
        }
    }
}

/// Outcome of a single press cycle's wait for the matching release event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReleaseWaitOutcome {
    /// Release event arrived (or the broadcast closed cleanly). Carries
    /// the [`ReleaseKind`] so the caller can dispatch to the normal
    /// commit path vs the cancel path.
    Released(ReleaseKind),
    /// No release arrived within the timeout window — caller must force
    /// the post-release path and tag the orphaned release as owed debt so
    /// [`wait_for_release_or_recover_realigning`] discards it whenever the
    /// OS redelivers it, rather than letting it collapse the next press
    /// (see backlog 0003 + manual-QA §12 for the prior-art on this
    /// stale-release failure mode).
    TimedOut,
}

/// Bounded variant of [`wait_for_release`]. Returns `Released(kind)`
/// once the release event has been observed; returns `TimedOut` if
/// `timeout` elapses first. Provides the always-correct safety net
/// described by [`WAIT_FOR_RELEASE_TIMEOUT`] regardless of whether the
/// upstream hotkey listener happens to drop a `kCGEventFlagsChanged`
/// under burst conditions.
pub(crate) async fn wait_for_release_or_recover(
    release_rx: &mut broadcast::Receiver<ReleaseKind>,
    timeout: Duration,
) -> ReleaseWaitOutcome {
    match tokio::time::timeout(timeout, wait_for_release(release_rx)).await {
        Ok(kind) => ReleaseWaitOutcome::Released(kind),
        Err(_) => ReleaseWaitOutcome::TimedOut,
    }
}

/// Bounds how long after a force-recovery the driver keeps discarding an
/// owed release (plan 039 task 27). Force-recovery timeouts are minutes
/// apart, whereas an OS-late modifier-up lands within a runloop tick or
/// two; past this window the owed release is assumed genuinely lost so the
/// debt is cleared and later presses wait normally.
///
/// Scope of the guarantee (accepted residuals — see plan 039 Slice 8 notes):
/// this bounds only the *discard window*, not every consequence. Because the
/// release broadcast is FIFO and shared across all keyed bindings (plan 038),
/// the debt is a count, not a per-press identity, so within the window two
/// edges remain:
///  * a genuinely-lost orphan is indistinguishable from a real release that
///    arrives inside the window, so a re-press within `STALE_RELEASE_CATCHUP`
///    can have its real release swallowed as the orphan — that press then
///    hangs for its full press-timeout cycle (only the 3 s discard is bounded,
///    not this consequence);
///  * a still-pending orphan that only arrives *after* the window has closed
///    is no longer discarded, so it can satisfy a later press's release-wait.
///
/// Both require the rare backlog-0003 dropped-`kCGEventFlagsChanged`
/// pathology; the dominant single-binding PTT/toggle paths are exact (FIFO
/// puts the orphan ahead of any same-key re-press, and buffered orphans are
/// burned unconditionally before the deadline is consulted). Closing them
/// fully needs a generation tag plumbed through the release payload; that is
/// deferred rather than paid on every press's hot path.
const STALE_RELEASE_CATCHUP: Duration = Duration::from_secs(3);

/// Like [`wait_for_release_or_recover`] but realigns the release stream
/// after force-recoveries (plan 039 task 27).
///
/// `stale_debt` is the number of releases still owed by presses that
/// force-recovered on timeout. Those releases may be delivered late by the
/// OS; the release broadcast is FIFO, so an owed (older) release always
/// arrives ahead of this press's real (newer) one. This discards an owed
/// release — whether already buffered OR arriving mid-wait — before honoring
/// a real release, decrementing the debt as each orphan is consumed.
///
/// It ignores a stale release by *identity* (we know one is owed) rather than
/// by whether it happened to be buffered at the iteration boundary — a
/// boundary-only drain could miss a release the OS redelivered a beat later,
/// the backlog-0003 late-release pathology this hardens.
///
/// `stale_deadline` is the [`STALE_RELEASE_CATCHUP`] cutoff after which an
/// owed release is assumed genuinely lost. Two invariants depend on honoring
/// it *inside* this wait rather than only at press start:
///  * an orphan already **buffered** at entry is burned against the debt
///    BEFORE the deadline is consulted, so a synthetic Commit that outlived
///    the catch-up window can't survive to collapse this press (the toggle
///    force-recovery emits its Commit immediately, but the next press can
///    arrive well after the window — e.g. the inline finalize alone can span
///    it);
///  * a genuinely-lost orphan stops swallowing releases once the deadline
///    passes **mid-wait**, so this press's own (possibly short) real release
///    is honored within the catch-up window instead of being discarded for
///    the whole multi-minute press timeout.
pub(crate) async fn wait_for_release_or_recover_realigning(
    release_rx: &mut broadcast::Receiver<ReleaseKind>,
    timeout: Duration,
    stale_debt: &mut u32,
    stale_deadline: Option<Instant>,
) -> ReleaseWaitOutcome {
    use tokio::sync::broadcast::error::TryRecvError;

    // 1. Burn already-buffered orphans first (non-blocking, FIFO-safe: any
    //    release buffered before this press even started belongs to an
    //    earlier, force-recovered press). This runs UNCONDITIONALLY — before
    //    any deadline check — so a buffered synthetic Commit is always
    //    consumed even when the catch-up window has already elapsed.
    while *stale_debt > 0 {
        match release_rx.try_recv() {
            Ok(_) => {
                *stale_debt -= 1;
                log::info!(
                    target: "session",
                    "discarded buffered stale release (owed now {})",
                    *stale_debt
                );
            }
            Err(TryRecvError::Lagged(_)) => continue,
            // Empty or Closed — nothing more buffered.
            Err(_) => break,
        }
    }

    // 2. Any debt still owed after draining the buffer is a release that has
    //    not arrived yet. If the catch-up window has already closed (or was
    //    never armed), assume it was genuinely lost and clear the debt so
    //    THIS press's real release is honored rather than swallowed.
    if *stale_debt > 0 && stale_deadline.map_or(true, |d| Instant::now() >= d) {
        log::info!(
            target: "session",
            "stale-release catch-up elapsed — clearing {} owed release(s) as lost",
            *stale_debt
        );
        *stale_debt = 0;
    }

    if *stale_debt == 0 {
        return wait_for_release_or_recover(release_rx, timeout).await;
    }

    // 3. Owed releases remain and we're still inside the catch-up window.
    //    Discard late-arriving orphans until the debt clears OR the deadline
    //    passes, then honor the next real release — all under the outer press
    //    timeout. Bounding the discard to the deadline is what stops a
    //    genuinely-lost orphan from swallowing this press's real release for
    //    the whole (multi-minute) press cycle.
    let outcome = tokio::time::timeout(timeout, async {
        loop {
            if *stale_debt == 0 {
                return wait_for_release(release_rx).await;
            }
            let remaining = stale_deadline
                .map(|d| d.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                log::info!(
                    target: "session",
                    "stale-release catch-up elapsed mid-wait — clearing {} owed release(s) as lost",
                    *stale_debt
                );
                *stale_debt = 0;
                continue;
            }
            match tokio::time::timeout(remaining, wait_for_release(release_rx)).await {
                Ok(_orphan) => {
                    *stale_debt -= 1;
                    log::info!(
                        target: "session",
                        "discarded late stale release (owed now {})",
                        *stale_debt
                    );
                }
                Err(_) => {
                    // Deadline elapsed before any orphan arrived — the owed
                    // releases are lost; stop discarding and honor the next
                    // real release under the remaining press-timeout budget.
                    log::info!(
                        target: "session",
                        "stale-release catch-up elapsed mid-wait — clearing {} owed release(s) as lost",
                        *stale_debt
                    );
                    *stale_debt = 0;
                }
            }
        }
    })
    .await;
    match outcome {
        Ok(kind) => ReleaseWaitOutcome::Released(kind),
        Err(_) => ReleaseWaitOutcome::TimedOut,
    }
}

pub fn resolve_env_key(env_var: &str, missing: MuniError) -> Result<String, MuniError> {
    match std::env::var(env_var) {
        Ok(key) if !key.is_empty() => Ok(key),
        _ => Err(missing),
    }
}

/// Build an [`EventEmitter`] that forwards events to a Tauri [`AppHandle`].
///
/// Emit failures are logged at warn but never propagate; the orchestrator's
/// pipeline must keep running even if the frontend has gone away.
pub fn app_handle_emitter<R: tauri::Runtime>(handle: tauri::AppHandle<R>) -> EventEmitter {
    Arc::new(move |event, payload| {
        if let Err(e) = tauri::Emitter::emit(&handle, event, payload) {
            log::warn!(target: "session", "emit {event} failed: {e}");
        }
    })
}

/// State notifier that does nothing. Used in tests and as a safe default
/// when the wiring code wants to defer hooking up the real notifier.
pub fn noop_state_notifier() -> StateNotifier {
    Arc::new(|_| {})
}

// ---- tests ----------------------------------------------------------------

#[cfg(test)]
mod tests;
