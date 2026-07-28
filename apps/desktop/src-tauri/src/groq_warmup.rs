//! Feature 016 — Groq cleanup cold-start warm-up.
//!
//! Attacks the two cold-start taxes that the user's first cleanup
//! call of a session would otherwise pay:
//! 1. `reqwest`-backed TLS / connection-pool warm-up (~200–500 ms),
//!    paid once per process on the first POST through `GroqClient`.
//! 2. Groq's per-account prompt-prefix cache cold (~2.85 s measured
//!    2026-05-13). The cache evicts after **2 hours** of inactivity,
//!    so this tax fires every time the user returns to the app after
//!    a ≥ 2 h gap.
//!
//! Speed is priority 1 per CLAUDE.md; the first press of a session
//! is the one that shapes the user's "is this app fast" perception
//! and it should NOT be the slowest of the session.
//!
//! See `.claude/brainstorm/003_groq_cleanup_cold_start_warmup.md` for
//! the full Q&A trail behind these decisions.
//!
//! ## What the warm-up does
//!
//! Spawns a detached `tauri::async_runtime::spawn` task that:
//! 1. Resolves the Groq API key via `secrets::get(GROQ_ACCOUNT)`.
//!    Missing key → log info and exit (the API-key save handler will
//!    re-fire this trigger later).
//! 2. Composes the prefix byte-identically to a real press by calling
//!    `CleanupPrompt::make_messages_with_context(transcript="hello",
//!    about_me=<live>, vocabulary_block=<live>)`. Reusing the
//!    composition path is what guarantees the byte-exact prefix match
//!    Groq's cache requires.
//! 3. Calls `GroqClient::complete(GroqRequest::cleanup(messages,
//!    cleanup_model), &api_key)`.
//! 4. On success: logs an `info` line and writes one `api_calls` row
//!    with `call_kind = "cleanup_warmup"` and `session_id = NULL` so
//!    per-press dashboards stay clean.
//! 5. On failure: logs a `warn` line. No retry, no UI event.
//!
//! ## Off-switch
//!
//! `MUNI_CLEANUP_WARMUP=false` (case-insensitive, trim-ws) disables
//! every call site. Default is enabled — the safe default for a
//! latency-win feature is "on"; the user opts out for A/B
//! comparison only.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager, Runtime};
use tokio::sync::mpsc::Sender;

use crate::about_me::AboutMe;
use crate::groq::{GroqClient, GroqRequest};
use crate::groq_activity::GroqActivity;
use crate::pricing::{CALL_KIND_CLEANUP_WARMUP, PROVIDER_GROQ};
use crate::prompt::CleanupPrompt;
use crate::secrets;
use crate::usage_writer::{try_send_drop_oldest, UsageRecord};
use crate::user_prompt::UserPrompt;
use crate::vocabulary::Vocabulary;

/// Env var that disables the warm-up at every call site. Default
/// behaviour (unset / any other value) is "warm-up enabled."
pub const CLEANUP_WARMUP_ENV: &str = "MUNI_CLEANUP_WARMUP";

/// Transcript sent in the warm-up's user message. Predictable,
/// short, log-friendly, and unlikely to elicit weird model output.
/// The prefix (system message) is what carries the cache cost; the
/// user message is incidental.
const WARMUP_USER_MESSAGE: &str = "hello";

/// Provenance label for the warm-up invocation. Travels into the
/// `[groq_warmup]` log line so post-hoc log analysis can attribute
/// "which trigger fired this warm-up" without grepping nearby
/// settings or IPC events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmupTrigger {
    /// App boot, immediately after `GroqClient` init.
    Boot,
    /// `CleanupPrompt.md` user-override saved through Settings →
    /// Cleanup Prompt. Isolated call site so the trigger is one
    /// delete when the prompt becomes non-editable.
    PromptEdit,
    /// About Me body saved through Settings → Cleanup → About Me.
    AboutMe,
    /// Vocabulary list saved through Settings → Cleanup → Vocabulary.
    Vocabulary,
    /// User-authored "preferences" prompt saved through
    /// Settings → Cleanup.
    UserPrompt,
    /// Groq API key saved through Settings → API Keys (or the
    /// onboarding wizard). Covers the case where the boot trigger
    /// was a no-op because no key was present yet.
    KeySaved,
    /// Plan 041 (slice 4) periodic staleness tick: the prompt-prefix
    /// cache went idle for ≥ 90 min and [`spawn_periodic_rewarm`]
    /// fired a re-warm to pre-empt the ~2.85s cold-cache tax on the
    /// user's next press.
    Periodic,
}

impl WarmupTrigger {
    /// Stable log-friendly label. Used both for the `trigger=` field
    /// on the success/failure log line and for log-grep filters.
    pub fn as_log_str(&self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::PromptEdit => "prompt_edit",
            Self::AboutMe => "about_me",
            Self::Vocabulary => "vocab",
            Self::UserPrompt => "user_prompt",
            Self::KeySaved => "key_saved",
            Self::Periodic => "periodic",
        }
    }
}

/// Read `MUNI_CLEANUP_WARMUP` and return `false` only when it is
/// explicitly set to `"false"` (case-insensitive, trim-ws).
///
/// Default = enabled — the safe default for a latency-win feature
/// is "on"; the user opts out for A/B comparison. Mirrors how
/// `MUNI_LID_HYBRID` opts IN with `"true"`; we opt OUT with
/// `"false"`.
pub fn is_enabled() -> bool {
    !std::env::var(CLEANUP_WARMUP_ENV)
        .map(|v| v.trim().eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------
// Plan 041 (slice 4) — periodic prompt-cache re-warm.
//
// Groq evicts the per-account prompt-prefix cache after 2h of
// inactivity (~2.85s cold-cache tax measured, vs a 384ms warm floor).
// A 5-minute staleness tick that fires a re-warm once the cache has
// been idle ≥ 90 min keeps that tax off the user's next press without
// polling so aggressively that it wastes API calls. The 5-min-tick /
// 90-min-threshold split (rather than one long sleep) is deliberate:
// it self-heals across Mac sleep — a laptop that wakes from a 3h nap
// finds the very next tick already stale and fires immediately,
// instead of waiting out a timer that was scheduled before the sleep.
// ---------------------------------------------------------------------

/// Env var that disables the periodic cache re-warm at its spawn site.
/// Default behaviour (unset / any other value) is "re-warm enabled."
/// Independent of [`CLEANUP_WARMUP_ENV`] — that flag still gates
/// [`warmup_from_app`] itself (see [`should_fire_periodic_rewarm`]), so
/// disabling cleanup warm-up entirely also silences the periodic tick.
pub const CACHE_REWARM_ENV: &str = "MUNI_CACHE_REWARM";

/// How often the periodic re-warm checks staleness. Short relative to
/// the 90-min fire threshold so the self-heal-across-sleep property
/// holds without meaningfully polling `secs_since_prefix_touch()` (an
/// atomic load) more than necessary.
const REWARM_TICK_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Staleness threshold: fire a re-warm once the prompt-prefix cache has
/// gone this long without a real cleanup-shaped touch. Comfortably
/// inside Groq's 2h eviction window so the re-warm always lands before
/// the cache actually evicts.
const REWARM_STALENESS_THRESHOLD_SECS: i64 = 90 * 60;

/// Read `MUNI_CACHE_REWARM` and return `false` only when it is
/// explicitly set to `"false"` (case-insensitive, trim-ws). Mirrors
/// [`is_enabled`] — default enabled.
pub fn is_rewarm_enabled() -> bool {
    !std::env::var(CACHE_REWARM_ENV)
        .map(|v| v.trim().eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

/// Pure staleness gate: has the prompt-prefix cache gone idle long
/// enough to warrant a re-warm? Extracted so the 89min/90min boundary
/// is unit-testable without spinning up a timer or a Tauri app.
fn is_prefix_stale(secs_since_prefix_touch: i64) -> bool {
    secs_since_prefix_touch >= REWARM_STALENESS_THRESHOLD_SECS
}

/// Whether one periodic tick should fire a re-warm: both the
/// `MUNI_CACHE_REWARM` opt-out AND the staleness gate must agree.
/// Extracted from the tick loop so tests can drive both flags and the
/// gate math directly, without an `AppHandle`.
fn should_fire_periodic_rewarm(activity: &GroqActivity) -> bool {
    if !is_rewarm_enabled() {
        log::debug!(
            target: "groq_warmup",
            "periodic re-warm disabled via {CACHE_REWARM_ENV}=false"
        );
        return false;
    }
    is_prefix_stale(activity.secs_since_prefix_touch())
}

/// Spawn the detached periodic re-warm task. Runs for the lifetime of
/// the process; the caller drops the returned `JoinHandle` on the floor
/// (mirrors [`crate::groq_keepalive::spawn`] and
/// [`crate::prices_refresher::spawn`]).
///
/// `activity` is the SAME [`GroqActivity`] instance a real press's
/// cleanup call touches via `note_prefix_touch` — reading a clone would
/// observe a clock that never moves.
pub fn spawn_periodic_rewarm<R: Runtime>(
    app: AppHandle<R>,
    activity: Arc<GroqActivity>,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        run_periodic_rewarm(app, activity).await;
    })
}

/// Task body: 5-minute interval loop that checks staleness each tick
/// (subject to the [`should_fire_periodic_rewarm`] gate) and, when due,
/// calls [`warmup_from_app`] with [`WarmupTrigger::Periodic`].
/// `warmup_from_app`'s internal `MUNI_CLEANUP_WARMUP` gate stays
/// authoritative — both flags apply, this loop just adds the
/// staleness/opt-out check on top.
async fn run_periodic_rewarm<R: Runtime>(app: AppHandle<R>, activity: Arc<GroqActivity>) {
    log::info!(
        target: "groq_warmup",
        "periodic re-warm starting (tick={}s stale_threshold={}s)",
        REWARM_TICK_INTERVAL.as_secs(),
        REWARM_STALENESS_THRESHOLD_SECS
    );
    let mut ticker = tokio::time::interval(REWARM_TICK_INTERVAL);
    // Consume the immediate first tick — `tokio::time::interval` fires
    // at t=0, and the boot warm-up (if any) just stamped
    // `last_prefix_touch`; checking staleness before a full tick has
    // elapsed would be a redundant no-op at best.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        if should_fire_periodic_rewarm(&activity) {
            warmup_from_app(&app, WarmupTrigger::Periodic);
        }
    }
}

/// Spawn the detached warm-up task. Caller drops the returned join
/// handle on the floor — the task is fire-and-forget; never blocks
/// the caller and never panics.
///
/// Caller is responsible for gating on [`is_enabled`] BEFORE
/// calling — keeping the gate at the call site means the env-var
/// read happens once per trigger rather than once per task body
/// (cheap, but the explicit gate also documents the off-switch at
/// each call site).
#[allow(clippy::too_many_arguments)]
pub fn spawn_cleanup_warmup(
    client: Arc<GroqClient>,
    prompt: Arc<CleanupPrompt>,
    about_me: Arc<AboutMe>,
    vocabulary: Arc<Vocabulary>,
    user_prompt: Arc<UserPrompt>,
    usage_tx: Option<Sender<UsageRecord>>,
    activity: Option<Arc<GroqActivity>>,
    trigger: WarmupTrigger,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        run_warmup_inner(
            client,
            prompt,
            about_me,
            vocabulary,
            user_prompt,
            usage_tx,
            activity,
            trigger,
        )
        .await;
    })
}

/// Convenience entry point for IPC save handlers: pulls every Arc
/// out of Tauri-managed state and spawns the warm-up, or logs and
/// returns when any required state is missing.
///
/// Keeps each save handler a one-line call site so adding/removing
/// triggers stays cheap. Gates on [`is_enabled`] before touching any
/// state so the off-switch is honoured uniformly.
pub fn warmup_from_app<R: Runtime>(app: &AppHandle<R>, trigger: WarmupTrigger) {
    if !is_enabled() {
        log::debug!(
            target: "groq_warmup",
            "warmup disabled via {CLEANUP_WARMUP_ENV}=false (trigger={})",
            trigger.as_log_str()
        );
        return;
    }
    let Some(client) = app.try_state::<Arc<GroqClient>>() else {
        log::debug!(
            target: "groq_warmup",
            "skipping warmup: GroqClient not managed (trigger={})",
            trigger.as_log_str()
        );
        return;
    };
    let Some(prompt) = app.try_state::<Arc<CleanupPrompt>>() else {
        log::debug!(
            target: "groq_warmup",
            "skipping warmup: CleanupPrompt not managed (trigger={})",
            trigger.as_log_str()
        );
        return;
    };
    let Some(about_me) = app.try_state::<Arc<AboutMe>>() else {
        log::debug!(
            target: "groq_warmup",
            "skipping warmup: AboutMe not managed (trigger={})",
            trigger.as_log_str()
        );
        return;
    };
    let Some(vocabulary) = app.try_state::<Arc<Vocabulary>>() else {
        log::debug!(
            target: "groq_warmup",
            "skipping warmup: Vocabulary not managed (trigger={})",
            trigger.as_log_str()
        );
        return;
    };
    let Some(user_prompt) = app.try_state::<Arc<UserPrompt>>() else {
        log::debug!(
            target: "groq_warmup",
            "skipping warmup: UserPrompt not managed (trigger={})",
            trigger.as_log_str()
        );
        return;
    };
    let usage_tx = app
        .try_state::<Sender<UsageRecord>>()
        .map(|s| s.inner().clone());
    // Plan 041 (task 7) — pull the shared activity tracker so the
    // warm-up's success arm can stamp `note_prefix_touch` and close the
    // re-warm staleness loop. Absent in tests / early boot → the
    // warm-up still runs, it just doesn't reset the clock.
    let activity = app
        .try_state::<Arc<GroqActivity>>()
        .map(|a| Arc::clone(a.inner()));

    drop(spawn_cleanup_warmup(
        Arc::clone(client.inner()),
        Arc::clone(prompt.inner()),
        Arc::clone(about_me.inner()),
        Arc::clone(vocabulary.inner()),
        Arc::clone(user_prompt.inner()),
        usage_tx,
        activity,
        trigger,
    ));
}

/// Inner body of the warm-up task. Extracted from `spawn_cleanup_warmup`
/// so the await chain is unit-testable without going through
/// `tauri::async_runtime::spawn`.
#[allow(clippy::too_many_arguments)]
async fn run_warmup_inner(
    client: Arc<GroqClient>,
    prompt: Arc<CleanupPrompt>,
    about_me: Arc<AboutMe>,
    vocabulary: Arc<Vocabulary>,
    user_prompt: Arc<UserPrompt>,
    usage_tx: Option<Sender<UsageRecord>>,
    activity: Option<Arc<GroqActivity>>,
    trigger: WarmupTrigger,
) {
    let api_key = match secrets::get(secrets::GROQ_ACCOUNT) {
        Ok(k) => k,
        Err(_) => {
            log::info!(
                target: "groq_warmup",
                "warmup skipped: no API key (trigger={})",
                trigger.as_log_str()
            );
            return;
        }
    };

    // Snapshot under the read lock and clone before any await. The
    // press path does the same (see `session.rs::cleanup_with_groq`)
    // — `parking_lot::RwLock` is not async-safe and holding a guard
    // across the Groq HTTP call would deadlock under load.
    let about_me_text = about_me.text();
    let vocabulary_block = vocabulary.render_block();
    let user_prompt_text = user_prompt.text();
    let messages = match prompt.make_messages_with_context(
        WARMUP_USER_MESSAGE,
        &about_me_text,
        &vocabulary_block,
        &user_prompt_text,
    ) {
        Ok(m) => m,
        Err(err) => {
            log::warn!(
                target: "groq_warmup",
                "warmup failed: prompt compose error: {} (trigger={})",
                err.user_message(),
                trigger.as_log_str()
            );
            return;
        }
    };

    let cleanup_model = client.cleanup_model().to_string();
    let started = Instant::now();
    let request = GroqRequest::cleanup(messages, &cleanup_model);
    match client.complete(request, &api_key).await {
        Ok((_cleaned, usage)) => {
            // Plan 041 (task 7) — a successful warm-up warmed Groq's
            // prompt-prefix cache; stamp the staleness clock so the
            // periodic re-warm (slice 4) knows the cache is fresh and
            // won't re-fire until it goes stale again.
            if let Some(activity) = activity.as_ref() {
                activity.note_prefix_touch();
            }
            let elapsed = started.elapsed();
            let prompt_tokens = usage.map(|u| u.prompt_tokens);
            let completion_tokens = usage.map(|u| u.completion_tokens);
            let cached_tokens = usage.and_then(|u| u.cached_tokens);
            log::info!(
                target: "groq_warmup",
                "warmup ok: latency={}ms tokens_in={} cached_in={} tokens_out={} model={} trigger={}",
                elapsed.as_millis(),
                prompt_tokens.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
                cached_tokens.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
                completion_tokens.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
                cleanup_model,
                trigger.as_log_str()
            );
            if let Some(tx) = usage_tx.as_ref() {
                try_send_drop_oldest(
                    tx,
                    UsageRecord {
                        provider: PROVIDER_GROQ.into(),
                        model: cleanup_model.clone(),
                        call_kind: CALL_KIND_CLEANUP_WARMUP.into(),
                        audio_seconds: None,
                        input_tokens: prompt_tokens,
                        output_tokens: completion_tokens,
                        latency_ms: Some(elapsed.as_millis() as i64),
                        status: "ok".into(),
                        request_id: None,
                        session_id: None,
                        created_at_unix: unix_seconds_now(),
                    },
                );
            }
        }
        Err(err) => {
            log::warn!(
                target: "groq_warmup",
                "warmup failed: {} (trigger={})",
                err.user_message(),
                trigger.as_log_str()
            );
        }
    }
}

/// Wall-clock unix seconds (UTC), saturating to 0 on the impossible
/// pre-1970 case. Duplicates the private `session::unix_seconds_now`
/// (3 lines, 2 call sites) per CLAUDE.md §2: minimal repetition is
/// acceptable when it avoids leaking orchestrator internals across
/// module boundaries.
fn unix_seconds_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `MUNI_CLEANUP_WARMUP` is a process-global resource; serialize
    /// env-mutating tests so parallel cargo-test workers can't observe
    /// each other's writes. Same pattern as
    /// `secrets::tests::ENV_VAR_TEST_LOCK`.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn is_enabled_defaults_to_true_when_env_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(CLEANUP_WARMUP_ENV);
        assert!(is_enabled());
    }

    #[test]
    fn is_enabled_respects_explicit_false() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for value in ["false", "FALSE", "False", "  false  ", "fAlSe"] {
            std::env::set_var(CLEANUP_WARMUP_ENV, value);
            assert!(
                !is_enabled(),
                "MUNI_CLEANUP_WARMUP={value:?} should disable warmup"
            );
        }
        std::env::remove_var(CLEANUP_WARMUP_ENV);
    }

    #[test]
    fn is_enabled_treats_other_values_as_true() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for value in ["true", "1", "", "yes", "no", "0", "off", "garbage"] {
            std::env::set_var(CLEANUP_WARMUP_ENV, value);
            assert!(
                is_enabled(),
                "MUNI_CLEANUP_WARMUP={value:?} should keep warmup enabled (only explicit `false` disables)"
            );
        }
        std::env::remove_var(CLEANUP_WARMUP_ENV);
    }

    #[test]
    fn warmup_trigger_as_log_str_is_stable() {
        assert_eq!(WarmupTrigger::Boot.as_log_str(), "boot");
        assert_eq!(WarmupTrigger::PromptEdit.as_log_str(), "prompt_edit");
        assert_eq!(WarmupTrigger::AboutMe.as_log_str(), "about_me");
        assert_eq!(WarmupTrigger::Vocabulary.as_log_str(), "vocab");
        assert_eq!(WarmupTrigger::KeySaved.as_log_str(), "key_saved");
        assert_eq!(WarmupTrigger::Periodic.as_log_str(), "periodic");
    }

    /// `MUNI_CACHE_REWARM` is also a process-global resource; reuses the
    /// same `ENV_LOCK` since these tests never run concurrently with the
    /// `MUNI_CLEANUP_WARMUP` tests above (both share one test binary's
    /// serial env-mutation discipline).
    #[test]
    fn is_rewarm_enabled_defaults_to_true_when_env_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(CACHE_REWARM_ENV);
        assert!(is_rewarm_enabled());
    }

    #[test]
    fn is_rewarm_enabled_respects_explicit_false() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for value in ["false", "FALSE", "  false  ", "fAlSe"] {
            std::env::set_var(CACHE_REWARM_ENV, value);
            assert!(
                !is_rewarm_enabled(),
                "MUNI_CACHE_REWARM={value:?} should disable the periodic re-warm"
            );
        }
        std::env::remove_var(CACHE_REWARM_ENV);
    }

    #[test]
    fn is_rewarm_enabled_treats_other_values_as_true() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for value in ["true", "1", "", "yes", "no", "0", "off", "garbage"] {
            std::env::set_var(CACHE_REWARM_ENV, value);
            assert!(is_rewarm_enabled(), "only explicit `false` disables");
        }
        std::env::remove_var(CACHE_REWARM_ENV);
    }

    #[test]
    fn is_prefix_stale_boundary_is_90_minutes() {
        // 89 minutes idle: not yet stale.
        assert!(!is_prefix_stale(89 * 60));
        // Exactly 90 minutes: fires.
        assert!(is_prefix_stale(90 * 60));
        // Well past threshold: fires.
        assert!(is_prefix_stale(3 * 60 * 60));
        // Never touched (i64::MAX sentinel): always fires.
        assert!(is_prefix_stale(i64::MAX));
    }

    #[test]
    fn should_fire_periodic_rewarm_gates_on_staleness_and_flag() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(CACHE_REWARM_ENV);

        let activity = GroqActivity::new();
        // Fresh tracker (never touched) reads as maximally stale — fires.
        assert!(should_fire_periodic_rewarm(&activity));

        // 89 min idle: gate math says skip.
        activity.seed_last_prefix_touch_unix(now_unix() - 89 * 60);
        assert!(!should_fire_periodic_rewarm(&activity));

        // 90 min idle: gate math says fire.
        activity.seed_last_prefix_touch_unix(now_unix() - 90 * 60);
        assert!(should_fire_periodic_rewarm(&activity));

        // A fresh touch (as a successful warmup would stamp) suppresses
        // the next check until stale again.
        activity.note_prefix_touch();
        assert!(!should_fire_periodic_rewarm(&activity));

        // Even when stale, MUNI_CACHE_REWARM=false must suppress firing.
        activity.seed_last_prefix_touch_unix(now_unix() - 90 * 60);
        std::env::set_var(CACHE_REWARM_ENV, "false");
        assert!(!should_fire_periodic_rewarm(&activity));

        std::env::remove_var(CACHE_REWARM_ENV);
    }

    fn now_unix() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}
