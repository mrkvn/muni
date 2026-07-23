use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutEvent, ShortcutState};
use tauri_plugin_store::StoreExt;

pub mod about_me;
pub mod asr_stream;
mod audio;
mod audio_devices_watcher;
pub mod audio_lid;
mod boot_health;
mod commands;
// `deepgram`, `groq`, `error`, `prompt`, `injection`, `session`, and `tray`
// are exposed so integration tests under `apps/desktop/src-tauri/tests/` can
// drive the clients (and the orchestrator) against local mocks. See
// `tests/deepgram_mock.rs`, `tests/groq_mock.rs`, `tests/injector_mock.rs`,
// `tests/session_warming.rs`, and `tests/end_to_end.rs`. `hud` is exposed
// purely to keep its small public surface (controller + helpers) navigable
// from doctests if we add them later.
pub mod deepgram;
pub mod deepgram_pool;
pub mod error;
pub mod error_presenter;
pub mod gemini_lid;
pub mod gladia;
pub mod groq;
pub mod groq_activity;
pub mod groq_keepalive;
pub mod groq_lid;
pub mod groq_warmup;
pub mod groq_whisper;
pub mod history_store;
mod hotkey;
pub mod hotkey_binding;
pub mod hud;
pub mod injection;
pub mod launch_item;
pub mod my_words;
pub mod parakeet;
pub mod permissions;
pub mod press_timing;
pub mod prices_client;
pub mod prices_refresher;
pub mod pricing;
pub mod prompt;
pub mod secrets;
pub mod self_correction;
pub mod session;
pub mod settings;
// Feature 033 — post-launch monitoring (Sentry crash spine; PostHog later).
mod telemetry;
pub mod text_lid;
pub mod tray;
pub mod updater;
pub mod usage_store;
pub mod usage_writer;
pub mod user_prompt;
pub mod vad;
pub mod vocabulary;

use about_me::AboutMe;
use audio::AudioCapture;
use audio_lid::{AudioLidClassifier, WhisperAudioLid};
use error::MuniError;
use error_presenter::app_handle_presenter;
use gemini_lid::GeminiLidClient;
use groq::GroqClient;
use groq_activity::GroqActivity;
use groq_lid::GroqLidClient;
use groq_whisper::GroqWhisperClient;
use history_store::HistoryStore;
use hud::HudController;
use injection::{default_injector, PlatformInjector};
use my_words::MyWords;
use pricing::DEFAULT_PRICES;
use prompt::CleanupPrompt;
use session::{
    app_handle_emitter, BilingualModeFlag, DeepgramPool, DictationSession, EnglishFastModeFlag,
    MicSilencedFlag, SessionDeps, SessionState, SessionStateTracker, StateNotifier,
};
use text_lid::TextLidClassifier;
use tray::TrayState;
use usage_store::{PriceRow, UsageStore, API_CALLS_RETENTION_DAYS};
use user_prompt::UserPrompt;
use vocabulary::Vocabulary;

/// Components needed to arm the press → audio → Deepgram → cleanup
/// pipeline. Held in app state until the hotkey listener can be
/// safely started — that is, after the user grants Input Monitoring
/// via the onboarding wizard. Wrapping in `Mutex<Option<_>>` lets
/// `arm_hotkey_pipeline` consume the contents exactly once and turn
/// any subsequent call into a no-op.
///
/// On a returning user (`did_complete_onboarding=true`) the bundle
/// is consumed inside `setup` and never reaches app state; on a
/// fresh install it sits idle until the wizard's Finish step calls
/// `complete_onboarding`, which delegates to
/// [`arm_hotkey_pipeline`].
pub struct PendingHotkeyArm {
    inner: Mutex<Option<PendingHotkeyArmInner>>,
}

struct PendingHotkeyArmInner {
    session: Arc<DictationSession>,
    audio: Arc<AudioCapture>,
    debug_dir: Option<PathBuf>,
}

impl PendingHotkeyArm {
    fn new(inner: PendingHotkeyArmInner) -> Self {
        Self {
            inner: Mutex::new(Some(inner)),
        }
    }

    fn take(&self) -> Option<PendingHotkeyArmInner> {
        // Plan 039 task 33 — recover from a poisoned mutex via `into_inner()`
        // instead of silently returning `None`. A poisoned lock here would
        // otherwise strand the pending bundle forever (the hotkey pipeline would
        // never arm), which is worse than reading the still-intact bundle out of
        // a poisoned guard.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// Restore the bundle so a subsequent retry can attempt the
    /// hotkey install again. Used when `HotkeyManager::start` fails
    /// because the user hasn't granted Input Monitoring yet — the
    /// failure is the expected path for the wizard's IM step, where
    /// the failed install is precisely what surfaces the macOS
    /// Keystroke Receiving prompt.
    fn put_back(&self, inner: PendingHotkeyArmInner) {
        // Plan 039 task 33 — poison-safe restore (mirror `take`): recover a
        // poisoned lock rather than dropping the bundle on the floor, which
        // would prevent any retry from re-arming the pipeline.
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Defensive: only restore when nothing else has stuffed a fresh bundle
        // into the slot. There's no concurrent writer today, but matching Mutex
        // semantics keeps the contract honest if a future caller arrives.
        if guard.is_none() {
            *guard = Some(inner);
        }
    }
}

/// Feature 037 — the managed re-paste global-shortcut controller.
///
/// Owns the accelerator currently registered with
/// `tauri-plugin-global-shortcut` and knows how to (re)bind or clear it. Boot
/// registers the stored binding once via [`Self::apply`]; `set_repaste_hotkey`
/// reaches the same managed instance through Tauri state to apply a rebind
/// **live** (no restart). Every plugin call hops to the main thread because the
/// underlying `global_hotkey` crate uses main-thread-only Carbon APIs on macOS.
///
/// Unlike the toggle-session Esc/Enter controller ([`hotkey::HotkeyManager`]),
/// this shortcut is registered **persistently** — it consumes its combo
/// system-wide for the whole time Muni runs (brainstorm 014 Q3/Q12).
/// Plan 039 task 51(a) — event the backend emits when a hotkey shortcut fails to
/// register with the OS (combo already held). The frontend toasts it and reverts
/// the optimistic value. Payload: `{ target: "dictation" | "repaste", accel }`.
const EVENT_HOTKEY_REGISTRATION_FAILED: &str = "hotkey://registration-failed";

/// Handle a keyed-shortcut OS registration failure (plan 039 task 51a): surface
/// it to the UI *and* heal the persisted store so the displayed binding always
/// matches what is actually registered. Called from either controller's
/// main-thread hop when `on_shortcut` rejects the accelerator (combo already
/// held by the system or another app).
///
/// Without the heal, the store keeps a binding the OS never accepted: the
/// Shortcuts UI shows a chord that isn't live, and every subsequent boot
/// re-attempts the same doomed registration (WARN + re-toast) on a loop. The
/// heal degrades to a value that is guaranteed registrable, so the failure
/// self-resolves after one occurrence.
fn fail_registration(app: &AppHandle, target: &str, accel: &str) {
    emit_registration_failed(app, target, accel);
    heal_failed_registration(app, target);
}

/// Emit [`EVENT_HOTKEY_REGISTRATION_FAILED`] so the Shortcuts UI can surface a
/// failed OS registration. Best-effort — a dropped event just means no toast.
fn emit_registration_failed(app: &AppHandle, target: &str, accel: &str) {
    let _ = app.emit(
        EVENT_HOTKEY_REGISTRATION_FAILED,
        json!({ "target": target, "accel": accel }),
    );
}

/// Heal the persisted store after a failed OS registration so boot and UI agree
/// with the live state (plan 039 task 51a). The chosen fallback is always
/// registrable, so the next boot won't re-loop:
/// - **dictation** heals to its modifier-only default (`Ctrl+Option`), driven by
///   the flags-tap — it registers no global shortcut, so it can never fail.
/// - **re-paste** heals to disabled (`null`) — it registers nothing.
///
/// Also live-applies the fallback (re-enabling the dictation flags-tap /
/// unregistering the dead re-paste accel) so the *current* session is coherent,
/// not just the next boot. Emits `settings://changed` so a mounted Shortcuts
/// screen re-renders the healed value. Best-effort throughout: a failed store
/// write just leaves the next boot to heal via `load_*_binding`'s own guard.
fn heal_failed_registration(app: &AppHandle, target: &str) {
    let Ok(store) = app.store(settings::SETTINGS_FILE) else {
        return;
    };
    match target {
        "dictation" => {
            let default = hotkey_binding::DictationBinding::default_dictation();
            let Ok(value) = serde_json::to_value(&default) else {
                return;
            };
            store.set(settings::KEY_HOTKEY_DICTATION_BINDING, value.clone());
            let _ = store.save();
            let _ = app.emit(
                settings::EVENT_SETTINGS_CHANGED,
                json!({ "key": settings::KEY_HOTKEY_DICTATION_BINDING, "value": value }),
            );
            // Re-enable the flags-tap for the modifier-only default and drop the
            // dead keyed accel, so dictation keeps working this session.
            if let Some(trigger) = app.try_state::<Arc<hotkey::HotkeyTriggerState>>() {
                trigger.apply(&default);
            }
            if let Some(controller) = app.try_state::<Arc<DictationShortcutController>>() {
                controller.apply(&default);
            }
        }
        "repaste" => {
            store.set(
                settings::KEY_HOTKEY_REPASTE_BINDING,
                serde_json::Value::Null,
            );
            let _ = store.save();
            let _ = app.emit(
                settings::EVENT_SETTINGS_CHANGED,
                json!({ "key": settings::KEY_HOTKEY_REPASTE_BINDING, "value": serde_json::Value::Null }),
            );
            // Unregister the dead accel so no stale registration lingers.
            if let Some(controller) = app.try_state::<Arc<RepasteController>>() {
                controller.apply(None);
            }
        }
        _ => {
            log::warn!(target: "hotkey", "heal_failed_registration: unknown target {target:?}");
        }
    }
}

/// Plan 039 task 51(c) — how long a recording-suppression may stay armed before
/// the watchdog force-clears it. Comfortably longer than any real recorder
/// interaction; the only thing it guards is an orphaned suppression (settings
/// window closed/reloaded/crashed mid-record without the paired `false`).
const RECORDING_SUPPRESSION_WATCHDOG: Duration = Duration::from_secs(60);

/// Plan 039 task 51(c) — generation counter behind the recording-suppression
/// watchdog. Every `hotkey_set_recording` call bumps it; an armed watchdog only
/// fires if its captured generation is still current when the timeout elapses,
/// so a later `false` (normal end) or a newer `true` (re-arm) cancels it.
#[derive(Default)]
pub struct RecordingWatchdog {
    generation: AtomicU64,
}

impl RecordingWatchdog {
    /// Bump and return the new generation, invalidating any pending watchdog.
    fn bump(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// The current generation (an armed watchdog compares against this).
    fn current(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

/// Force the recording-suppression off across all sources (trigger flags-tap +
/// both keyed controllers). Used by the watchdog and reachable for an explicit
/// disarm; each lookup tolerates absence (pre-arm / onboarding).
pub(crate) fn force_disarm_recording<R: tauri::Runtime>(app: &AppHandle<R>) {
    if let Some(trigger) = app.try_state::<Arc<hotkey::HotkeyTriggerState>>() {
        trigger.set_suppressed(false);
    }
    if let Some(controller) = app.try_state::<Arc<DictationShortcutController>>() {
        controller.set_recording(false);
    }
    if let Some(controller) = app.try_state::<Arc<RepasteController>>() {
        controller.set_recording(false);
    }
}

/// Arm the recording-suppression watchdog: after [`RECORDING_SUPPRESSION_WATCHDOG`]
/// force-disarm suppression IF this generation is still current (nothing newer
/// bumped the counter). Cheap: one sleeping task per record-start, self-cancels.
fn arm_recording_watchdog<R: tauri::Runtime>(app: AppHandle<R>, generation: u64) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(RECORDING_SUPPRESSION_WATCHDOG).await;
        let Some(watchdog) = app.try_state::<RecordingWatchdog>() else {
            return;
        };
        if watchdog.current() != generation {
            // A set_recording(false) or a newer set_recording(true) superseded
            // this watchdog — the suppression was already handled.
            return;
        }
        log::warn!(
            target: "hotkey",
            "recording suppression watchdog fired ({RECORDING_SUPPRESSION_WATCHDOG:?}) — clearing orphaned suppression"
        );
        force_disarm_recording(&app);
    });
}

/// Plan 039 task 51(c) — apply a recording-suppression state change and manage
/// the watchdog. `true` suppresses every trigger source and arms a timeout;
/// `false` resumes them and (by bumping the generation) cancels any pending
/// watchdog. Called by the `hotkey_set_recording` command.
pub(crate) fn set_recording_suppression<R: tauri::Runtime>(
    app: &AppHandle<R>,
    trigger: &hotkey::HotkeyTriggerState,
    watchdog: &RecordingWatchdog,
    recording: bool,
) {
    trigger.set_suppressed(recording);
    // Feature 038 — a keyed dictation binding registers a *consuming* global
    // shortcut, so a flag-ignore is insufficient: unregister it for the window.
    if let Some(controller) = app.try_state::<Arc<DictationShortcutController>>() {
        controller.set_recording(recording);
    }
    // Plan 039 task 49(b) — the re-paste shortcut is the same kind of consuming
    // shortcut, so suppress it for the recording window too.
    if let Some(controller) = app.try_state::<Arc<RepasteController>>() {
        controller.set_recording(recording);
    }

    // Task 51c: bump the generation (cancels any pending watchdog); re-arm only
    // when starting a new recording window.
    let generation = watchdog.bump();
    if recording {
        arm_recording_watchdog(app.clone(), generation);
    }
}

pub struct RepasteController {
    app: AppHandle,
    /// Same injector instance the orchestrator uses, so re-paste reuses the
    /// snapshot→write→Cmd+V→restore path and never clobbers the real clipboard.
    injector: Arc<dyn PlatformInjector>,
    /// The full history store, so the newest recorded dictation is always
    /// re-pastable. `None` only when the store failed to open at boot.
    history: Option<Arc<HistoryStore>>,
    /// The accelerator string currently registered, so a rebind knows what to
    /// unregister. Tracks the *intended* accelerator: on a failed register the
    /// later `unregister` of a never-registered accel is a harmless no-op.
    current_accel: Mutex<Option<String>>,
    /// Plan 039 task 48(b) — the last edge forwarded to the re-paste handler, so
    /// a duplicate `Pressed` (auto-repeat / plugin double-fire) is filtered
    /// before it can spawn a second re-paste. Mirrors the dictation controller's
    /// `held` transition guard.
    held: Arc<AtomicBool>,
    /// Plan 039 task 48(b) — set while a re-paste is executing so further presses
    /// are dropped (not queued) until the in-flight paste completes.
    in_flight: Arc<AtomicBool>,
    /// Plan 039 task 51(d) — bumped on every `apply`, captured into the
    /// main-thread dispatch, so a superseded (stale) rebind's dispatch is
    /// skipped rather than interleaving a dead registration.
    apply_version: Arc<AtomicU64>,
}

impl RepasteController {
    fn new(
        app: AppHandle,
        injector: Arc<dyn PlatformInjector>,
        history: Option<Arc<HistoryStore>>,
    ) -> Self {
        Self {
            app,
            injector,
            history,
            current_accel: Mutex::new(None),
            held: Arc::new(AtomicBool::new(false)),
            in_flight: Arc::new(AtomicBool::new(false)),
            apply_version: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Register the re-paste shortcut on the main thread, wiring the transition
    /// guard + in-flight drop. Shared by `apply` and `set_recording(false)` so a
    /// fresh bind and a resume-after-recording get the identical handler. MUST be
    /// called from within a `run_on_main_thread` hop.
    ///
    /// Plan 039 task 51(b): idempotent — if the accelerator is already
    /// registered (e.g. `apply` just registered it and the commit's
    /// `set_recording(false)` re-registers), this skips rather than logging the
    /// misleading "combo taken by the OS?" WARN on every commit.
    fn register(
        app: &AppHandle,
        accel: &str,
        injector: Arc<dyn PlatformInjector>,
        history: Option<Arc<HistoryStore>>,
        held: Arc<AtomicBool>,
        in_flight: Arc<AtomicBool>,
    ) {
        let gs = app.global_shortcut();
        if gs.is_registered(accel) {
            log::debug!(
                target: "hotkey",
                "re-paste shortcut already registered ({accel}) — skipping re-register"
            );
            return;
        }
        // Fresh registration: Carbon fires no `Pressed` for a key held at
        // registration time, so reset the transition guard to a released
        // baseline (mirrors the dictation controller).
        held.store(false, Ordering::Release);
        let handler = move |_app: &_, _shortcut: &_, event: ShortcutEvent| {
            let pressed = event.state == ShortcutState::Pressed;
            // Filter duplicate edges (auto-repeat / double-fire); only a genuine
            // press re-pastes.
            if !is_dictation_transition(&held, pressed) {
                return;
            }
            if !pressed {
                return;
            }
            spawn_repaste(
                Arc::clone(&injector),
                history.clone(),
                Arc::clone(&in_flight),
            );
        };
        match gs.on_shortcut(accel, handler) {
            Ok(()) => log::info!(target: "hotkey", "re-paste shortcut registered ({accel})"),
            Err(err) => {
                log::warn!(
                    target: "hotkey",
                    "re-paste shortcut register {accel} failed (combo taken by the OS?): {err}"
                );
                fail_registration(app, "repaste", accel);
            }
        }
    }

    /// Rebind (or clear) the re-paste shortcut. `None` disables it entirely.
    ///
    /// Unregisters whatever accelerator is currently bound, then — when a
    /// binding is supplied — registers the new one. Both plugin calls run in a
    /// single main-thread hop so they can never interleave with a concurrent
    /// rebind. A stale rebind (superseded by a newer `apply` before its dispatch
    /// runs) is skipped via the version counter (task 51d).
    pub(crate) fn apply(&self, binding: Option<&hotkey_binding::RepasteBinding>) {
        let new_accel = binding.map(|b| b.accelerator());
        // Swap `current_accel` and bump `apply_version` as ONE critical section
        // under the same lock (task 51d): the version must totalize on the same
        // order as the `current_accel` swaps, or a racing pair can capture
        // versions in the opposite order from their swaps and leave the older
        // binding registered while `current_accel` names the newer one.
        let (old_accel, version) = {
            let mut guard = self
                .current_accel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let old = std::mem::replace(&mut *guard, new_accel.clone());
            let version = self.apply_version.fetch_add(1, Ordering::AcqRel) + 1;
            (old, version)
        };

        let app = self.app.clone();
        let injector = Arc::clone(&self.injector);
        let history = self.history.clone();
        let held = Arc::clone(&self.held);
        let in_flight = Arc::clone(&self.in_flight);
        let apply_version = Arc::clone(&self.apply_version);
        let app_for_dispatch = app.clone();
        let dispatch = move || {
            // Task 51d: a newer apply may have superseded this dispatch before it
            // reached the main thread. We still unregister our old accel (see
            // `plan_rebind_steps`) so a superseded rebind can't leak a live stale
            // registration; only the register half is skipped.
            let superseded = rebind_superseded(version, &apply_version);
            if superseded {
                log::debug!(
                    target: "hotkey",
                    "re-paste rebind v{version} superseded — unregistering stale accel, skipping register"
                );
            }
            let gs = app_for_dispatch.global_shortcut();
            for step in plan_rebind_steps(old_accel, new_accel, superseded) {
                match step {
                    // Ignore the error: unregistering an accel that never
                    // registered (a prior failed bind) or that the OS holds is
                    // expected.
                    RebindStep::Unregister(old) => {
                        let _ = gs.unregister(old.as_str());
                    }
                    RebindStep::Register(new) => Self::register(
                        &app_for_dispatch,
                        &new,
                        Arc::clone(&injector),
                        history.clone(),
                        Arc::clone(&held),
                        Arc::clone(&in_flight),
                    ),
                    RebindStep::ClearHeld => {
                        held.store(false, Ordering::Release);
                        log::info!(target: "hotkey", "re-paste shortcut disabled");
                    }
                }
            }
        };

        if let Err(err) = app.run_on_main_thread(dispatch) {
            log::warn!(
                target: "hotkey",
                "run_on_main_thread for re-paste (re)bind failed: {err}"
            );
        }
    }

    /// Plan 039 task 49(b) — unregister (recording start) or re-register
    /// (recording end) the re-paste shortcut around the Shortcuts recorder
    /// window, mirroring [`DictationShortcutController::set_recording`]. A keyed
    /// shortcut consumes its combo at the OS level regardless of the handler, so
    /// holding the current re-paste chord to re-record it would fire a re-paste
    /// and hide the key from the webview. `current_accel` is left intact so
    /// resume re-registers the same combo.
    pub(crate) fn set_recording(&self, recording: bool) {
        // Capture the accel and the current apply_version as one snapshot under
        // the lock (task 51d): a concurrent `apply` that has since bumped the
        // counter now owns the live registration, so a resume re-register that
        // captured the older version must yield to it rather than resurrect a
        // stale accel — exactly the heal-then-resume interleave the counter
        // guards. We snapshot the version (not bump it): `set_recording` is a
        // suppress/resume around a rebind, not a rebind itself.
        let (accel, version) = {
            let guard = self
                .current_accel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (guard.clone(), self.apply_version.load(Ordering::Acquire))
        };
        let Some(accel) = accel else {
            return;
        };

        let app = self.app.clone();
        let injector = Arc::clone(&self.injector);
        let history = self.history.clone();
        let held = Arc::clone(&self.held);
        let in_flight = Arc::clone(&self.in_flight);
        let apply_version = Arc::clone(&self.apply_version);
        let app_for_dispatch = app.clone();
        let dispatch = move || {
            if recording {
                let _ = app_for_dispatch
                    .global_shortcut()
                    .unregister(accel.as_str());
                held.store(false, Ordering::Release);
                log::info!(
                    target: "hotkey",
                    "re-paste shortcut suppressed for recording ({accel})"
                );
            } else {
                // A rebind that landed while recording was open supersedes this
                // resume — its own dispatch registers the current accel, so
                // re-registering the snapshotted (stale) accel here would leak a
                // dead registration the version counter exists to prevent.
                if rebind_superseded(version, &apply_version) {
                    log::debug!(
                        target: "hotkey",
                        "re-paste resume superseded by a newer rebind — skipping re-register of {accel}"
                    );
                    return;
                }
                Self::register(
                    &app_for_dispatch,
                    &accel,
                    injector,
                    history,
                    held,
                    in_flight,
                );
            }
        };
        if let Err(err) = app.run_on_main_thread(dispatch) {
            log::warn!(
                target: "hotkey",
                "run_on_main_thread for re-paste recording toggle failed: {err}"
            );
        }
    }
}

/// Decide whether a shortcut edge is a real transition worth forwarding.
///
/// Swaps the tracked `held` flag to `both_held` and returns `true` only when
/// the value actually changed. This is the mandatory transition guard: a
/// duplicate `Pressed`/`Released` from the OS (auto-repeat, or the plugin
/// double-firing an edge) is filtered here, before it can reach the untouched
/// hold/tap/toggle state machine — where a stray `false` would read as
/// release→commit and a spurious `Pressed/Released` pair as tap→toggle.
fn is_dictation_transition(held: &AtomicBool, both_held: bool) -> bool {
    held.swap(both_held, Ordering::AcqRel) != both_held
}

/// Plan 039 task 51(d) — decide whether a queued (re)bind dispatch has been
/// superseded by a newer `apply` before it ran on the main thread.
///
/// Each `apply` bumps the controller's `apply_version` and captures the new
/// value into its dispatch closure. When the dispatch finally runs, it compares
/// its captured `version` against the live counter: if a later `apply` moved the
/// counter on, that newer rebind now owns the registration and this stale
/// dispatch must skip rather than interleave a dead (unregister/register)
/// sequence. Split out so the supersede decision is unit-testable without a live
/// `AppHandle` / global-shortcut plugin.
fn rebind_superseded(captured_version: u64, current: &AtomicU64) -> bool {
    current.load(Ordering::Acquire) != captured_version
}

/// Plan 039 task 51(d) — one plugin action a (re)bind dispatch performs, in the
/// order [`plan_rebind_steps`] emits them.
#[derive(Debug, PartialEq, Eq)]
enum RebindStep {
    /// Unregister the previously-bound accelerator (idempotent).
    Unregister(String),
    /// Register the newly-bound accelerator as the live shortcut.
    Register(String),
    /// No keyed accel is live afterwards (re-paste disabled / modifier-only
    /// dictation): reset the transition tracker so a later keyed rebind starts
    /// from a released baseline.
    ClearHeld,
}

/// Plan 039 task 51(d) — the ordered plugin actions a controller's (re)bind
/// dispatch must run, given its captured old/new accelerators and whether a
/// newer `apply` superseded it before it reached the main thread. Split out from
/// the main-thread closure so this ordering is unit-testable without a live
/// global-shortcut plugin (the closure only executes the returned steps).
///
/// The old accel's `Unregister` is emitted **unconditionally** — including for a
/// superseded dispatch — because dropping it is exactly the leak the version
/// counter exists to prevent. Two racing rebinds (Z→A then A→B) queue their
/// dispatches; the older (Z→A) finds itself superseded. If it returned *before*
/// unregistering, its old accel `Z` would stay registered forever while the
/// newer rebind only ever unregisters `A` — leaving both `Z` and `B` live.
/// Unregistering a stale or never-registered accel is idempotent and dispatches
/// run FIFO, so the stale unregister always precedes the newer register. Only
/// the `Register`/`ClearHeld` half is gated on `!superseded`: the newest rebind
/// owns the final live registration.
fn plan_rebind_steps(
    old_accel: Option<String>,
    new_accel: Option<String>,
    superseded: bool,
) -> Vec<RebindStep> {
    let mut steps = Vec::with_capacity(2);
    if let Some(old) = old_accel {
        steps.push(RebindStep::Unregister(old));
    }
    if superseded {
        // A newer apply owns the final registration; skip only the register half.
        return steps;
    }
    match new_accel {
        Some(new) => steps.push(RebindStep::Register(new)),
        None => steps.push(RebindStep::ClearHeld),
    }
    steps
}

/// Feature 038 — owns the consuming global shortcut for a **keyed** dictation
/// binding (a modifier+key combo, e.g. `Ctrl+Shift+R`). A modifier-only binding
/// is driven by the CGEvent flags-tap instead, so this controller registers
/// nothing then — the two edge sources are mutually exclusive by construction
/// (the flags-tap gates itself off via `HotkeyTriggerState::key_bound`).
///
/// The shortcut's `Pressed`/`Released` edges feed the untouched state machine
/// through a typed [`hotkey::DictationEdgeSender`], mirroring the flags-tap's
/// `both_held` contract, so PTT and quick-tap-toggle both work for keyed
/// bindings for free. Every plugin call hops to the main thread because the
/// underlying Carbon APIs are main-thread-only — the exact discipline of
/// [`RepasteController`].
pub struct DictationShortcutController {
    app: AppHandle,
    /// Typed edge feed into the hotkey state machine. Cloned into each shortcut
    /// handler closure so a `Pressed`/`Released` becomes a `FlagsChanged` edge
    /// without exposing the private `HotkeyMsg` channel.
    edge: hotkey::DictationEdgeSender,
    /// The accelerator currently registered (`None` for a modifier-only binding,
    /// which registers nothing). Tracks the *intended* accelerator so a rebind
    /// knows what to unregister and `set_recording` can re-register the same
    /// combo after the recording window.
    current_accel: Mutex<Option<String>>,
    /// The last edge forwarded to the state machine, shared between the handler
    /// closure and `set_recording` (which resets it to `false` whenever the
    /// shortcut is torn down, so resume can't emit a spurious release). See
    /// [`is_dictation_transition`].
    held: Arc<AtomicBool>,
    /// Plan 039 task 51(d) — bumped on every `apply`, captured into the
    /// main-thread dispatch so a superseded rebind's dispatch is skipped.
    apply_version: Arc<AtomicU64>,
}

impl DictationShortcutController {
    fn new(app: AppHandle, edge: hotkey::DictationEdgeSender) -> Self {
        Self {
            app,
            edge,
            current_accel: Mutex::new(None),
            held: Arc::new(AtomicBool::new(false)),
            apply_version: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Register a keyed dictation shortcut on the main thread. Shared by `apply`
    /// and `set_recording(false)` so the handler (edge feed + transition guard)
    /// is wired identically on a fresh bind and on resume-after-recording. MUST
    /// be called from within a `run_on_main_thread` hop (Carbon is main-only).
    fn register(
        app: &AppHandle,
        accel: &str,
        edge: hotkey::DictationEdgeSender,
        held: Arc<AtomicBool>,
    ) {
        // Plan 039 task 51(b): idempotent — skip if already registered so the
        // commit flow's `apply` + `set_recording(false)` double-register doesn't
        // log the misleading "combo taken by the OS?" WARN on every commit.
        let gs = app.global_shortcut();
        if gs.is_registered(accel) {
            log::debug!(
                target: "hotkey",
                "keyed dictation shortcut already registered ({accel}) — skipping re-register"
            );
            return;
        }
        // Start from a released baseline: Carbon fires no `Pressed` for a key
        // already held at registration time, so the first edge we'll ever see is
        // a `Pressed`. This resets `held` on every (re)bind and on resume-after-
        // recording, so a stale `true` can never leak an edge into the machine.
        held.store(false, Ordering::Release);
        let handler = move |_app: &_, _shortcut: &_, event: ShortcutEvent| {
            // The plugin fires on both edges; `Released` means the key lifted.
            let both_held = event.state == ShortcutState::Pressed;
            if is_dictation_transition(&held, both_held) {
                edge.send_edge(both_held);
            }
        };
        match gs.on_shortcut(accel, handler) {
            Ok(()) => {
                log::info!(target: "hotkey", "keyed dictation shortcut registered ({accel})")
            }
            Err(err) => {
                log::warn!(
                    target: "hotkey",
                    "keyed dictation shortcut register {accel} failed (combo taken by the OS?): {err}"
                );
                fail_registration(app, "dictation", accel);
            }
        }
    }

    /// (Re)bind the keyed dictation shortcut from a binding. A keyed binding
    /// registers a consuming global shortcut; a modifier-only binding registers
    /// nothing (the flags-tap owns the edge) and resets the transition guard.
    ///
    /// Mirrors [`RepasteController::apply`]: swap `current_accel`, capture the
    /// old, then in one main-thread hop unregister the old and register the new.
    /// A failed register (combo already held by the OS) is a WARN, never fatal.
    pub(crate) fn apply(&self, binding: &hotkey_binding::DictationBinding) {
        let new_accel = binding.accelerator();
        // Swap `current_accel` and bump `apply_version` atomically under the same
        // lock (task 51d) — see `RepasteController::apply` for why the two must
        // totalize on one order.
        let (old_accel, version) = {
            let mut guard = self
                .current_accel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let old = std::mem::replace(&mut *guard, new_accel.clone());
            let version = self.apply_version.fetch_add(1, Ordering::AcqRel) + 1;
            (old, version)
        };

        let app = self.app.clone();
        let edge = self.edge.clone();
        let held = Arc::clone(&self.held);
        let apply_version = Arc::clone(&self.apply_version);
        let app_for_dispatch = app.clone();
        let dispatch = move || {
            // Task 51d: a newer apply may have superseded this dispatch. We still
            // unregister our old accel (see `plan_rebind_steps`) so a superseded
            // rebind can't leak a live stale registration; only the register half
            // is skipped.
            let superseded = rebind_superseded(version, &apply_version);
            if superseded {
                log::debug!(
                    target: "hotkey",
                    "keyed dictation rebind v{version} superseded — unregistering stale accel, skipping register"
                );
            }
            let gs = app_for_dispatch.global_shortcut();
            for step in plan_rebind_steps(old_accel, new_accel, superseded) {
                match step {
                    // Ignore the error: unregistering an accel that never
                    // registered (a prior failed bind or a modifier-only binding)
                    // is expected.
                    RebindStep::Unregister(old) => {
                        let _ = gs.unregister(old.as_str());
                    }
                    RebindStep::Register(new) => Self::register(
                        &app_for_dispatch,
                        new.as_str(),
                        edge.clone(),
                        Arc::clone(&held),
                    ),
                    // Modifier-only: the flags-tap drives the edge. Register
                    // nothing and clear the transition tracker so a later keyed
                    // rebind starts from a known-released baseline.
                    RebindStep::ClearHeld => {
                        held.store(false, Ordering::Release);
                        log::info!(
                            target: "hotkey",
                            "keyed dictation shortcut disabled (modifier-only binding)"
                        );
                    }
                }
            }
        };

        if let Err(err) = app.run_on_main_thread(dispatch) {
            log::warn!(
                target: "hotkey",
                "run_on_main_thread for keyed dictation (re)bind failed: {err}"
            );
        }
    }

    /// Unregister (recording start) or re-register (recording end) the keyed
    /// dictation shortcut around the Shortcuts recorder window.
    ///
    /// A `suppressed` flag is insufficient: a Carbon registration consumes the
    /// key at the OS level regardless of the handler, so holding the current
    /// combo to re-record it would both fire dictation and hide the key from the
    /// webview (brainstorm Q5). `current_accel` is left intact so resume
    /// re-registers the same combo; `held` resets on suppress so the machine
    /// can't see a stale edge on resume. Modifier-only bindings are a no-op here
    /// (the flags-tap side is handled by `HotkeyTriggerState::set_suppressed`).
    pub(crate) fn set_recording(&self, recording: bool) {
        // Snapshot accel + apply_version together under the lock (task 51d):
        // mirrors `RepasteController::set_recording` — a rebind that landed while
        // recording was open supersedes this resume, so the stale re-register is
        // version-gated below.
        let (accel, version) = {
            let guard = self
                .current_accel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (guard.clone(), self.apply_version.load(Ordering::Acquire))
        };
        let Some(accel) = accel else {
            return;
        };

        let app = self.app.clone();
        let edge = self.edge.clone();
        let held = Arc::clone(&self.held);
        let apply_version = Arc::clone(&self.apply_version);
        let app_for_dispatch = app.clone();
        let dispatch = move || {
            if recording {
                let _ = app_for_dispatch
                    .global_shortcut()
                    .unregister(accel.as_str());
                held.store(false, Ordering::Release);
                log::info!(
                    target: "hotkey",
                    "keyed dictation shortcut suppressed for recording ({accel})"
                );
            } else {
                // A newer rebind owns the live registration — skip resurrecting
                // the snapshotted stale accel (see repaste equivalent).
                if rebind_superseded(version, &apply_version) {
                    log::debug!(
                        target: "hotkey",
                        "keyed dictation resume superseded by a newer rebind — skipping re-register of {accel}"
                    );
                    return;
                }
                Self::register(&app_for_dispatch, accel.as_str(), edge, held);
            }
        };
        if let Err(err) = app.run_on_main_thread(dispatch) {
            log::warn!(
                target: "hotkey",
                "run_on_main_thread for keyed dictation recording toggle failed: {err}"
            );
        }
    }
}

/// Plan 039 task 48(b) — RAII guard that clears the re-paste in-flight flag when
/// the spawned paste task ends, no matter how it exits (early return, error, or
/// panic). Drop-guard discipline (learned/013) so a panic in the paste path can
/// never wedge the flag `true` and permanently drop every future re-paste.
struct RepasteInFlightGuard(Arc<AtomicBool>);

impl Drop for RepasteInFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Atomically claim the re-paste in-flight slot (plan 039 task 48b). Returns
/// `true` if the slot was free and is now claimed (caller must release it via
/// [`RepasteInFlightGuard`]); `false` if a re-paste is already running, in which
/// case the press is dropped. Split out so the double-press drop is unit-testable
/// without spawning a task.
fn try_claim_repaste(in_flight: &AtomicBool) -> bool {
    in_flight
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Blindly re-paste the newest dictation on its own async task.
///
/// No focus probe, no auto-Enter, no pre-padding — the deliberate press pastes
/// wherever the cursor is (or is a silent no-op with nothing focused, since the
/// injector's Cmd+V simply catches on nothing). `injector.paste` adds the
/// trailing space itself; empty history is a silent no-op.
///
/// Plan 039 task 48(b): `in_flight` gates re-entrancy — a press that arrives
/// while a re-paste is already running is dropped (not queued), so a double-tap
/// yields exactly one paste. The flag is released by [`RepasteInFlightGuard`]
/// when this task ends. The injector itself serializes pastes against dictation
/// delivery (see `SerializedInjector`).
fn spawn_repaste(
    injector: Arc<dyn PlatformInjector>,
    history: Option<Arc<HistoryStore>>,
    in_flight: Arc<AtomicBool>,
) {
    // Drop the press if a re-paste is already in flight.
    if !try_claim_repaste(&in_flight) {
        log::debug!(target: "hotkey", "re-paste already in flight — dropping press");
        return;
    }
    tauri::async_runtime::spawn(async move {
        // Release the in-flight flag when this task ends, however it ends.
        let _guard = RepasteInFlightGuard(in_flight);
        let Some(history) = history else {
            log::debug!(target: "hotkey", "re-paste: no history store available — no-op");
            return;
        };
        match history.latest() {
            Ok(Some(record)) => match injector.paste(&record.cleaned_text).await {
                Ok(()) => log::info!(
                    target: "hotkey",
                    "re-paste injected newest dictation ({} chars)",
                    record.cleaned_text.chars().count()
                ),
                Err(err) => {
                    log::warn!(target: "hotkey", "re-paste injection failed: {err}")
                }
            },
            Ok(None) => log::info!(target: "hotkey", "re-paste: history empty — no-op"),
            Err(err) => log::warn!(target: "hotkey", "re-paste: history query failed: {err}"),
        }
    });
}

/// Feature 037 — read the persisted dictation trigger from the settings store,
/// falling back to the shipped default on a missing key or a parse failure.
/// **Never panics**: a corrupt or legacy value must not block boot — the tap
/// simply runs on the default `Ctrl+Option` binding until the user rebinds.
fn load_dictation_binding(app: &AppHandle) -> hotkey_binding::DictationBinding {
    use hotkey_binding::DictationBinding;

    let Ok(store) = app.store(settings::SETTINGS_FILE) else {
        return DictationBinding::default_dictation();
    };
    let binding = match store.get(settings::KEY_HOTKEY_DICTATION_BINDING) {
        Some(value) => serde_json::from_value(value).unwrap_or_else(|err| {
            log::warn!(
                target: "hotkey",
                "stored dictation_binding is invalid, using default: {err}"
            );
            DictationBinding::default_dictation()
        }),
        None => DictationBinding::default_dictation(),
    };
    // Re-validate against current rules: a binding that was legal when it was
    // stored may have been outlawed since (e.g. a Shift-only chord). Fall back
    // to the default rather than driving the tap with a now-invalid trigger.
    if let Err(err) = binding.validate() {
        log::warn!(
            target: "hotkey",
            "stored dictation_binding fails current validation ({err}), using default"
        );
        let default = DictationBinding::default_dictation();
        // Self-heal the store so the Settings recorder (which reads the stored
        // value) and the active trigger agree — otherwise the UI would show the
        // now-invalid chord while the tap runs on the default. Best-effort: a
        // failed write just leaves the fallback active for this session.
        if let Ok(value) = serde_json::to_value(&default) {
            store.set(settings::KEY_HOTKEY_DICTATION_BINDING, value);
            let _ = store.save();
        }
        return default;
    }
    binding
}

/// Feature 037 — read the persisted re-paste binding from the settings store.
/// An **absent** key falls back to the shipped **enabled** `Ctrl+Cmd+V` default
/// (mirrors [`load_dictation_binding`] — the feature works out of the box); only
/// an explicit stored `null`, the user's clear-to-disable, yields `None`. **Never
/// panics**: a corrupt value degrades to "disabled" rather than blocking boot;
/// the Shortcuts screen still shows and can rebind it.
fn load_repaste_binding(app: &AppHandle) -> Option<hotkey_binding::RepasteBinding> {
    let store = app.store(settings::SETTINGS_FILE).ok()?;
    let stored = store.get(settings::KEY_HOTKEY_REPASTE_BINDING);
    let resolved = resolve_repaste_binding(stored.clone());

    // Plan 039 task 49(a): self-heal like `load_dictation_binding`. A stored
    // value that fails to deserialize OR fails current validation resolves to
    // DISABLED; persist that so the Shortcuts recorder (which reads the store)
    // and the runtime agree — otherwise the UI would show a chord the runtime
    // never registered. An explicit `null` or an absent key is already coherent;
    // only a genuinely invalid stored value (present + non-null) needs a write.
    let stored_is_invalid = matches!(&stored, Some(v) if !v.is_null()) && resolved.is_none();
    if stored_is_invalid {
        store.set(
            settings::KEY_HOTKEY_REPASTE_BINDING,
            serde_json::Value::Null,
        );
        let _ = store.save();
        log::warn!(
            target: "hotkey",
            "stored repaste_binding was invalid — healed to disabled"
        );
    }
    resolved
}

/// Pure resolution of a stored re-paste value into a binding, split out from
/// [`load_repaste_binding`] so the absent-key-vs-explicit-`null` distinction is
/// unit-testable without standing up a Tauri store. Shared with
/// `commands::effective_repaste_binding` so boot and the collision-check command
/// apply one semantic (plan 039 task 49a).
///
/// Semantics:
/// - **absent** (never set) → enabled `Ctrl+Cmd+V` default (works out of the box)
/// - **`null`** (user cleared) → `None` (disabled)
/// - **malformed JSON** → `None` (disabled, warn)
/// - **valid JSON but fails `validate()`** → `None` (disabled, warn) — degrades to
///   disabled rather than to a default, so an invalid stored binding is never
///   silently registered.
pub(crate) fn resolve_repaste_binding(
    stored: Option<serde_json::Value>,
) -> Option<hotkey_binding::RepasteBinding> {
    // Absent key: the user has never touched this setting → enabled default,
    // NOT disabled. Conflating "absent" with "null" would silently ship the
    // re-paste hotkey off on every fresh install.
    let Some(value) = stored else {
        return Some(hotkey_binding::RepasteBinding::default_repaste());
    };
    // A stored JSON `null` deserialises to `None` (explicit clear-to-disable); a
    // malformed object logs and also disables rather than surfacing a boot error.
    let binding: Option<hotkey_binding::RepasteBinding> = serde_json::from_value(value)
        .unwrap_or_else(|err| {
            log::warn!(
                target: "hotkey",
                "stored repaste_binding is invalid, treating as disabled: {err}"
            );
            None
        });
    // A structurally invalid binding (valid JSON, fails the rules — e.g. a bare
    // key or a reserved paste combo) also disables rather than registering a
    // footgun accelerator.
    let binding = binding?;
    if let Err(err) = binding.validate() {
        log::warn!(
            target: "hotkey",
            "stored repaste_binding fails current validation ({err}), treating as disabled"
        );
        return None;
    }
    Some(binding)
}

/// Read the persisted onboarding-complete flag, defaulting to `false` (wizard
/// wins) on any store-open / read failure — mirrors the first-run gate in
/// `setup`. Used to decide whether a boot-time hotkey-arm failure surfaces to
/// the user (plan 039 task 31).
fn is_onboarding_complete(app: &AppHandle) -> bool {
    match app.store(settings::SETTINGS_FILE) {
        Ok(store) => store
            .get(settings::KEY_DID_COMPLETE_ONBOARDING)
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        Err(err) => {
            log::warn!(
                target: "onboarding",
                "onboarding-complete read failed while deciding arm-failure surface: {err}"
            );
            false
        }
    }
}

/// Decide whether a boot-time hotkey-arm failure is surfaced to the user
/// (plan 039 task 31). The wizard's first arm attempt is *expected* to fail —
/// it's precisely what triggers the macOS Input Monitoring prompt — so it stays
/// silent while onboarding is in progress. Once onboarding is complete, an arm
/// failure means a returning user's Input Monitoring was revoked (or the tap
/// couldn't install), which warrants the user-visible notification the
/// presenter already classifies (`InputMonitoringDenied` → Notification +
/// requires-action).
fn present_arm_failure(
    onboarding_complete: bool,
    err: &MuniError,
    present: &error_presenter::PresentError,
) {
    if onboarding_complete {
        present(err);
    }
}

pub fn arm_hotkey_pipeline(app: &AppHandle) {
    let Some(state) = app.try_state::<PendingHotkeyArm>() else {
        log::debug!(
            target: "hotkey",
            "arm_hotkey_pipeline called with no pending arm — already armed?"
        );
        return;
    };
    let Some(inner) = state.take() else {
        log::debug!(target: "hotkey", "arm_hotkey_pipeline called twice — ignoring");
        return;
    };

    let hotkey_config = hotkey::HotkeyConfig::from_env();
    log::info!(
        target: "hotkey",
        "boot: ptt_debounce_ms={} tap_threshold_ms={} max_duration_s={} toggle_enabled={}",
        hotkey_config.debounce_ms,
        hotkey_config.tap_threshold_ms,
        hotkey_config.max_toggle_duration_s,
        hotkey_config.toggle_enabled,
    );

    // Feature 037 — the live dictation-trigger state is seeded + managed in
    // `setup` (independent of when the pipeline arms, so the Shortcuts settings
    // commands can reach it during onboarding). Reuse it here; fall back to a
    // freshly-seeded default only if setup somehow didn't manage it.
    let trigger = match app.try_state::<Arc<hotkey::HotkeyTriggerState>>() {
        Some(existing) => existing.inner().clone(),
        None => {
            let binding = load_dictation_binding(app);
            log::info!(target: "hotkey", "boot: dictation_binding={}", binding.label());
            let trigger = Arc::new(hotkey::HotkeyTriggerState::from_binding(&binding));
            app.manage(trigger.clone());
            trigger
        }
    };

    match hotkey::HotkeyManager::start(app.clone(), hotkey_config, trigger) {
        Ok(manager) => {
            let press_rx = manager.subscribe_press();
            let release_rx = manager.subscribe_release();
            let silence_threshold = manager.silence_threshold();
            let silence_signaler = manager.silence_timeout_signaler();
            inner.session.spawn_driver(
                inner.audio,
                inner.debug_dir,
                press_rx,
                release_rx,
                silence_threshold,
                silence_signaler,
            );

            // Feature 038 — the keyed-dictation controller lives here, not in
            // `setup`: its edge feed is born on the manager (arm-time), so a
            // controller built next to `RepasteController` would have no way to
            // reach the state machine. A keyed binding registers a consuming
            // global shortcut; a modifier-only binding registers nothing (the
            // flags-tap drives it). Plan 039 task 51(a): a boot register failure
            // now *heals* the store to a registrable fallback via `apply`'s
            // `fail_registration`, which re-`apply`s the healed binding through
            // `try_state::<DictationShortcutController>` — so the controller MUST
            // be managed BEFORE its first `apply`, or that heal-side re-apply
            // silently no-ops and leaves `current_accel` holding the dead accel.
            let edge = manager.dictation_edge_sender();
            let dictation_controller =
                Arc::new(DictationShortcutController::new(app.clone(), edge));
            app.manage(Arc::clone(&dictation_controller));
            dictation_controller.apply(&load_dictation_binding(app));

            app.manage(manager);
            log::info!(target: "hotkey", "hotkey pipeline armed");
        }
        Err(err) => {
            // The first failed install is precisely what triggers
            // the macOS Keystroke Receiving prompt — CGEventTap
            // creation without Input Monitoring access is the only
            // reliable way to surface that prompt and register the
            // app in TCC. Put the bundle back so the next call (the
            // wizard polls until the user grants, then advances to
            // Finish which arms again) can retry the install.
            log::warn!(
                target: "hotkey",
                "hotkey listener install failed; bundle preserved for retry: {} (severity={:?})",
                err.user_message(),
                err.severity()
            );
            state.put_back(inner);

            // Plan 039 task 31 — for the post-onboarding boot path (a returning
            // user whose Input Monitoring was revoked, or a genuine tap-install
            // failure), route the error through the presenter so it reaches the
            // user as a notification. The wizard's expected first-failure — the
            // one that surfaces the Input Monitoring prompt — stays silent,
            // gated on the onboarding-complete flag.
            let present = app_handle_presenter(app.clone());
            present_arm_failure(is_onboarding_complete(app), &err, &present);
        }
    }
}

/// Run the bits of app initialisation that were deferred during
/// onboarding so the wizard could introduce each macOS permission
/// prompt in context (instead of having them fire cold at app
/// launch).
///
/// Called from `commands::complete_onboarding` after the wizard's
/// Finish step. Idempotent enough — the first launch after wizard
/// completion will also run these again from the normal
/// `onboarding_complete == true` path during `setup`. The double-run
/// is intentional: this call brings the running process up to a
/// post-onboarding state without forcing a restart.
///
/// Currently bundles only:
///   * `tray::build` — would trigger the Microphone prompt via cpal
///     device enumeration in `build_mic_submenu`.
///
/// **Launch-at-Login is not reconciled here.** The onboarding wizard's
/// Launch-at-Login step applies the user's choice directly via
/// `set_launch_at_login` (SMAppService — no prompt, no second app to
/// authorize), so the OS state is already consistent by Finish-time; a
/// reconcile pass would be redundant. Returning users reconcile on every
/// launch via `setup`'s direct call.
///
/// Hotkey arming is NOT bundled here — it has its own deferral
/// rationale (Input Monitoring) and its own retry mechanism
/// (`arm_hotkey_pipeline`'s preserved-bundle pattern), so it stays a
/// separate call site.
pub fn run_post_onboarding_init(app: &AppHandle) {
    // Tray. Safe to call even if a tray already exists for this
    // bundle id — `TrayIconBuilder::with_id(TRAY_ID).build()` would
    // log a duplicate-id warning but onboarding completion is a
    // one-shot, so this path runs at most once per process.
    if let Err(err) = tray::build(app) {
        log::error!(target: "tray", "post-onboarding tray init failed: {err}");
    }
    // Plan 039 task 33 — arm the silent updater UNCONDITIONALLY, independent of
    // the tray build. A tray-build failure must not silently disable
    // auto-update: a bad build could then never pull its own fix. No-ops on
    // dev/staging (placeholder feed).
    updater::spawn_background_check(app);
}

/// Install a panic hook that records the panic to the rotating log *before*
/// the process aborts (`panic = "abort"` in release). A bundled `.app` has no
/// stderr, so without this a crash-on-launch vanishes without a trace; with it,
/// the panic site + message land in `Muni.log`, and the cross-launch crash-loop
/// guard ([`boot_health`]) recovers on the next launch. Chains the previous
/// hook so any default reporting still runs.
fn install_panic_logging() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        log::error!(
            target: "panic",
            "PANIC at {location}: {message} — process will abort; crash-loop guard recovers on next launch"
        );
        // Feature 033 — flush Sentry before the abort. When Sentry is active,
        // its default panic integration (installed at `init`) has ALREADY
        // captured this panic as an event earlier in the hook chain (its hook
        // wraps ours as `next`). But `panic = "abort"` means the
        // `ClientInitGuard`'s Drop-based flush never runs, so without an
        // explicit flush the captured event dies in the in-memory transport
        // queue. `flush` blocks up to 2s to drain it; it's a no-op (returns
        // immediately) when Sentry was never initialised. `sentry-rust-minidump`
        // is the primary abort-safe path for native crashes; this covers
        // pure-Rust panics belt-and-suspenders.
        if let Some(client) = sentry::Hub::current().client() {
            client.flush(Some(std::time::Duration::from_secs(2)));
        }
        previous(info);
    }));
}

/// Plan 039 task 35 — the effective unclean-launch count for THIS launch given
/// the value currently on disk. `setup()`'s `boot_health::record_launch` bumps
/// the on-disk counter by 1 for this launch *before* the safe-mode gate reads
/// it; this pre-runtime Sentry gate reads the counter before that bump, so it
/// adds 1 to evaluate the crash-loop predicate on the same value the safe-mode
/// gate will see. Saturating so a pathological tally can't wrap.
///
/// On dev/staging the updater isn't configured, so `record_launch` never runs
/// (no bump) — but Sentry has no baked DSN there either, so the resulting
/// `is_crash_loop` verdict is inert (`init_sentry` returns `None` regardless).
fn effective_launch_count(on_disk: u32) -> u32 {
    on_disk.saturating_add(1)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Make crashes legible (and recoverable): log the panic site before the
    // `panic = "abort"` profile tears the process down. Installed first so it
    // covers the whole boot path.
    install_panic_logging();

    // Feature 033 — initialise Sentry crash capture BEFORE the Tauri runtime.
    //
    // Why here, not in `setup()`: Sentry registers global state before threads
    // spawn, and `sentry-rust-minidump` re-launches THIS executable as a
    // monitor process — every line that runs before its `init` runs in BOTH
    // processes. Doing it at the very top keeps that doubled-up code minimal
    // (just the install-id + toggle file reads below) instead of the whole
    // boot path.
    //
    // There's no `AppHandle` yet, so we resolve the app-data dir from `$HOME` +
    // the compile-time bundle id and read the crash toggle / boot-health
    // counter straight from their store files (see `telemetry`). The install-id
    // and boot-health reads fail OPEN, but the crash-consent toggle fails CLOSED
    // on a present-but-unreadable/corrupt store (see
    // `telemetry::read_consent_toggle` for that contract). The returned guard
    // must live for the whole program — it's
    // dropped when `run()` returns at app exit, flushing any queued events.
    // It's `None` (and crash capture is fully off, no monitor spawned) on
    // dev/staging (no baked DSN), when the toggle is off, or in safe mode.
    let _sentry_guard = {
        let data_dir = telemetry::app_local_data_dir();
        let install_id = match &data_dir {
            Some(dir) => telemetry::install_id::load_or_create(dir),
            None => uuid::Uuid::new_v4().to_string(),
        };
        let crash_enabled = data_dir
            .as_deref()
            .map(telemetry::read_crash_toggle)
            .unwrap_or(true);
        let unclean = data_dir
            .as_deref()
            .map(telemetry::read_boot_health_count)
            .unwrap_or(0);
        // Plan 039 task 35 — gate Sentry on the SAME counter value the safe-mode
        // gate in `setup()` sees. That gate bumps the on-disk counter via
        // `record_launch` before reading it, so we anticipate the +1 here.
        // Without this, the launch that trips safe mode would still initialise
        // Sentry + the minidump monitor (the exact incoherence this closes).
        telemetry::init_sentry(
            &install_id,
            crash_enabled,
            boot_health::is_crash_loop(effective_launch_count(unclean)),
        )
    };

    // DEV-ONLY native-crash trigger (Feature 033 verification). This is the
    // *only* faithful test of native crash delivery: the in-process
    // `fire_test_event` exercises the SDK message/error path, NOT the
    // `sentry-rust-minidump` monitor path that the launch consent gate guards
    // (the gap that let the v0.1.8 consent-gate regression ship). A real abort
    // HERE — after the monitor has forked and `CRASH_REPORTING_LIVE` is armed —
    // is what `panic = "abort"` does in the field, so the separate monitor (not
    // a Drop-based flush) must capture and deliver it. Gated to debug builds AND
    // an explicit env var, so it can never fire in a release binary or by
    // accident: run with `MUNI_SENTRY_DSN="$MUNI_SENTRY_DSN_DEV" MUNI_FORCE_NATIVE_CRASH=1`.
    #[cfg(debug_assertions)]
    if std::env::var_os("MUNI_FORCE_NATIVE_CRASH").is_some() {
        eprintln!(
            "[crash-test] MUNI_FORCE_NATIVE_CRASH set; sentry_guard_active={}; dsn_resolved={}; aborting in 2s",
            _sentry_guard.is_some(),
            telemetry::resolve_sentry_dsn().is_some(),
        );
        // Let the minidump monitor fully settle before we abort (field crashes
        // happen long after startup; an immediate abort could race monitor init).
        std::thread::sleep(std::time::Duration::from_secs(2));
        panic!(
            "muni forced native crash (MUNI_FORCE_NATIVE_CRASH) — telemetry minidump verification"
        );
    }

    // Custom format with millisecond-precision timestamps. The plugin
    // default rounds to seconds, which makes sub-second QA analysis
    // (§10 latency-feel) impossible — every sample collapses to either
    // 0 s or 1 s. Falls back to UTC if the local-offset lookup fails
    // (sandboxed contexts or worker threads where libc tzdata is
    // unreachable).
    // Diagnostic gate: `MUNI_TRACE_GLADIA=1` enables `trace!` for the
    // `"gladia"` log target so the raw inbound WS frames are visible.
    // Used when reverse-engineering Gladia's transcript-frame schema
    // (see `gladia.rs::parse_message` — the `audio_duration` field
    // name has been confirmed missing from final frames, and the trace
    // is how we'd find what schema they actually emit). Off by default
    // because trace lines per press add ~5–10 verbose JSON dumps to the
    // log file.
    let trace_gladia = std::env::var("MUNI_TRACE_GLADIA")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    let mut log_builder = tauri_plugin_log::Builder::new()
        .level(log::LevelFilter::Info)
        .level_for("muni", log::LevelFilter::Debug)
        // Feature 025 — the audio-LID gate's per-window skip events
        // (`audio-LID: window skipped (vad_silent, ...)`) and routing
        // diagnostics fire at DEBUG against the `lid` target. The
        // global `.level(Info)` floor would otherwise drop them.
        // Cost is bounded: skip lines only fire when the gate is on
        // (opt-in via `MUNI_VAD_AUDIO_LID_GATE`), and the routing
        // diagnostics already fire elsewhere at INFO.
        .level_for("lid", log::LevelFilter::Debug)
        // Backlog 0050 — the plugin's default `max_file_size` is 40 KB,
        // which rotates `Muni.log` mid-batch during a multi-press dogfood
        // session (observed at ~6 KB and ~23 KB during feat/026 dogfood
        // 2026-05-21) and loses earlier `[lid]` traces. 1 MB gives ample
        // headroom for any realistic dogfood batch with negligible disk
        // cost for a single-user app. Default rotation is `KeepOne` —
        // made explicit here so the intent is grep-able alongside the
        // size bump and a future tauri-plugin-log default change can't
        // silently regress us.
        .max_file_size(1024 * 1024)
        .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepOne);
    if trace_gladia {
        log_builder = log_builder.level_for("gladia", log::LevelFilter::Trace);
    }
    let log_plugin = log_builder
        .format(|out, message, record| {
            const FMT: &[time::format_description::FormatItem<'_>] = time::macros::format_description!(
                "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:3]"
            );
            let ts = time::OffsetDateTime::now_local()
                .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
                .format(&FMT)
                .unwrap_or_else(|_| String::from("?"));
            out.finish(format_args!(
                "[{}][{}][{}] {}",
                ts,
                record.target(),
                record.level(),
                message
            ));
        })
        .build();

    let builder = tauri::Builder::default();

    // Debug-only MCP bridge plugin — lets the Tauri MCP server attach
    // to this webview for DOM/CSS inspection during development. Gated
    // on `debug_assertions` so it never compiles into release builds.
    #[cfg(debug_assertions)]
    let builder = builder.plugin(tauri_plugin_mcp_bridge::init());

    builder
        .plugin(log_plugin)
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        // Launch-at-Login is handled by `crate::launch_item` (SMAppService),
        // not a Tauri plugin. SMAppService registers the main app itself, so
        // it shows under System Settings → "Open at Login" with no Automation
        // prompt — see `launch_item.rs` for why this replaced the old
        // `tauri-plugin-autostart` AppleScript path.
        // Phase 10 — Tauri auto-updater (silent: checks on launch,
        // downloads in background, prompts on next launch). Endpoint
        // and pubkey live in `tauri.conf.json plugins.updater`. Until
        // those are populated with real values (see README), runtime
        // `check()` calls return a typed error and the app continues
        // running normally.
        .plugin(tauri_plugin_updater::Builder::new().build())
        // Registered for capability completeness (R7). The actual modifier-only
        // chord uses CGEventTap directly via `hotkey::HotkeyManager`; this
        // plugin only services any future opt-in keyed shortcut features.
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            commands::ping,
            commands::set_tray_state,
            commands::get_session_state,
            commands::settings_get,
            commands::settings_set,
            commands::secrets_set_deepgram,
            commands::secrets_set_groq,
            commands::secrets_set_gemini,
            commands::secrets_delete_deepgram,
            commands::secrets_delete_groq,
            commands::secrets_delete_gemini,
            commands::secrets_is_deepgram_set,
            commands::secrets_is_groq_set,
            commands::secrets_is_gemini_set,
            commands::secrets_set_gladia,
            commands::secrets_delete_gladia,
            commands::secrets_is_gladia_set,
            commands::validate_api_key,
            commands::cleanup_prompt_get,
            commands::cleanup_prompt_save,
            commands::cleanup_prompt_revert,
            commands::my_words_get,
            commands::my_words_save,
            commands::about_me_get,
            commands::about_me_save,
            commands::user_prompt_get,
            commands::user_prompt_save,
            commands::vocabulary_get,
            commands::vocabulary_save,
            commands::microphone_status,
            commands::request_microphone_access,
            commands::is_accessibility_trusted,
            commands::prompt_accessibility,
            commands::open_system_settings,
            commands::complete_onboarding,
            commands::input_monitoring_status,
            commands::request_input_monitoring,
            commands::set_launch_at_login,
            commands::is_launch_at_login_enabled,
            commands::get_stored_launch_at_login_pref,
            commands::set_dictation_hotkey,
            commands::set_repaste_hotkey,
            commands::hotkey_set_recording,
            commands::is_mic_likely_silenced,
            commands::restart_app,
            commands::history_list,
            commands::history_count,
            commands::history_delete,
            commands::history_wipe,
            commands::app_display_name,
            commands::check_for_updates,
            commands::apply_pending_update,
            commands::focus_main_window,
            commands::usage_summary_list_all_months,
            commands::usage_summary_get_per_model_current_month,
            commands::usage_prices_list_current_month,
            commands::enumerate_input_devices,
            // Feature 033 — dev-only test affordance. `#[cfg(debug_assertions)]`
            // keeps it out of release builds entirely (not compiled, not
            // registered, not invocable) so users can't trigger it.
            #[cfg(debug_assertions)]
            commands::telemetry_fire_test_event,
            #[cfg(debug_assertions)]
            commands::debug_fire_loud_notification,
            // Feature 033 (Phase 3) — frontend activation-funnel events route
            // through the same durable PostHog queue (no posthog-js).
            commands::telemetry_track,
        ])
        .setup(|app| {
            log::info!("muni: setup starting");

            // Debug-only MCP bridge ACL grant (plan 039 task 45). The
            // `mcp-bridge:default` permission is no longer in the static
            // `capabilities/default.json`, so release builds never expose the
            // bridge to any window. In dev we add it at runtime from a file
            // kept OUTSIDE the auto-discovered `capabilities/` directory, so
            // it only lands under `debug_assertions` — paired with the
            // `#[cfg(debug_assertions)]` plugin registration above. Register
            // before the safe-mode early-return so the bridge is usable even
            // on a headless boot during development.
            #[cfg(debug_assertions)]
            if let Err(e) =
                app.add_capability(include_str!("../capabilities_debug/mcp-bridge.json"))
            {
                log::warn!(target: "setup", "failed to add debug mcp-bridge capability: {e}");
            }

            // Crash-loop safety net (prod only). With `panic = "abort"` a bad
            // update can't be caught in-process, so the guard works across
            // launches: bump an on-disk counter BEFORE any risky init, and a
            // delayed task (at the end of setup) clears it once we've run
            // cleanly for a bit. After N consecutive unclean launches, boot
            // MINIMAL — arm the updater and skip audio/hotkey/tray/ASR — so a
            // fix can download and apply on the next launch instead of
            // crash-looping forever. Gated on `is_configured` so dev/staging
            // (placeholder feed, no fix to pull) always boot normally.
            let updater_configured = updater::is_configured(app.handle());
            if updater_configured {
                let unclean = boot_health::record_launch(app.handle());
                if boot_health::is_crash_loop(unclean) {
                    log::error!(
                        target: "boot_health",
                        "SAFE MODE: {unclean} consecutive unclean launches — skipping audio/hotkey/tray/ASR and arming the updater only so a fix can ship"
                    );
                    updater::spawn_background_check(app.handle());
                    // Self-limiting: still schedule the reset, so a false
                    // positive costs at most one mostly-headless launch.
                    boot_health::schedule_healthy_reset(app.handle());
                    return Ok(());
                }
                log::info!(
                    target: "boot_health",
                    "launch attempt #{unclean} (counter resets after a clean run)"
                );
            }

            // Phase 9 — first-run gate. The Tauri Store may not exist yet
            // on a brand-new install; treat any read failure as
            // "onboarding not complete" so the wizard always wins on
            // first launch (and the user can still open Main via the
            // tray once onboarding is done in a later session).
            let onboarding_complete = match app.store(settings::SETTINGS_FILE) {
                Ok(store) => store
                    .get(settings::KEY_DID_COMPLETE_ONBOARDING)
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                Err(err) => {
                    log::warn!(
                        target: "onboarding",
                        "settings store unreadable on launch: {err}; defaulting to wizard"
                    );
                    false
                }
            };
            log::info!(
                target: "onboarding",
                "first-run gate: onboarding_complete={onboarding_complete}"
            );

            if let Some(window) = app.get_webview_window("main") {
                if onboarding_complete {
                    let _ = window.show();
                } else {
                    // Stay hidden until the wizard's Finish step calls
                    // `complete_onboarding`. Without this the main
                    // window flashes briefly behind the wizard.
                    let _ = window.hide();
                }
                // The red close button on macOS would otherwise destroy
                // the WebviewWindow — after which the tray's Settings /
                // History items have nothing to focus and emit a warning
                // on every click. Intercepting `CloseRequested` and
                // hiding instead matches the menu-bar-app convention
                // users expect: ⌘Q (or tray "Quit Muni") fully exits;
                // closing the window leaves the app running in the tray.
                let hide_handle = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        if let Err(err) = hide_handle.hide() {
                            log::warn!(target: "main_window", "hide on close failed: {err}");
                        }
                    }
                });
            }

            if !onboarding_complete {
                if let Some(onboarding) = app.get_webview_window("onboarding") {
                    if let Err(err) = onboarding.show() {
                        log::warn!(target: "onboarding", "show wizard failed: {err}");
                    }
                    if let Err(err) = onboarding.set_focus() {
                        log::warn!(target: "onboarding", "focus wizard failed: {err}");
                    }
                    // Pull the app to the foreground so the onboarding
                    // window doesn't open behind whatever the user was
                    // last looking at. LSUIElement=true makes Muni a
                    // menu-bar accessory app, which means show() +
                    // set_focus() only focus the window inside Muni's
                    // own window list — they don't activate the app.
                    // Without activation, the wizard appears unfocused
                    // and forces the user to Cmd+Tab to find it. Gated
                    // to release builds: in the dev shell, onboarding
                    // is rare (only after a manual data wipe) and the
                    // implicit focus-steal on a rebuild would be more
                    // annoying than helpful.
                    #[cfg(all(target_os = "macos", not(debug_assertions)))]
                    activate_app_macos();
                } else {
                    log::error!(
                        target: "onboarding",
                        "onboarding window missing — check tauri.conf.json"
                    );
                }
            }

            // MicSilencedFlag is fresh per process: silence detection
            // can only be cleared by an actual process restart anyway,
            // so there's nothing to persist or restore at boot.
            let mic_silenced = MicSilencedFlag::default();
            app.manage(mic_silenced.clone());

            // Spike — restore persisted ASR routing flag. Default
            // false (Groq Whisper); a returning user keeps whatever
            // they last toggled in the tray menu. Read errors fall
            // back to the default rather than failing boot — a
            // missing settings file on a fresh install must not
            // block the app from starting.
            let initial_fast_mode = match app.store(settings::SETTINGS_FILE) {
                Ok(store) => store
                    .get(settings::KEY_ENGLISH_FAST_MODE)
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                Err(_) => false,
            };
            let english_fast_mode = EnglishFastModeFlag::new(initial_fast_mode);
            app.manage(english_fast_mode.clone());
            log::info!(
                target: "asr",
                "boot: english_fast_mode={initial_fast_mode} ({})",
                if initial_fast_mode { "deepgram (forced)" } else { "auto-detect" }
            );

            // Feature 037 — live dictation-trigger state. Seeded from the store
            // so a rebind survives restart; the leaked flags-tap reads its
            // atomics per-event so `set_dictation_hotkey` applies without a tap
            // reinstall (the `EnglishFastModeFlag` live-reconfig precedent).
            // Managed here — independent of when the hotkey pipeline arms — so
            // the Shortcuts settings commands can reach it even during
            // onboarding, before the tap is installed.
            let dictation_binding = load_dictation_binding(app.handle());
            let dictation_trigger =
                Arc::new(hotkey::HotkeyTriggerState::from_binding(&dictation_binding));
            app.manage(dictation_trigger);

            // Plan 039 task 51(c) — the recording-suppression watchdog counter.
            // `hotkey_set_recording` bumps it and (on start) arms a timeout that
            // clears an orphaned suppression if the settings window closes,
            // reloads, or crashes mid-record without the paired resume.
            app.manage(RecordingWatchdog::default());
            log::info!(
                target: "hotkey",
                "boot: dictation_binding={}",
                dictation_binding.label()
            );

            // Backlog 0012 — Bilingual Mode is **force-OFF at boot** as
            // of 2026-05-25. The tray entry is hidden (see `tray.rs`) and
            // any persisted `true` from earlier sessions is overwritten
            // with `false` so on-disk state matches the hidden-UI state.
            // The flag itself is still managed so the session-layer
            // routing code can read it; it just stays `false` for the
            // lifetime of the process. To re-enable the feature, restore
            // the original persisted-load logic here AND restore the tray
            // entry in `tray.rs::build`.
            if let Ok(store) = app.store(settings::SETTINGS_FILE) {
                let was_persisted_on = store
                    .get(settings::KEY_BILINGUAL_MODE)
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if was_persisted_on {
                    log::info!(
                        target: "asr",
                        "boot: bilingual_mode was persisted=true — forcing off and overwriting on-disk value (feature hidden)"
                    );
                    store.set(settings::KEY_BILINGUAL_MODE, json!(false));
                    if let Err(err) = store.save() {
                        log::warn!(
                            target: "asr",
                            "boot: failed to overwrite persisted bilingual_mode → {err}"
                        );
                    }
                }
            }
            let bilingual_mode = BilingualModeFlag::new(false);
            app.manage(bilingual_mode.clone());
            log::info!(
                target: "asr",
                "boot: bilingual_mode=false (hidden — feature dormant)"
            );

            // Tray is **deferred during onboarding**. Building it now
            // would call `build_mic_submenu` → `cpal::Host::input_devices`
            // → trigger the macOS Microphone prompt before the wizard
            // has had a chance to introduce it. The orchestrator's
            // tray-state pushes during onboarding don't matter (no
            // session can run until the hotkey is armed, which is
            // itself deferred). `commands::complete_onboarding` will
            // call `run_post_onboarding_init` to build the tray once
            // the user has walked through the permission steps.
            //
            // Returning users (`onboarding_complete == true`) get the
            // tray immediately — same behaviour as before.
            //
            // Failures here are logged loudly but non-fatal — the app
            // still runs headless from the hotkey if the tray cannot
            // be created.
            if onboarding_complete {
                if let Err(err) = tray::build(app.handle()) {
                    log::error!(target: "tray", "tray init failed: {err}");
                }
                // Plan 039 task 33 — arm the silent updater independent of the
                // tray build so a tray failure can't disable auto-update
                // (prod-only; no-op on dev/staging).
                updater::spawn_background_check(app.handle());
            } else {
                log::info!(
                    target: "tray",
                    "deferring tray build until onboarding completes"
                );
            }

            // CoreAudio device-change watcher: keeps `Microphone ▸`
            // live without requiring the user to restart on
            // hot-plug or default-input changes (AirPods auto-claim
            // the OS default on connect, for example). The watcher
            // emits a Tauri event the tray subscribes to inside
            // `tray::build`.
            audio_devices_watcher::start(app.handle().clone());

            // HUD overlay window is configured here (on the main thread) so
            // the NSWindow flags — status-bar level, all-spaces collection
            // behaviour, click-through — are applied before the first
            // `Listening` transition could try to show it. The window stays
            // hidden (`visible: false` in tauri.conf.json) until the session
            // notifier asks for it.
            let hud_controller = HudController::new();
            app.manage(Arc::clone(&hud_controller));
            match app.get_webview_window(hud::HUD_WINDOW_LABEL) {
                Some(hud_window) => {
                    if let Err(err) = hud::configure_for_overlay(&hud_window) {
                        log::error!(target: "hud", "overlay configuration failed: {err}");
                    }
                    // Pre-position so the very first show() lands on the
                    // right monitor without a one-frame teleport.
                    if let Err(err) = hud::position_on_active_screen(&hud_window) {
                        log::warn!(target: "hud", "initial positioning failed: {err}");
                    }
                }
                None => {
                    log::error!(
                        target: "hud",
                        "HUD window '{}' missing — check tauri.conf.json",
                        hud::HUD_WINDOW_LABEL,
                    );
                }
            }

            // AudioCapture is constructed unconditionally so the audio host
            // and bridge threads are alive before the hotkey can fire. The
            // input stream itself only opens on the first press → permission
            // prompts (and any failures) are deferred until the user actually
            // asks to dictate, mirroring Swift v1's lazy-AVAudioEngine path.
            let audio = Arc::new(audio::AudioCapture::new(app.handle().clone()));
            app.manage(audio.clone());

            let debug_dir = app.path().app_local_data_dir().ok();

            // CleanupPrompt + GroqClient are constructed up-front so the
            // first press doesn't pay an extra resource_dir lookup or HTTP
            // client init. Failures here are non-fatal — the orchestrator
            // surfaces a typed error event when it can't load the prompt or
            // the Groq client.
            let cleanup_prompt = match CleanupPrompt::from_app(app.handle()) {
                Ok(cp) => Some(Arc::new(cp)),
                Err(err) => {
                    log::error!(
                        target: "session",
                        "CleanupPrompt init failed: {} ({:?})",
                        err.user_message(),
                        err.severity()
                    );
                    None
                }
            };
            // Manage the live `Arc<CleanupPrompt>` so the Phase 8 IPC
            // commands (`cleanup_prompt_get/save/revert`) share the
            // orchestrator's cache. When the user saves a new override
            // from Settings, the next press sees the new prompt without
            // any extra plumbing — `save_override` invalidates the
            // shared cache.
            if let Some(ref cp) = cleanup_prompt {
                app.manage(Arc::clone(cp));
            }

            // Feature 013 — My Words substitution layer. Boot-fallback is
            // an empty enabled snapshot so a misconfigured store can never
            // block the app from starting.
            let my_words = MyWords::from_app(app.handle());
            app.manage(Arc::clone(&my_words));

            // Feature 014 — About Me free-form vocabulary context.
            // Boot-fallback is an empty string so a misconfigured store
            // can never block the app from starting; the press path
            // treats empty as a no-op (byte-identical cleanup prompt).
            let about_me = AboutMe::from_app(app.handle());
            app.manage(Arc::clone(&about_me));

            // Feature 015 — Vocabulary soft-bias word list for cleanup.
            // Boot-fallback is an empty enabled snapshot so a
            // misconfigured store can never block the app from
            // starting; the press path treats empty as a no-op
            // (byte-identical cleanup prompt).
            let vocabulary = Vocabulary::from_app(app.handle());
            app.manage(Arc::clone(&vocabulary));

            // User-authored "preferences" block. Empty by default;
            // when non-empty it is appended AFTER the bundled cleanup
            // body with a header that explicitly tells the model to
            // follow these instructions when they conflict with rules
            // above. Boot-fallback is an empty string so a
            // misconfigured store can never block the app from
            // starting.
            let user_prompt = UserPrompt::from_app(app.handle());
            app.manage(Arc::clone(&user_prompt));

            // Plan 041 (task 6) — build the ONE `reqwest::Client` shared
            // by all three Groq clients (cleanup / Whisper / LID) and the
            // pool keepalive so they share a single warm TCP/TLS pool.
            // On the (near-impossible) builder failure, each client falls
            // back to its own standalone constructor — behaviour reverts
            // to pre-plan-041 (three separate pools), never a boot
            // failure.
            let shared_groq_http = match groq::shared_groq_http() {
                Ok(http) => Some(http),
                Err(err) => {
                    log::error!(
                        target: "groq",
                        "shared Groq HTTP client build failed: {} ({:?}) — falling back to per-client pools",
                        err.user_message(),
                        err.severity()
                    );
                    None
                }
            };

            let groq_client = match shared_groq_http
                .as_ref()
                .map(|http| {
                    Ok(GroqClient::with_http_client(
                        http.clone(),
                        groq::resolve_groq_endpoint(),
                        groq::resolve_cleanup_model(),
                    ))
                })
                .unwrap_or_else(GroqClient::new)
            {
                Ok(c) => {
                    // Mirror the `[lid] boot:` line so the cleanup
                    // configuration is visible at startup without
                    // dictating first. Cleanup now routes per-press:
                    // short presses (≤ threshold) use
                    // `MUNI_CLEANUP_MODEL` / `MUNI_CLEANUP_REASONING_EFFORT`;
                    // long presses (> threshold) use the
                    // `MUNI_CLEANUP_LONG_*` overrides; threshold is
                    // `MUNI_CLEANUP_LONG_PRESS_THRESHOLD_S` (default
                    // 15.0 s). See `.claude/learned/005_*.md`.
                    log::info!(
                        target: "groq",
                        "boot: cleanup short={}/{} long={}/{} threshold={}s max_completion_tokens={}",
                        c.cleanup_model(),
                        groq::resolve_cleanup_reasoning_effort(),
                        groq::resolve_cleanup_long_model(),
                        groq::resolve_cleanup_long_reasoning_effort(),
                        groq::resolve_cleanup_long_press_threshold_s(),
                        groq::DEFAULT_MAX_COMPLETION_TOKENS
                    );
                    // Surface the endpoint when overridden so a
                    // local-mock dogfood session is unambiguous in the
                    // log (and so a forgotten override can't quietly
                    // make a prod-looking run actually hit a stale
                    // mock). The default endpoint is omitted from the
                    // log to keep the steady-state output tight.
                    let endpoint = groq::resolve_groq_endpoint();
                    if endpoint != groq::DEFAULT_ENDPOINT {
                        log::warn!(
                            target: "groq",
                            "boot: cleanup endpoint OVERRIDDEN via {}={}",
                            groq::GROQ_ENDPOINT_ENV,
                            endpoint
                        );
                    }
                    Some(Arc::new(c))
                }
                Err(err) => {
                    log::error!(
                        target: "groq",
                        "GroqClient init failed: {} ({:?})",
                        err.user_message(),
                        err.severity()
                    );
                    None
                }
            };
            // Feature 016 — manage `Arc<GroqClient>` so the
            // IPC save handlers (`commands::cleanup_prompt_save`,
            // `about_me_save`, `vocabulary_save`, `secrets_set_groq`)
            // can pull the client out of Tauri state to fire the
            // cleanup warm-up. Must happen BEFORE the `let deps =
            // SessionDeps { groq: groq_client, ... }` move at the
            // bottom of `setup`, which consumes `groq_client`. Cheap
            // clone (Arc).
            if let Some(ref c) = groq_client {
                app.manage(Arc::clone(c));
            }
            // Spike — Whisper client init mirrors GroqClient: failures
            // log loudly but the app still boots. The orchestrator
            // falls back to Deepgram when whisper is None.
            let whisper_client = match shared_groq_http
                .as_ref()
                .map(|http| {
                    Ok(GroqWhisperClient::with_http_client(
                        http.clone(),
                        GroqWhisperClient::resolve_endpoint(),
                    ))
                })
                .unwrap_or_else(GroqWhisperClient::new)
            {
                Ok(c) => Some(Arc::new(c)),
                Err(err) => {
                    log::error!(
                        target: "groq_whisper",
                        "GroqWhisperClient init failed: {} ({:?})",
                        err.user_message(),
                        err.severity()
                    );
                    None
                }
            };

            // Parakeet local-ASR sidecar. Only spawned when
            // `MUNI_ASR_BACKEND=parakeet`; any failure degrades to the cloud
            // Deepgram English path (`parakeet: None`). The spawn blocks boot
            // until the sidecar reports READY (model load ~2 s) so the first
            // press is already warm — acceptable for an opt-in dev backend.
            // Tagalog/Taglish routing (LID → Whisper) is untouched.
            let parakeet_client = if crate::parakeet::is_selected() {
                use crate::parakeet::Engine;
                let engine = crate::parakeet::resolve_engine();
                // ONNX takes a model dir (argv[1]); ANE self-manages its model.
                let (bin, model_dir) = match engine {
                    Engine::Ane => (crate::parakeet::resolve_ane_sidecar_bin(app.handle()), None),
                    Engine::Onnx => (
                        crate::parakeet::resolve_sidecar_bin(app.handle()),
                        Some(crate::parakeet::resolve_model_dir(app.handle())),
                    ),
                };
                match tauri::async_runtime::block_on(crate::parakeet::ParakeetClient::spawn(
                    bin.clone(),
                    model_dir.clone(),
                )) {
                    Ok(c) => {
                        log::info!(
                            target: "asr",
                            "boot: asr_backend=parakeet engine={engine:?} bin={} model_dir={} sidecar_pid={:?}",
                            bin.display(),
                            model_dir.as_ref().map_or("<self-managed>".to_string(), |p| p.display().to_string()),
                            c.pid()
                        );
                        Some(Arc::new(c))
                    }
                    Err(err) => {
                        log::error!(
                            target: "asr",
                            "boot: parakeet selected but sidecar unavailable ({}) — English path falls back to Deepgram",
                            err.user_message()
                        );
                        None
                    }
                }
            } else {
                log::info!(target: "asr", "boot: asr_backend=deepgram");
                None
            };
            // Feature 003 — text-LID classifier. The provider is
            // chosen at boot from `MUNI_LID_PROVIDER` (defaults to
            // `groq`); the model is read from `MUNI_LID_MODEL`
            // (defaults per provider). Both are looked up here once
            // — flipping providers requires a relaunch, which is
            // fine because A/B comparison is the only reason to
            // change them.
            //
            // The trait object on `SessionDeps` keeps every other
            // call site agnostic; init failures fall through to
            // `None`, which makes the LID task default each press
            // to Whisper (see `whisper: None` graceful-degradation
            // pattern).
            let text_lid_client = build_text_lid_classifier(shared_groq_http.as_ref());
            // Backlog 0012 — opt-in secondary classifier for hybrid
            // mode (`MUNI_LID_HYBRID=true`). `None` keeps the LID
            // task on the single-classifier 0011 path.
            //
            // Feature 021 — when audio-LID is the active primary
            // (the post-feature-020 default), `text_lid_secondary` is
            // also gated on `MUNI_LID_AUDIO_HYBRID`. The
            // factory itself stays single-purpose (return Some when
            // gating permits), but the gating logic is intentionally
            // split between the two primaries so flipping either env
            // var doesn't accidentally enable the wrong code path.
            let text_lid_secondary_client = build_secondary_lid_classifier();
            // Feature 020 — local audio-LID classifier. `Some` only
            // when `MUNI_LID_PROVIDER=audio_whisper_tiny`; mutually
            // exclusive with `text_lid_client` at the factory level
            // (the text factory returns `None` for the audio slug).
            // Init failures log loudly and the orchestrator falls
            // back to multilingual ASR.
            let audio_lid_client = build_audio_lid_classifier(app.handle());

            // Feature 021 — when audio-LID owns the primary path,
            // swap the secondary from Gemini (text-LID-primary
            // hybrid) to Groq (audio-LID-primary hybrid). The
            // Gemini variant was rejected for the audio-LID path
            // after 2026-05-18 dogfood — its classify latency tail
            // (~1.7 s median, 4 s p95) was too slow to land before
            // press finalisation. Groq's gpt-oss-120b lands in ~300
            // ms median, well inside the bounded-wait budget.
            // The text-LID-primary rollback path keeps Gemini as
            // its secondary (backlog 0012's design); these are
            // independent factories for independent code paths.
            let text_lid_secondary_client = if audio_lid_client.is_some() {
                build_audio_hybrid_secondary_classifier(shared_groq_http.as_ref())
            } else {
                text_lid_secondary_client
            };

            // The platform-default injector is constructed once at boot so
            // every press uses the same configured delays. The paste delay is
            // the user-configurable `paste.delay_ms` setting; a missing setting
            // (fresh install) or read error falls back to the settings-layer
            // default — the single source of truth for the 50 ms value. A live
            // change takes effect on the next launch (same as english_fast_mode
            // is read here at boot). Restore delay is not user-configurable.
            let default_paste_delay_ms = settings::default_for(settings::KEY_PASTE_DELAY_MS)
                .and_then(|v| v.as_u64())
                .unwrap_or(settings::DEFAULT_PASTE_DELAY_MS);
            let paste_delay_ms = match app.store(settings::SETTINGS_FILE) {
                Ok(store) => store
                    .get(settings::KEY_PASTE_DELAY_MS)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(default_paste_delay_ms),
                Err(_) => default_paste_delay_ms,
            };
            let injector: Arc<dyn PlatformInjector> = default_injector(paste_delay_ms);
            // Feature 037 — manage the injector so the re-paste global-shortcut
            // handler and the hotkey commands can reach the same instance to
            // reinject the last dictation. Cheap Arc clone; the orchestrator
            // still owns its own clone via `SessionDeps.injector` below.
            app.manage(Arc::clone(&injector));

            // Spawn the Deepgram pool with a key provider that
            // re-reads `secrets::get` on every warmer attempt and
            // every inline open. A key saved mid-session through
            // the wizard's API Keys step (or Settings → API Keys)
            // takes effect on the very next press — the pool no
            // longer caches a stale empty key from boot.
            //
            // `secrets::get` resolves env-var first
            // (`MUNI_DEEPGRAM_KEY`) then falls back to the OS
            // keychain. The pool tolerates a missing/invalid key by
            // logging warmer failures and falling through to inline
            // open attempts on each press.
            let deepgram_pool =
                DeepgramPool::spawn(Arc::new(|| secrets::get_cached(secrets::DEEPGRAM_ACCOUNT)));
            // Surface a non-production endpoint so a local-mock / dogfood
            // session is unambiguous in the log (and a forgotten override
            // can't quietly make a prod-looking run hit a dead/mock host).
            let dg_endpoint = crate::deepgram::resolve_deepgram_endpoint();
            if dg_endpoint != crate::deepgram::default_endpoint() {
                log::warn!(
                    target: "deepgram",
                    "boot: streaming endpoint OVERRIDDEN via {}={}",
                    crate::deepgram::DEEPGRAM_ENDPOINT_ENV,
                    dg_endpoint
                );
            }
            // Tauri-manage the pool so the secrets IPC handlers can
            // invalidate it whenever the Deepgram key changes — the
            // warm socket would otherwise authenticate with the
            // previous key and serve a successful press right after
            // Remove saved key. Cheap clone (Arc).
            app.manage(deepgram_pool.clone());

            // The state notifier closes over an `AppHandle` clone so it
            // can fetch the live tray icon every time. Cloning the handle
            // is cheap (Arc internally); tray::set_state itself does the
            // tray_by_id lookup. Constructed here (in `setup`) instead of
            // inside the orchestrator so the orchestrator stays
            // tray-agnostic.
            //
            // The HUD overlay piggybacks on the same notifier — keeping
            // both the tray icon and HUD visibility driven from a single
            // edge means they can never disagree about the current state.
            // Track the orchestrator's current state in app-managed atomic
            // storage so the HUD webview can pull the live value on mount.
            // Without this, a webview that boots AFTER the first
            // `Listening` transition (the common case on a fresh launch —
            // see `SessionStateTracker` rationale) has no way to learn it
            // ever happened and leaves the pill hidden through the press.
            let session_state_tracker = SessionStateTracker::new();
            app.manage(Arc::clone(&session_state_tracker));

            let tray_app_handle = app.handle().clone();
            let hud_app_handle = app.handle().clone();
            let hud_controller_for_notifier = Arc::clone(&hud_controller);
            let tracker_for_notifier = Arc::clone(&session_state_tracker);
            let state_notifier: StateNotifier = Arc::new(move |session_state| {
                tracker_for_notifier.set(session_state);
                tray::set_state(&tray_app_handle, session_to_tray_state(session_state));
                hud::handle_state_change(
                    &hud_controller_for_notifier,
                    &hud_app_handle,
                    session_state,
                );
            });

            // Phase 10 — open the SQLite-backed history store and run any
            // pending purge based on the user's retention preference.
            // Failure to open is logged but non-fatal; the session simply
            // runs with `history: None` and skips per-press persistence
            // until the next launch retries the open.
            let history_store = match app.path().app_local_data_dir() {
                Ok(dir) => {
                    if let Err(err) = std::fs::create_dir_all(&dir) {
                        log::warn!(
                            target: "history",
                            "create app_local_data_dir failed: {err}"
                        );
                    }
                    let path = HistoryStore::default_path(&dir);
                    match HistoryStore::open(&path) {
                        Ok(store) => Some(Arc::new(store)),
                        Err(err) => {
                            log::error!(
                                target: "history",
                                "open failed at {}: {err}",
                                path.display()
                            );
                            None
                        }
                    }
                }
                Err(err) => {
                    log::error!(target: "history", "app_local_data_dir unavailable: {err}");
                    None
                }
            };
            if let Some(ref store) = history_store {
                app.manage(Arc::clone(store));
            }

            // Feature 033 — start the PostHog analytics queue alongside the
            // history store (same `history.sqlite3` file, its own
            // `telemetry_queue` table + connection). Gated on the analytics
            // toggle (read once here) and a resolvable key (prod bakes one;
            // dev/staging are silent). The returned queue is managed so the
            // on-exit flush in the run loop can reach it; the public
            // `telemetry::emit_event` API talks to the same queue via a global.
            // Non-fatal everywhere: any failure leaves analytics off.
            if let Ok(dir) = app.path().app_local_data_dir() {
                let analytics_enabled = read_analytics_toggle(&dir);
                let install_id = telemetry::install_id::load_or_create(&dir);
                let db_path = HistoryStore::default_path(&dir);
                if let Some(queue) =
                    telemetry::init_posthog(&db_path, &install_id, analytics_enabled)
                {
                    app.manage(queue);

                    // Feature 033 (Phase 3, task 22) — retention identity. Emit
                    // ONE low-frequency, person-profiled event per launch so
                    // retention cohorts fall out of the stable install-UUID
                    // distinct_id. This is the only event carrying `$set` person
                    // props. Fire-and-forget; a no-op when analytics is off
                    // (the queue wouldn't exist).
                    telemetry::emit_event(telemetry::events::app_launched());
                }
            }

            // Feature 005 — open the cost-tracking store alongside the
            // history store (same SQLite file, separate `Mutex<Connection>`)
            // and seed `DEFAULT_PRICES` for the current UTC month if
            // `price_history` is empty. Failure to open is non-fatal:
            // the orchestrator runs with `usage_tx: None` and no rows
            // land until the next launch.
            let usage_store = match app.path().app_local_data_dir() {
                Ok(dir) => {
                    let path = HistoryStore::default_path(&dir);
                    match UsageStore::open(&path) {
                        Ok(store) => {
                            let arc = Arc::new(store);
                            seed_default_prices_if_empty(&arc);
                            Some(arc)
                        }
                        Err(err) => {
                            log::error!(
                                target: "usage",
                                "open failed at {}: {}",
                                path.display(),
                                err.user_message()
                            );
                            None
                        }
                    }
                }
                Err(err) => {
                    log::error!(target: "usage", "app_local_data_dir unavailable: {err}");
                    None
                }
            };
            if let Some(ref store) = usage_store {
                app.manage(Arc::clone(store));
            }

            // Plan 039 task 47 — retention purge moves from a launch-only
            // one-shot to launch + a periodic 6h tick, and now also covers
            // `api_calls` (previously unbounded). `tokio::time::interval`
            // fires its first tick immediately, so this single task's
            // first pass through the loop IS the launch purge — no
            // separate one-shot spawn is needed alongside it.
            if history_store.is_some() || usage_store.is_some() {
                drop(spawn_retention_purge_task(
                    app.handle().clone(),
                    history_store.clone(),
                    usage_store.clone(),
                ));
            }

            let usage_tx = usage_store
                .as_ref()
                .map(|store| usage_writer::spawn_writer(Arc::clone(store)));
            // Feature 016 — manage `Sender<UsageRecord>` so the
            // groq_warmup helper can dispatch warmup `api_calls`
            // rows from save-handler IPC commands without rewiring
            // `SessionDeps`. Cheap clone; mirrors the `deepgram_pool`
            // manage pattern above.
            if let Some(ref tx) = usage_tx {
                app.manage(tx.clone());
            }

            // Feature 005 — fire up the prices refresher (launch
            // burst + hourly tick). Failures to construct the client
            // are non-fatal; the bootstrap `DEFAULT_PRICES` already
            // cover the very first dictation.
            if let Some(ref store) = usage_store {
                match prices_client::PricesClient::new() {
                    Ok(client) => {
                        // Refresher runs for the lifetime of the
                        // process — drop the JoinHandle on the floor.
                        // Clippy would otherwise flag the bare
                        // `JoinHandle` (which `impl Future`) as a
                        // forgotten future.
                        drop(prices_refresher::spawn(Arc::clone(store), Arc::new(client)));
                    }
                    Err(err) => {
                        log::warn!(
                            target: "pricing",
                            "PricesClient init failed: {} — keeping DEFAULT_PRICES",
                            err
                        );
                    }
                }
            }

            // Plan 041 (task 7) — shared Groq activity tracker. Fed by
            // the delivery tail (cleanup → prefix touch) and the Whisper
            // transcribe path (call), read by the keepalive skip-gate
            // (task 8) and the periodic cache re-warm (slice 4). Managed
            // so the update sites can pull it from Tauri state the same
            // way they reach `usage_tx`.
            let groq_activity = Arc::new(GroqActivity::new());
            app.manage(Arc::clone(&groq_activity));

            // Plan 041 (task 8) — Groq connection-pool keepalive. Spawned
            // with a CLONE of the shared client so the connection it
            // keeps warm is the one real presses reuse. Gated on
            // `MUNI_GROQ_KEEPALIVE` (opt-out) and on the shared client
            // having built. `JoinHandle` dropped on the floor —
            // process-lifetime, mirrors the prices refresher above.
            if groq_keepalive::is_enabled() {
                if let Some(ref http) = shared_groq_http {
                    drop(groq_keepalive::spawn(
                        http.clone(),
                        Arc::clone(&groq_activity),
                        groq_keepalive::DEFAULT_MODELS_ENDPOINT.to_string(),
                    ));
                } else {
                    log::debug!(
                        target: "groq_keepalive",
                        "keepalive not spawned: shared Groq HTTP client unavailable"
                    );
                }
            } else {
                log::debug!(
                    target: "groq_keepalive",
                    "keepalive disabled via {}=false",
                    groq_keepalive::KEEPALIVE_ENV
                );
            }

            // Phase 10 — surface typed errors to the user via the
            // ErrorPresenter (loud → notification, quiet → emit
            // `error://quiet`). The presenter closes over an `AppHandle`
            // clone so it can dispatch from any thread.
            let present_error = app_handle_presenter(app.handle().clone());

            // Feature 037 — the no-editable-field HUD notice. Fired by the
            // orchestrator when a completed dictation lands with nothing focused
            // to receive it. Reads the LIVE re-paste binding on every fire so a
            // rebind updates the copy without a restart; when the binding is
            // disabled (cleared), the copy drops the hotkey reference entirely.
            let repaste_notice: session::RepasteNotice = {
                let notice_app = app.handle().clone();
                Arc::new(move || {
                    let text = match load_repaste_binding(&notice_app) {
                        Some(binding) => {
                            format!("Press {} to insert your dictation", binding.label())
                        }
                        None => "Nothing focused — your dictation is saved in history".to_string(),
                    };
                    hud::show_notice(&notice_app, &text, crate::error::HudTone::Neutral);
                })
            };

            // Feature 037 — the persistent re-paste global shortcut. Built here
            // so it reaches the same injector + history store the orchestrator
            // uses (re-paste must reuse the snapshot/restore path and read the
            // newest recorded dictation). Managed so `set_repaste_hotkey` can
            // rebind it live; registered once from the stored binding. A failed
            // boot registration (combo already held by the OS) is a WARN — the
            // Shortcuts screen still shows the binding and the app boots.
            let repaste_binding = load_repaste_binding(app.handle());
            match repaste_binding.as_ref() {
                Some(binding) => log::info!(
                    target: "hotkey",
                    "boot: repaste_binding={} ({})",
                    binding.label(),
                    binding.accelerator()
                ),
                None => log::info!(target: "hotkey", "boot: repaste_binding disabled"),
            }
            let repaste_controller = Arc::new(RepasteController::new(
                app.handle().clone(),
                Arc::clone(&injector),
                history_store.clone(),
            ));
            // Plan 039 task 51(a): manage BEFORE the first `apply` so a boot
            // register failure's heal (which re-`apply`s via
            // `try_state::<RepasteController>`) reaches the live controller
            // instead of no-oping and leaving `current_accel` on the dead accel.
            app.manage(Arc::clone(&repaste_controller));
            repaste_controller.apply(repaste_binding.as_ref());

            // Install the notification-click delegate once, on the main thread,
            // so a click on a loud-error notification foregrounds Muni (and
            // banners present even while Muni is active). No-op when unbundled.
            error_presenter::install_notification_delegate(app.handle().clone());

            // History recording is always on; the retention slider controls how
            // long rows are kept, not whether they're written. `history_store` is
            // already `None` only when the store failed to open at boot.
            let history_for_session = history_store.clone();

            // Feature 016 — fire the cleanup cold-start warm-up before
            // `groq_client` and `cleanup_prompt` are moved into
            // `SessionDeps`. Seeds Groq's prompt-prefix cache AND warms
            // the reqwest TLS pool on a hidden fire-and-forget request
            // so the user's first real press isn't the slowest of the
            // session. See `groq_warmup` module docs.
            if groq_warmup::is_enabled() {
                match (groq_client.as_ref(), cleanup_prompt.as_ref()) {
                    (Some(client), Some(prompt)) => {
                        drop(groq_warmup::spawn_cleanup_warmup(
                            Arc::clone(client),
                            Arc::clone(prompt),
                            Arc::clone(&about_me),
                            Arc::clone(&vocabulary),
                            Arc::clone(&user_prompt),
                            usage_tx.clone(),
                            Some(Arc::clone(&groq_activity)),
                            groq_warmup::WarmupTrigger::Boot,
                        ));
                    }
                    _ => log::info!(
                        target: "groq_warmup",
                        "boot warmup skipped: GroqClient or CleanupPrompt unavailable"
                    ),
                }
            } else {
                log::info!(
                    target: "groq_warmup",
                    "boot warmup disabled via MUNI_CLEANUP_WARMUP=false"
                );
            }

            // Plan 041 (slice 4) — periodic prompt-cache re-warm. A 5-min
            // staleness tick that fires a re-warm once the cache has been
            // idle ≥ 90 min, so Groq's 2h cache eviction never taxes the
            // first press after a gap. Gated on `MUNI_CACHE_REWARM`
            // (opt-out, spawn-site check) AND, per-fire,
            // `warmup_from_app`'s own `MUNI_CLEANUP_WARMUP` gate — both
            // flags apply. Reads the SAME `groq_activity` the boot
            // warm-up above and every real press cleanup touch, so a
            // fresh boot (which just stamped `last_prefix_touch`) makes
            // the tick's first staleness check a no-op rather than a
            // double-fire. `JoinHandle` dropped on the floor —
            // process-lifetime, mirrors the keepalive spawn above.
            if groq_warmup::is_rewarm_enabled() {
                drop(groq_warmup::spawn_periodic_rewarm(
                    app.handle().clone(),
                    Arc::clone(&groq_activity),
                ));
            } else {
                log::debug!(
                    target: "groq_warmup",
                    "periodic re-warm disabled via {}=false",
                    groq_warmup::CACHE_REWARM_ENV
                );
            }

            // Feature 023 (backlog 0040) — silent-press VAD gate.
            // `None` keeps feat/022 amplitude-only behavior;
            // `Some(SileroVad)` enables the content-aware gate at
            // both release-path Whisper and audio-LID-hybrid slice
            // sites. Boot-time toggle via `MUNI_VAD_GATE`.
            let vad_detector = build_vad_detector();
            // Feature 024 (backlog 0042) — streaming VAD factory.
            // `None` keeps current ship behavior (byte-identical);
            // `Some(factory)` enables the per-stream streaming detector
            // for Site D (hybrid buffer gate) and/or Site E (release-
            // path Whisper batch buffer trim). Default OFF; kill
            // switches are `MUNI_VAD_STREAM_HYBRID` and
            // `MUNI_VAD_TRIM_RELEASE_BUFFER`.
            let streaming_vad_factory = build_streaming_vad_factory();
            let deps = SessionDeps {
                deepgram_pool,
                groq: groq_client,
                prompt: cleanup_prompt,
                injector,
                emitter: app_handle_emitter(app.handle().clone()),
                state_notifier,
                present_error,
                show_repaste_notice: repaste_notice,
                history: history_for_session,
                mic_silenced,
                whisper: whisper_client,
                parakeet: parakeet_client,
                text_lid: text_lid_client,
                text_lid_secondary: text_lid_secondary_client,
                audio_lid: audio_lid_client,
                english_fast_mode,
                bilingual_mode,
                usage_tx,
                usage_store: usage_store.clone(),
                groq_activity: Some(Arc::clone(&groq_activity)),
                my_words: Arc::clone(&my_words),
                about_me: Arc::clone(&about_me),
                vocabulary: Arc::clone(&vocabulary),
                user_prompt: Arc::clone(&user_prompt),
                vad_detector,
                streaming_vad_factory,
            };
            let session = DictationSession::new(deps);

            // Stash the bits required to arm the hotkey pipeline.
            // For returning users we drain the bundle immediately;
            // first-time users keep it parked until their wizard's
            // Finish step calls `complete_onboarding`, which calls
            // `arm_hotkey_pipeline`.
            //
            // Why defer at all? `hotkey::HotkeyManager::start` triggers
            // the macOS Input Monitoring prompt the first time it
            // runs. Firing that prompt independently of the wizard's
            // Input Monitoring step caused the prompt-collision UX
            // mess where users dismissed it accidentally and were
            // then stuck (TCC won't re-prompt without manual entry
            // removal in System Settings).
            app.manage(PendingHotkeyArm::new(PendingHotkeyArmInner {
                session,
                audio: audio.clone(),
                debug_dir,
            }));

            if onboarding_complete {
                arm_hotkey_pipeline(app.handle());
            } else {
                log::info!(
                    target: "hotkey",
                    "deferring hotkey listener until onboarding completes"
                );
            }

            // Phase 10 — reconcile launch-at-login to match the user's
            // stored preference. **Deferred during onboarding**: the
            // autostart plugin's `Manager::is_enabled()` call probes
            // state via AppleScript, which trips the macOS Automation
            // prompt for "Muni wants to control System Events". Firing
            // that prompt before the wizard has explained what the app
            // does is exactly the cold-prompt UX we are trying to
            // avoid. `commands::complete_onboarding` calls this once
            // the user has walked through the wizard.
            //
            // Returning users (`onboarding_complete == true`) get the
            // reconciliation immediately — preserves the original
            // Swift-v1-migration unwind behaviour for already-onboarded
            // installs.
            if onboarding_complete {
                reconcile_launch_at_login(app.handle());
            } else {
                log::info!(
                    target: "autostart",
                    "deferring launch-at-login reconciliation until onboarding completes"
                );
            }

            // Reached the end of setup cleanly. Schedule the crash-loop
            // counter reset (fires after a short healthy window) so this
            // normal launch clears any prior unclean-launch tally. Prod-only,
            // mirroring the bump at the top of setup.
            if updater_configured {
                boot_health::schedule_healthy_reset(app.handle());
            }

            log::info!("muni: setup complete");
            Ok(())
        })
        // `build` + `run(closure)` (rather than `run(context)`) so we get the
        // `RunEvent` stream — needed for Feature 033's flush-on-exit. The
        // `_sentry_guard` above stays in scope until this returns at app exit.
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|app_handle, event| {
            // Feature 033 — best-effort flush of the PostHog queue when the app
            // is asked to quit. The queue is durable (SQLite), so anything not
            // sent in the tight exit window survives to the next launch; this
            // just shortens the delivery gap for a clean quit. No-op when
            // analytics is off (the queue was never managed).
            if let tauri::RunEvent::ExitRequested { .. } = event {
                if let Some(queue) =
                    app_handle.try_state::<std::sync::Arc<telemetry::posthog::TelemetryQueue>>()
                {
                    let queue = std::sync::Arc::clone(&queue);
                    // Block briefly on the flush so it actually runs before the
                    // process tears down. `flush_on_exit` bounds itself with a
                    // hard overall deadline (EXIT_FLUSH_DEADLINE, plan 039 task
                    // 41) — not just the per-request timeout — so a slow endpoint
                    // with a full queue can never hold up quit; unsent rows are
                    // durable and drain on the next launch.
                    tauri::async_runtime::block_on(queue.flush_on_exit());
                }
            }
        });
}

/// Pull Muni to the foreground via `[NSApplication activate]`.
///
/// `LSUIElement=true` makes Muni a menu-bar accessory app — by default
/// any window it shows opens *behind* the currently-active app, and
/// `WebviewWindow::set_focus()` only focuses the window inside Muni's
/// own window list (not the OS-wide foreground app). Calling this on
/// the main thread after a window is shown is what makes Cmd-Tab pick
/// Muni and what gives keyboard input to the wizard's first field.
///
/// macOS 14 (our `minimumSystemVersion`) introduced the
/// no-args `activate()` as the preferred replacement for the
/// deprecated `activateIgnoringOtherApps:` — we use the modern form.
///
/// Returns silently on non-macOS or when called off the main thread.
#[cfg(all(target_os = "macos", not(debug_assertions)))]
fn activate_app_macos() {
    use objc2_app_kit::NSApplication;
    use objc2_foundation::MainThreadMarker;

    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!(target: "onboarding", "activate skipped: not on main thread");
        return;
    };
    let ns_app = NSApplication::sharedApplication(mtm);
    ns_app.activate();
}

/// Map the orchestrator's coarse state onto a tray-icon state. The session
/// pipeline doesn't know about offline / connectivity concerns, so those
/// stay tray-only and never originate here.
fn session_to_tray_state(state: SessionState) -> TrayState {
    match state {
        SessionState::Idle => TrayState::Idle,
        // Plan 030 — a locked toggle session is functionally a
        // Listening state; the tray icon is fixed (only the tooltip
        // changes), so we collapse both Listening variants onto the
        // same TrayState. The HUD carries the visible "locked" affordance.
        SessionState::Listening | SessionState::ListeningLocked => TrayState::Listening,
        SessionState::Cleaning => TrayState::Cleaning,
        SessionState::Recovering => TrayState::Recovering,
        SessionState::Error => TrayState::Error,
    }
}

/// Read the user's `history.retention_days` preference, falling back to the
/// Swift v1 default (30) when the setting is unset or unreadable. Read fresh
/// on every retention-purge tick (not cached) — keeping the read here
/// (rather than inside `HistoryStore`) avoids dragging the Tauri Store into
/// the storage layer.
fn read_retention_days(app: &AppHandle) -> u32 {
    let store = match app.store(settings::SETTINGS_FILE) {
        Ok(s) => s,
        Err(err) => {
            log::warn!(
                target: "history",
                "settings store unreadable for retention: {err}; using default"
            );
            return 30;
        }
    };
    store
        .get(settings::KEY_HISTORY_RETENTION_DAYS)
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(30)
}

/// How often the retention-purge task re-checks the user's retention
/// preference and purges stale rows (plan 039 task 47). `tokio::time::
/// interval`'s first tick fires immediately, so this single interval
/// IS the launch pass as well — there is no separate one-shot spawn.
/// 6h is frequent enough to keep both tables bounded on a long-lived,
/// rarely-restarted install without adding any meaningful background load.
const RETENTION_PURGE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Spawn the combined retention-purge task covering both stores that share
/// `history.sqlite3` (plan 039 task 47): dictation history (user's
/// `history.retention_days` slider) and `api_calls` (fixed
/// [`API_CALLS_RETENTION_DAYS`], no user-facing control). Either argument
/// may be `None` if its store failed to open at launch — that half of the
/// pass is skipped, matching the non-fatal posture the rest of storage
/// bootstrap already follows.
///
/// Runs for the lifetime of the process; the caller drops the returned
/// `JoinHandle` on the floor (mirrors `prices_refresher::spawn`).
fn spawn_retention_purge_task(
    app_handle: AppHandle,
    history: Option<Arc<HistoryStore>>,
    usage: Option<Arc<UsageStore>>,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(RETENTION_PURGE_INTERVAL);
        loop {
            ticker.tick().await;
            let retention_days = read_retention_days(&app_handle);
            let history_pass = history.clone();
            let usage_pass = usage.clone();
            // Blocking SQLite work off the async runtime, same posture as
            // every other store call site in `setup`.
            let joined = tauri::async_runtime::spawn_blocking(move || {
                if let Some(store) = history_pass {
                    match store.purge_older_than(retention_days) {
                        Ok(0) => log::debug!(target: "history", "retention purge: 0 stale rows"),
                        Ok(n) => {
                            log::info!(target: "history", "retention purge: removed {n} stale rows")
                        }
                        Err(err) => log::warn!(target: "history", "retention purge failed: {err}"),
                    }
                }
                if let Some(store) = usage_pass {
                    match store.purge_api_calls_older_than(API_CALLS_RETENTION_DAYS) {
                        Ok(0) => log::debug!(target: "usage", "api_calls purge: 0 stale rows"),
                        Ok(n) => {
                            log::info!(target: "usage", "api_calls purge: removed {n} stale rows")
                        }
                        Err(err) => log::warn!(target: "usage", "api_calls purge failed: {err}"),
                    }
                    // Plan 041 (wave 1) — the press-timing ledger shares the
                    // `api_calls` retention window (no user-facing control).
                    match store.purge_press_timings_older_than(API_CALLS_RETENTION_DAYS) {
                        Ok(0) => {
                            log::debug!(target: "usage", "press_timings purge: 0 stale rows")
                        }
                        Ok(n) => log::info!(
                            target: "usage",
                            "press_timings purge: removed {n} stale rows"
                        ),
                        Err(err) => {
                            log::warn!(target: "usage", "press_timings purge failed: {err}")
                        }
                    }
                }
            })
            .await;
            if let Err(err) = joined {
                log::warn!(target: "history", "retention purge task panicked: {err}");
            }
        }
    })
}

/// Feature 033 — read the analytics (PostHog) toggle from the live settings
/// store. Reads the raw `settings.json` directly (via the shared telemetry
/// helper) rather than through `tauri-plugin-store`, which silently swallows
/// file-load errors and would collapse a corrupt/unreadable store into the
/// key-absent case. The two cases are split on purpose (plan 039 task 40a):
///
/// - **Key/store absent** (fresh install, or the key was never written):
///   default-on, so a new user reports operational health until they opt out.
/// - **Store present but unreadable/corrupt**: fail **CLOSED** — analytics off
///   this boot. The user may have opted out and we can't see it; silently
///   re-enrolling them would violate consent.
///
/// The toggle is read ONCE here at boot and cached inside the telemetry handle;
/// flipping it later takes effect on next launch (same posture as the crash
/// toggle's pre-runtime read).
fn read_analytics_toggle(app_local_data_dir: &std::path::Path) -> bool {
    telemetry::read_consent_toggle(app_local_data_dir, settings::KEY_ANALYTICS_ENABLED)
}

/// Reconcile the OS launch-at-login state with the user's stored
/// `general.launch_at_login` preference. Called at boot so a manual edit
/// to the launch agent (or a migration from Swift v1) can't drift out of
/// sync with the in-app toggle. Logged-only on failure — the user can
/// still toggle from Settings → General.
pub fn reconcile_launch_at_login(app: &AppHandle) {
    // Migration: any `Muni*.plist` left over from the LaunchAgent-mode
    // builds would cause launchd to start the app *in addition to* the
    // login item — double-launch on next login. Sweep on every boot.
    // Cheap (a couple stat calls) and unambiguous since we no longer
    // write plists.
    sweep_legacy_launch_agent_plists();

    // One-time: remove the AppleScript-created login item left by
    // pre-SMAppService builds. SMAppService keeps a *separate* record, so
    // a surviving legacy entry would double-launch the app at login.
    migrate_legacy_login_item(app);

    let want_enabled = read_launch_at_login_pref(app);
    let is_enabled = launch_item::is_enabled();
    if want_enabled == is_enabled {
        log::debug!(
            target: "autostart",
            "launch_at_login already in sync (enabled={is_enabled})"
        );
        return;
    }
    // Dev-build guard: never register a Login Item that points at
    // `target/debug` or `target/release/bundle` — the path is unstable
    // across rebuilds. Disabling is still allowed so users can clean up
    // entries left over from a previous installed build. (Manual QA §13.)
    if want_enabled && current_exe_is_dev_bundle() {
        log::warn!(
            target: "autostart",
            "skipping reconcile to enabled=true: running from a dev/build-output bundle"
        );
        return;
    }
    match launch_item::set_enabled(want_enabled) {
        Ok(()) => log::info!(
            target: "autostart",
            "launch_at_login reconciled to {want_enabled}"
        ),
        Err(err) => log::warn!(
            target: "autostart",
            "reconcile to {want_enabled} failed: {err}"
        ),
    }
}

/// One-time removal of the legacy AppleScript login item created by
/// pre-SMAppService builds. Guarded by a stored marker so it runs at most
/// once per install.
///
/// We only attempt the (osascript-driven) delete when the stored preference
/// says Launch-at-Login was on — i.e. when a legacy entry actually exists.
/// Anyone in that state already granted "control System Events" under the old
/// mechanism, so the delete is silent. Fresh installs (pref off) skip it
/// entirely and therefore never see an Automation prompt.
fn migrate_legacy_login_item(app: &AppHandle) {
    let store = match app.store(settings::SETTINGS_FILE) {
        Ok(store) => store,
        Err(err) => {
            log::warn!(target: "autostart", "legacy login-item migration: open store failed: {err}");
            return;
        }
    };

    let already_migrated = store
        .get(settings::KEY_GENERAL_LAUNCH_ITEM_MIGRATED)
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if already_migrated {
        return;
    }

    // `read_launch_at_login_pref` reflects the old mechanism's enabled state;
    // if it was on, a legacy AppleScript login item is present and must go.
    if read_launch_at_login_pref(app) && !current_exe_is_dev_bundle() {
        log::info!(
            target: "autostart",
            "migrating launch-at-login: removing legacy AppleScript login item"
        );
        launch_item::remove_legacy_login_item();
    }

    store.set(settings::KEY_GENERAL_LAUNCH_ITEM_MIGRATED, json!(true));
    if let Err(err) = store.save() {
        log::warn!(target: "autostart", "legacy login-item migration: save marker failed: {err}");
    }
}

/// Remove any stale `~/Library/LaunchAgents/Muni{,-dev}.plist` left by
/// older LaunchAgent-mode builds. A no-op once both files are gone, so
/// it's safe to invoke on every boot. We don't bother with `launchctl
/// bootout` — the next login is when these would have mattered, and
/// the plist absence at that point is enough to keep launchd from
/// re-spawning us as a legacy agent.
fn sweep_legacy_launch_agent_plists() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let dir = std::path::Path::new(&home)
        .join("Library")
        .join("LaunchAgents");
    for name in ["Muni.plist", "Muni-dev.plist"] {
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => log::info!(
                target: "autostart",
                "swept legacy LaunchAgent plist: {}",
                path.display()
            ),
            Err(err) => log::warn!(
                target: "autostart",
                "couldn't remove legacy plist {}: {err}",
                path.display()
            ),
        }
    }
}

/// Returns `true` when `path` looks like a build-output bundle whose
/// location isn't stable across rebuilds (so it shouldn't be registered
/// as a Login Item). The check is a single lookup for a `target`
/// component, which catches `target/debug/...`, `target/release/...`,
/// and `target/release/bundle/macos/Muni.app/...` alike.
///
/// This guard exists because dev sessions toggling Launch at Login
/// register the dev bundle path with macOS's LSSharedFileList. After a
/// logout/login, macOS launches that dev bundle, but its TCC posture
/// is fragile across login sessions; meanwhile the path itself can
/// move with each `cargo clean`. Manual QA §13 hit a four-zombie pile
/// + LaunchServices `GetProcessPID` confusion on a single login cycle.
pub(crate) fn is_dev_bundle_path(path: &std::path::Path) -> bool {
    path.components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("target"))
}

/// Convenience: `true` when the running executable resolves to a dev
/// bundle. Returns `false` if `current_exe()` fails so the production
/// path is the safe default — failing-open here matches the production
/// invariant that an installed bundle path never contains `target`.
pub(crate) fn current_exe_is_dev_bundle() -> bool {
    std::env::current_exe()
        .map(|p| is_dev_bundle_path(&p))
        .unwrap_or(false)
}

/// Env var that picks the text-LID provider at boot. Values:
/// `"groq"` (default) or `"gemini"`. Anything else falls back to the
/// default with a warning. Read once in [`build_text_lid_classifier`];
/// flipping it mid-run has no effect until relaunch.
///
/// Default switched from `gemini` to `groq` (backlog 0011 dogfood):
/// Gemini's `gemini-2.5-flash-lite` classify call typically takes
/// 600+ ms per press, which combined with Whisper transcribe (~370 ms)
/// overflows `RELEASE_LID_WAIT = 1000 ms` for short pure-English
/// presses (sub-1.5 s audio). Groq's chat models return in ~200-400 ms
/// classify time, fitting the budget. Backlog 0009 introduced the
/// Groq option; the default-switch completed that intent. The
/// per-model default (currently `openai/gpt-oss-120b`) lives in
/// `groq_lid::DEFAULT_MODEL`; see its docstring for the rationale
/// behind the model choice.
const LID_PROVIDER_ENV: &str = "MUNI_LID_PROVIDER";

/// Env var that overrides the chosen provider's default model. When
/// unset, each provider uses its own `DEFAULT_MODEL` constant.
const LID_MODEL_ENV: &str = "MUNI_LID_MODEL";

/// Backlog 0012 — opts the LID task into hybrid mode. When set to
/// `"true"` (case-insensitive), [`build_secondary_lid_classifier`]
/// returns a Gemini client that the LID task fires in parallel with
/// the primary on the pass#2 slice. Off by default while we collect
/// dogfood evidence for cost vs. accuracy on long-English presses.
/// Anything else (unset, `"false"`, malformed) keeps the press flow
/// identical to backlog 0011.
const LID_HYBRID_ENV: &str = "MUNI_LID_HYBRID";

/// Feature 021 — gates the audio-LID + parallel text-LID hybrid
/// path (provider-agnostic; the actual secondary classifier is
/// chosen at boot by [`build_audio_hybrid_secondary_classifier`]).
/// Default `true`. Set to `"false"` (case-insensitive) to fall
/// back to feature 020's pure audio-LID behaviour.
///
/// This is the audio-LID-side analogue of [`LID_HYBRID_ENV`]: that
/// env var gates the text-LID-primary hybrid (backlog 0012); this
/// one gates the audio-LID-primary hybrid (feature 021). They are
/// independent toggles for independent code paths — do not collapse.
pub const MUNI_LID_AUDIO_HYBRID_ENV: &str = "MUNI_LID_AUDIO_HYBRID";

/// Feature 021 — read [`MUNI_LID_AUDIO_HYBRID_ENV`] once at
/// boot. Defaults to `true` so the J49-class Tagalog-recovery flow
/// ships on by default; set the env var to `"false"`
/// (case-insensitive) to fall back to feature 020 (audio-LID alone).
fn resolve_audio_hybrid_enabled() -> bool {
    std::env::var(MUNI_LID_AUDIO_HYBRID_ENV)
        .ok()
        .map(|v| !v.trim().eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

/// Feature 023 (backlog 0040) — silent-press VAD gate toggles. The
/// gate is on by default; calibration knobs (threshold +
/// min-speech-ms) are env-tunable so dogfood iterations can adjust
/// without a rebuild. `MUNI_VAD_REQUIRED=1` promotes load failures
/// from "degrade gracefully" to "panic at boot" — useful in CI / dev.
pub const MUNI_VAD_GATE_ENV: &str = "MUNI_VAD_GATE";
pub const MUNI_VAD_THRESHOLD_ENV: &str = "MUNI_VAD_THRESHOLD";
pub const MUNI_VAD_MIN_SPEECH_MS_ENV: &str = "MUNI_VAD_MIN_SPEECH_MS";
pub const MUNI_VAD_REQUIRED_ENV: &str = "MUNI_VAD_REQUIRED";

/// Feature 024 (backlog 0042) — kill switch for streaming VAD's Site D
/// (hybrid buffer gate). Default `on` post-dogfood (2026-05-21); set to
/// `0|off|false|no` to disable.
pub const MUNI_VAD_STREAM_HYBRID_ENV: &str = "MUNI_VAD_STREAM_HYBRID";

/// Feature 024 (backlog 0042) — kill switch for streaming VAD's Site E
/// (release-path Whisper batch buffer trim). Independent from the Site
/// D switch so each gate can be calibrated in isolation. Default `on`
/// post-dogfood (2026-05-21); set to `0|off|false|no` to disable.
pub const MUNI_VAD_TRIM_RELEASE_BUFFER_ENV: &str = "MUNI_VAD_TRIM_RELEASE_BUFFER";

/// Feature 025 (backlog 0046) — kill switch for the audio-LID windowing
/// loop's per-window silence gate. **Default ON** post-dogfood
/// (2026-05-21). Skips whisper.cpp tiny-q5_1 classify calls when the
/// candidate 2 s window contains zero speech frames. Dogfood verdicts
/// in `docs/findings/004_audio_lid_silence_gate_dogfood.md` settled all
/// six rubric scenarios — gate saves ~700–800 ms of GPU work per
/// long-silence press without functional regression. Set to
/// `0|off|false|no` (case-insensitive) to disable.
pub const MUNI_VAD_AUDIO_LID_GATE_ENV: &str = "MUNI_VAD_AUDIO_LID_GATE";

/// Feature 024 (backlog 0042) — continuous-silence duration before the
/// streaming detector latches suppression. Default 500 ms (conservative
/// per brainstorm 006 § Decision 3). Higher values trim less
/// aggressively but better protect against false-positive word
/// clipping.
pub const MUNI_VAD_STREAM_MIN_SILENCE_MS_ENV: &str = "MUNI_VAD_STREAM_MIN_SILENCE_MS";

/// Feature 024 (backlog 0042) — post-silence-end frames passed
/// unconditionally to protect word starts. Default 500 ms. The
/// unconditional pass makes first-phoneme clipping mathematically
/// impossible after a silence stretch ends.
pub const MUNI_VAD_STREAM_WORD_GUARD_MS_ENV: &str = "MUNI_VAD_STREAM_WORD_GUARD_MS";

/// Feature 023 — resolve `MUNI_VAD_GATE`. Defaults to `true` so the
/// gate ships on. Treats `0|off|false|no` (case-insensitive) as off
/// to match the project's existing boolean-env conventions
/// (`resolve_audio_hybrid_enabled` uses just `"false"`; we add the
/// `0|off|no` synonyms because the README guidance is more permissive).
fn resolve_vad_enabled() -> bool {
    std::env::var(MUNI_VAD_GATE_ENV)
        .ok()
        .map(|v| {
            let t = v.trim().to_lowercase();
            !matches!(t.as_str(), "0" | "off" | "false" | "no")
        })
        .unwrap_or(true)
}

/// Feature 023 — resolve `MUNI_VAD_THRESHOLD`. Falls back to
/// [`vad::VAD_DEFAULT_THRESHOLD`] when the env is unset, unparseable,
/// or out of `[0.0, 1.0]`. The clamp is correctness-critical: a
/// negative or NaN threshold would gate every press as "speech" or
/// "silence" and break either correctness or the cost-savings goal.
fn resolve_vad_threshold() -> f32 {
    std::env::var(MUNI_VAD_THRESHOLD_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|t| (0.0..=1.0).contains(t))
        .unwrap_or(vad::VAD_DEFAULT_THRESHOLD)
}

/// Feature 023 — resolve `MUNI_VAD_MIN_SPEECH_MS`. Falls back to
/// [`vad::VAD_DEFAULT_MIN_SPEECH_MS`] when unset / unparseable / zero.
/// Zero would short-circuit the P2 policy to "any single frame above
/// threshold counts as speech," reintroducing the false-positive
/// surface the duration arm was added to prevent.
fn resolve_vad_min_speech_ms() -> u32 {
    std::env::var(MUNI_VAD_MIN_SPEECH_MS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|m| *m > 0)
        .unwrap_or(vad::VAD_DEFAULT_MIN_SPEECH_MS)
}

/// Feature 024 (backlog 0042) — resolve `MUNI_VAD_STREAM_HYBRID`.
/// **Default ON** post-dogfood (2026-05-21). Scenarios 1–7 and 9 in
/// `docs/qa/024_streaming_vad_midpress_silence.md` all passed at the
/// 500 ms / 500 ms calibration knobs; the two calibration-axis
/// scenarios (3 + 4) cleared without word-clipping. Explicit off via
/// `MUNI_VAD_STREAM_HYBRID=0|off|false|no` remains the kill switch.
pub(crate) fn resolve_vad_stream_hybrid_enabled() -> bool {
    std::env::var(MUNI_VAD_STREAM_HYBRID_ENV)
        .ok()
        .map(|v| {
            let t = v.trim().to_lowercase();
            !matches!(t.as_str(), "0" | "off" | "false" | "no")
        })
        .unwrap_or(true)
}

/// Feature 024 (backlog 0042) — resolve `MUNI_VAD_TRIM_RELEASE_BUFFER`.
/// **Default ON** post-dogfood (2026-05-21). The two streaming-VAD kill
/// switches are independent so calibration of Site D and Site E can
/// iterate separately. Explicit off via
/// `MUNI_VAD_TRIM_RELEASE_BUFFER=0|off|false|no` remains the kill
/// switch.
pub(crate) fn resolve_vad_trim_release_buffer_enabled() -> bool {
    std::env::var(MUNI_VAD_TRIM_RELEASE_BUFFER_ENV)
        .ok()
        .map(|v| {
            let t = v.trim().to_lowercase();
            !matches!(t.as_str(), "0" | "off" | "false" | "no")
        })
        .unwrap_or(true)
}

/// Feature 025 (backlog 0046) — resolve `MUNI_VAD_AUDIO_LID_GATE`.
/// **Default ON** post-dogfood (2026-05-21). All six scenarios in
/// `docs/qa/025_audio_lid_silence_gate.md` passed (see
/// `docs/findings/004_audio_lid_silence_gate_dogfood.md`): no regression
/// vs gate-off baseline, ~700–800 ms of GPU work saved per long-silence
/// press, first-window protection intact, warm-state cost 1.43× the
/// pre-silence median (under the 1.5× threshold — no heartbeat needed).
/// Explicit off via `MUNI_VAD_AUDIO_LID_GATE=0|off|false|no` remains
/// the kill switch.
pub(crate) fn resolve_vad_audio_lid_gate_enabled() -> bool {
    std::env::var(MUNI_VAD_AUDIO_LID_GATE_ENV)
        .ok()
        .map(|v| {
            let t = v.trim().to_lowercase();
            !matches!(t.as_str(), "0" | "off" | "false" | "no")
        })
        .unwrap_or(true)
}

/// Feature 024 (backlog 0042) — resolve `MUNI_VAD_STREAM_MIN_SILENCE_MS`.
/// Falls back to [`vad::STREAM_DEFAULT_MIN_SILENCE_MS`] when unset /
/// unparseable / out of `[50, 10_000]`. The sanity floor of 50 ms
/// matches the smallest physically-meaningful silence gap; values below
/// it would degenerate to per-frame thrashing.
fn resolve_vad_stream_min_silence_ms() -> u32 {
    std::env::var(MUNI_VAD_STREAM_MIN_SILENCE_MS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|ms| (50..=10_000).contains(ms))
        .unwrap_or(vad::STREAM_DEFAULT_MIN_SILENCE_MS)
}

/// Feature 024 (backlog 0042) — resolve `MUNI_VAD_STREAM_WORD_GUARD_MS`.
/// Falls back to [`vad::STREAM_DEFAULT_WORD_GUARD_MS`] when unset /
/// unparseable / above 5 s. `0` is explicitly allowed (disables the
/// guard window — useful for dogfood A/B testing the guard's effect).
fn resolve_vad_stream_word_guard_ms() -> u32 {
    std::env::var(MUNI_VAD_STREAM_WORD_GUARD_MS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|ms| *ms <= 5_000)
        .unwrap_or(vad::STREAM_DEFAULT_WORD_GUARD_MS)
}

/// Construct the text-LID classifier from `MUNI_LID_PROVIDER` and
/// `MUNI_LID_MODEL`. Returns `None` if construction fails — the LID
/// task already has a graceful-degradation path (default each press
/// to Whisper) for that case, so init failures don't block boot.
///
/// Pulled out as a free function so the env-resolution logic is
/// testable and the `setup` callback stays thin.
/// Plan 041 (task 6) — construct a [`GroqLidClient`] on the shared Groq
/// connection pool when a shared client is available, or via the
/// standalone `with_model` constructor otherwise (tests / boot-fallback
/// when the shared client failed to build). Both target
/// [`groq_lid::DEFAULT_ENDPOINT`]; the only difference is which pool the
/// client dials through.
fn build_groq_lid_client(
    shared_http: Option<&reqwest::Client>,
    model: String,
) -> Result<GroqLidClient, MuniError> {
    match shared_http {
        Some(http) => Ok(GroqLidClient::with_http_client(
            http.clone(),
            groq_lid::DEFAULT_ENDPOINT.to_string(),
            model,
        )),
        None => GroqLidClient::with_model(model),
    }
}

fn build_text_lid_classifier(
    shared_http: Option<&reqwest::Client>,
) -> Option<Arc<dyn TextLidClassifier>> {
    let provider = resolve_lid_provider_slug();
    let model_override = std::env::var(LID_MODEL_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    build_text_lid_classifier_for_provider(
        shared_http,
        provider.as_str(),
        model_override.as_deref(),
    )
}

/// Feature 020 — provider slug that selects the local whisper.cpp
/// audio-LID instead of any text-LID backend. When
/// `MUNI_LID_PROVIDER=audio_whisper_tiny` *or* the env var is unset
/// (the new default since 2026-05-18), [`build_audio_lid_classifier`]
/// returns `Some(WhisperAudioLid)` and [`build_text_lid_classifier`]
/// returns `None` — the two factories are mutually exclusive at boot.
pub const AUDIO_LID_PROVIDER_AUDIO_WHISPER_TINY: &str = "audio_whisper_tiny";

/// Feature 020 — slug that resolves the live `MUNI_LID_PROVIDER` env
/// var with audio-LID as the *default* when unset. Empty / missing →
/// audio_whisper_tiny; trimmed + lowercased so legacy launch configs
/// (mixed-case slugs, trailing whitespace) keep booting cleanly.
fn resolve_lid_provider_slug() -> String {
    let raw = std::env::var(LID_PROVIDER_ENV).unwrap_or_default();
    let trimmed = raw.trim().to_lowercase();
    if trimmed.is_empty() {
        AUDIO_LID_PROVIDER_AUDIO_WHISPER_TINY.to_string()
    } else {
        trimmed
    }
}

/// Pure dispatch of the text-LID factory. Reads no env vars; takes the
/// resolved provider slug and the optional model override as
/// arguments. Returns `None` when the slug is the audio-LID slug or
/// unrecognised — the audio-LID factory ([`build_audio_lid_classifier`])
/// owns the audio path.
///
/// Pulled out as a pure function (feature 020) so the rollback smoke
/// tests can exercise the groq/gemini construction paths without
/// touching process-wide env vars.
fn build_text_lid_classifier_for_provider(
    shared_http: Option<&reqwest::Client>,
    provider: &str,
    model_override: Option<&str>,
) -> Option<Arc<dyn TextLidClassifier>> {
    let result = match provider {
        "groq" => {
            let model = model_override
                .map(|s| s.to_string())
                .unwrap_or_else(|| groq_lid::DEFAULT_MODEL.to_string());
            log::info!(target: "lid", "boot: text-LID provider=groq model={model}");
            // Plan 041 (task 6) — inject the shared Groq pool when
            // available; the standalone `with_model` fallback keeps the
            // rollback smoke tests key-free and pool-agnostic.
            let client = build_groq_lid_client(shared_http, model);
            client.map(|c| Arc::new(c) as Arc<dyn TextLidClassifier>)
        }
        "gemini" => {
            let model = model_override
                .map(|s| s.to_string())
                .unwrap_or_else(|| gemini_lid::DEFAULT_MODEL.to_string());
            log::info!(target: "lid", "boot: text-LID provider=gemini model={model}");
            GeminiLidClient::with_model(model).map(|c| Arc::new(c) as Arc<dyn TextLidClassifier>)
        }
        // Audio-LID slug (and empty, post default-flip) is handled by
        // the separate audio factory; text-LID dispatch returns None
        // so the rollback path sits dormant when audio-LID is active.
        AUDIO_LID_PROVIDER_AUDIO_WHISPER_TINY | "" => {
            log::debug!(
                target: "lid",
                "text-LID factory yielding to audio-LID for provider={provider:?}"
            );
            return None;
        }
        unknown => {
            // Feature 020 default flip: an unrecognised value no
            // longer silently falls through to groq. It logs and
            // yields to audio-LID — the new safe default. Setting a
            // real rollback explicitly is `groq` or `gemini`.
            log::warn!(
                target: "lid",
                "unknown {LID_PROVIDER_ENV}={unknown:?} — yielding to audio-LID default"
            );
            return None;
        }
    };

    match result {
        Ok(client) => Some(client),
        Err(err) => {
            log::error!(
                target: "lid",
                "text-LID classifier init failed: {} ({:?}) — auto-detect will default every press to Whisper",
                err.user_message(),
                err.severity()
            );
            None
        }
    }
}

/// Feature 020 — local audio-LID factory. Returns `Some(WhisperAudioLid)`
/// only when `MUNI_LID_PROVIDER=audio_whisper_tiny`; otherwise returns
/// `None`. Mirrors [`build_text_lid_classifier`]'s graceful-degradation
/// semantics — init failures log loudly but do not block boot, and the
/// orchestrator's "no LID classifier → default press to multilingual"
/// fallback absorbs the loss.
fn build_audio_lid_classifier(app: &AppHandle) -> Option<Arc<dyn AudioLidClassifier>> {
    let provider = resolve_lid_provider_slug();
    // Audio-LID owns the audio slug and the unset-default path. The
    // text-LID rollback slugs (`groq`, `gemini`) yield to the text
    // factory below; unknown values also yield to audio per the
    // default-on-unknown rule (feature 020 default flip).
    let dispatches_to_audio = matches!(provider.as_str(), AUDIO_LID_PROVIDER_AUDIO_WHISPER_TINY)
        || !matches!(provider.as_str(), "groq" | "gemini");
    if !dispatches_to_audio {
        return None;
    }
    match WhisperAudioLid::from_app(app) {
        Ok(client) => {
            log::info!(
                target: "lid",
                "boot: audio-LID provider={AUDIO_LID_PROVIDER_AUDIO_WHISPER_TINY} ({})",
                client.provider_label()
            );
            Some(Arc::new(client) as Arc<dyn AudioLidClassifier>)
        }
        Err(err) => {
            log::error!(
                target: "lid",
                "audio-LID classifier init failed: {} ({:?}) — auto-detect will default every press to multilingual",
                err.user_message(),
                err.severity()
            );
            None
        }
    }
}

/// Backlog 0012 — env-gated secondary LID classifier for the
/// text-LID-primary hybrid path (`MUNI_LID_PROVIDER=groq|gemini`).
/// Returns `Some(GeminiLidClient)` when `MUNI_LID_HYBRID=true`,
/// `None` otherwise. Fires in parallel with Groq's pass#2; a
/// Gemini-via-secondary `english` verdict overrides Groq's
/// `Whisper` verdict on long-English-leading presses (the
/// long-English failure mode backlog 0011 left open).
///
/// This is the text-LID-primary side of the hybrid story. The
/// audio-LID-primary side uses [`build_audio_hybrid_secondary_classifier`]
/// — a separate factory (since 2026-05-18 dogfood, feature 021)
/// because Gemini's classify latency tail (~1.7 s median, 4 s p95)
/// is too slow to land before press finalisation on most presses.
///
/// Construction failures degrade gracefully — primary stays wired,
/// hybrid override silently disables.
fn build_secondary_lid_classifier() -> Option<Arc<dyn TextLidClassifier>> {
    // Default true since 2026-05-14: the gemini secondary catches the
    // long-English-leading failure mode (backlog 0011) at a small per-press
    // cost. Set `MUNI_LID_HYBRID=false` (case-insensitive) to disable.
    let hybrid = std::env::var(LID_HYBRID_ENV)
        .ok()
        .map(|v| !v.trim().eq_ignore_ascii_case("false"))
        .unwrap_or(true);
    if !hybrid {
        return None;
    }
    let model = gemini_lid::DEFAULT_MODEL.to_string();
    log::info!(
        target: "lid",
        "boot: text-LID secondary=gemini model={model} (hybrid mode enabled via {LID_HYBRID_ENV})"
    );
    match GeminiLidClient::with_model(model) {
        Ok(c) => Some(Arc::new(c) as Arc<dyn TextLidClassifier>),
        Err(err) => {
            log::error!(
                target: "lid",
                "secondary Gemini LID init failed: {} ({:?}) — hybrid override disabled",
                err.user_message(),
                err.severity()
            );
            None
        }
    }
}

/// Feature 021 — secondary text-LID classifier specifically for the
/// audio-LID-primary hybrid path. Defaults to Groq (`openai/gpt-oss-120b`,
/// ~298 ms median classify) instead of Gemini (~1.7 s median, 4 s p95)
/// because the audio-LID hybrid needs to land its verdict before the
/// press releases — Gemini's latency tail caused most overrides to
/// arrive too late to actually flip the route in 2026-05-18 dogfood.
///
/// Gated by [`MUNI_LID_AUDIO_HYBRID_ENV`]. Construction failures
/// degrade gracefully — audio-LID alone handles the press if the
/// secondary is unavailable.
fn build_audio_hybrid_secondary_classifier(
    shared_http: Option<&reqwest::Client>,
) -> Option<Arc<dyn TextLidClassifier>> {
    if !resolve_audio_hybrid_enabled() {
        return None;
    }
    let model = groq_lid::DEFAULT_MODEL.to_string();
    log::info!(
        target: "lid",
        "boot: audio-LID hybrid secondary=groq model={model} (enabled via {MUNI_LID_AUDIO_HYBRID_ENV})"
    );
    match build_groq_lid_client(shared_http, model) {
        Ok(c) => Some(Arc::new(c) as Arc<dyn TextLidClassifier>),
        Err(err) => {
            log::error!(
                target: "lid",
                "audio-LID hybrid secondary Groq LID init failed: {} ({:?}) — audio-LID alone will handle presses",
                err.user_message(),
                err.severity()
            );
            None
        }
    }
}

/// Feature 023 (backlog 0040) — build the silent-press VAD gate.
/// Returns `None` when the gate is disabled via [`MUNI_VAD_GATE_ENV`].
/// On Silero load failure: degrades to `None` by default (callers
/// treat `None` as "no gate" and proceed unchanged); set
/// [`MUNI_VAD_REQUIRED_ENV`] to `1|true|yes|on` to promote the failure
/// to a boot-time panic for CI / dev assertions.
fn build_vad_detector() -> Option<Arc<dyn vad::VadDetector>> {
    if !resolve_vad_enabled() {
        log::info!(
            target: "vad",
            "boot: VAD gate disabled via {MUNI_VAD_GATE_ENV} — skipping detector init"
        );
        return None;
    }
    let threshold = resolve_vad_threshold();
    let min_speech_ms = resolve_vad_min_speech_ms();
    match vad::SileroVad::new(threshold, min_speech_ms) {
        Ok(d) => Some(Arc::new(d) as Arc<dyn vad::VadDetector>),
        Err(err) => {
            let required = std::env::var(MUNI_VAD_REQUIRED_ENV)
                .ok()
                .map(|v| {
                    matches!(
                        v.trim().to_lowercase().as_str(),
                        "1" | "true" | "yes" | "on"
                    )
                })
                .unwrap_or(false);
            if required {
                panic!(
                    "{MUNI_VAD_REQUIRED_ENV} set but VAD init failed: {}",
                    err.user_message()
                );
            }
            log::warn!(
                target: "vad",
                "VAD detector init failed: {} — gate disabled for rest of session (set {MUNI_VAD_REQUIRED_ENV}=1 to hardfail)",
                err.user_message()
            );
            None
        }
    }
}

/// Feature 024 (backlog 0042) — build the streaming-VAD factory. Returns
/// `None` when all three streaming-VAD switches are off
/// (`MUNI_VAD_STREAM_HYBRID`, `MUNI_VAD_TRIM_RELEASE_BUFFER`, and
/// feat/025's `MUNI_VAD_AUDIO_LID_GATE`); `Some(factory)` when at least
/// one is on.
///
/// The factory is a closure constructed at boot with the resolved
/// calibration knobs baked in; each invocation builds a fresh
/// [`vad::SileroStreamingVad`] for one stream's lifetime. Per-stream
/// instances mean no shared Mutex, no cross-stream contention, and
/// per-instance fail-open on inference errors. Feature 025's audio-LID
/// silence gate is the third site that consumes per-task instances
/// from this factory — see [`session::DictationSession::run_audio_lid_pass`].
///
/// On construction failure (Silero ONNX load error) the factory falls
/// back to [`vad::PassThroughStreamingVad`] so the caller never sees
/// `None` on per-stream construction failures — fail-open at the
/// per-stream construction layer too, matching the batch-VAD precedent.
fn build_streaming_vad_factory() -> Option<vad::StreamingVadFactory> {
    let hybrid_on = resolve_vad_stream_hybrid_enabled();
    let trim_on = resolve_vad_trim_release_buffer_enabled();
    let audio_lid_gate_on = resolve_vad_audio_lid_gate_enabled();
    if !hybrid_on && !trim_on && !audio_lid_gate_on {
        log::info!(
            target: "vad",
            "boot: streaming VAD disabled ({MUNI_VAD_STREAM_HYBRID_ENV}=off, {MUNI_VAD_TRIM_RELEASE_BUFFER_ENV}=off, {MUNI_VAD_AUDIO_LID_GATE_ENV}=off)"
        );
        return None;
    }
    let threshold = resolve_vad_threshold();
    let min_silence_ms = resolve_vad_stream_min_silence_ms();
    let word_guard_ms = resolve_vad_stream_word_guard_ms();
    log::info!(
        target: "vad",
        "boot: streaming VAD enabled (hybrid={hybrid_on}, trim_release={trim_on}, audio_lid_gate={audio_lid_gate_on}, threshold={threshold:.2}, min_silence_ms={min_silence_ms}, word_guard_ms={word_guard_ms})"
    );
    Some(Arc::new(move || {
        match vad::SileroStreamingVad::new(threshold, min_silence_ms, word_guard_ms) {
            Ok(d) => Box::new(d) as Box<dyn vad::StreamingVadDetector>,
            Err(err) => {
                log::warn!(
                    target: "vad",
                    "streaming VAD construction failed: {} — using pass-through impl (fail-open)",
                    err.user_message()
                );
                Box::new(vad::PassThroughStreamingVad) as Box<dyn vad::StreamingVadDetector>
            }
        }
    }))
}

/// On first run (`price_history` table empty), insert every entry
/// from [`pricing::DEFAULT_PRICES`] under the current UTC month so the
/// very first dictation can compute a cost without waiting for the
/// scraper API fetch.
///
/// Subsequent launches see a non-empty table and short-circuit; the
/// hourly refresher takes over from there.
fn seed_default_prices_if_empty(store: &Arc<UsageStore>) {
    // Idempotent per-entry upsert under the current UTC month so new
    // rows added to `DEFAULT_PRICES` (e.g. when wiring up a new ASR
    // backend) get picked up on the next launch, not only at first
    // run. Previously gated on `count_price_history() == 0`, which
    // left existing installs with `cost = NULL` rows for any newly-
    // added provider/model — surfaced during `docs/qa/004` §3
    // dogfood (2026-05-11): `[usage][WARN] no price for
    // gladia/solaria-1 in 2026-05 — recording cost=NULL`. The upsert
    // ON CONFLICT clause means re-running is cheap and won't
    // override a fresher row written by `prices_refresher`.
    let yyyymm = current_utc_yyyymm();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut seeded = 0usize;
    for entry in DEFAULT_PRICES {
        // Skip if this (month, provider, model) already exists — we
        // do not want to clobber a refresher-written row with the
        // stale baked-in seed. The refresher is the authoritative
        // source post-bootstrap.
        match store.lookup_price(&yyyymm, entry.provider, entry.model) {
            Ok(Some(_)) => continue,
            Ok(None) => {} // missing — fall through to upsert
            Err(err) => {
                log::warn!(
                    target: "usage",
                    "lookup_price failed during seed for {}/{}: {} — attempting upsert anyway",
                    entry.provider,
                    entry.model,
                    err.user_message()
                );
            }
        }
        let row = PriceRow {
            effective_month: yyyymm.clone(),
            provider: entry.provider.into(),
            model: entry.model.into(),
            kind: entry.kind.as_wire().into(),
            usd_per_second: entry.usd_per_second,
            usd_per_input_token: entry.usd_per_input_token,
            usd_per_output_token: entry.usd_per_output_token,
            source_url: Some(entry.source_url.into()),
            fetched_at: now,
        };
        if let Err(err) = store.upsert_price(&row) {
            log::warn!(
                target: "usage",
                "seed upsert failed for {}/{}: {}",
                entry.provider,
                entry.model,
                err.user_message()
            );
            continue;
        }
        seeded += 1;
    }
    if seeded > 0 {
        log::info!(
            target: "usage",
            "seeded {} bootstrap price row(s) for {}",
            seeded,
            yyyymm
        );
    }
}

/// Format `now()` as `YYYY-MM` UTC. Centralised so the seed path,
/// refresher, and price-history storage all agree on the format. This
/// is the *storage* month — `price_history.effective_month` and the
/// cost frozen onto each `api_calls` row are keyed to it. Do not switch
/// this to local time: doing so would change what gets written to the
/// database. The Cost & Usage *display* month is computed separately in
/// the machine's local timezone — see
/// [`usage_store::UsageStore::current_local_yyyymm`].
pub fn current_utc_yyyymm() -> String {
    const FMT: &[time::format_description::FormatItem<'_>] =
        time::macros::format_description!("[year]-[month]");
    time::OffsetDateTime::now_utc()
        .format(&FMT)
        .unwrap_or_else(|_| "1970-01".into())
}

/// Pure resolution of a stored `general.launch_at_login` value, split out
/// from [`read_launch_at_login_pref`] so the unset/malformed fallback is
/// unit-testable without standing up a Tauri store (mirrors
/// [`resolve_repaste_binding`]).
///
/// Unset, unreadable, or malformed → `settings::default_for`'s value
/// (`false`), never a hardcoded `true`. Plan 039 task 39: the boot-time
/// reconcile and the Settings UI must agree on the same fallback, or a
/// corrupted/absent store silently re-enables a Login Item behind the
/// onboarding wizard's back.
fn resolve_launch_at_login_pref(stored: Option<serde_json::Value>) -> bool {
    let default = settings::default_for(settings::KEY_GENERAL_LAUNCH_AT_LOGIN)
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    stored.and_then(|v| v.as_bool()).unwrap_or(default)
}

fn read_launch_at_login_pref(app: &AppHandle) -> bool {
    let store = match app.store(settings::SETTINGS_FILE) {
        Ok(s) => s,
        Err(_) => return resolve_launch_at_login_pref(None),
    };
    resolve_launch_at_login_pref(store.get(settings::KEY_GENERAL_LAUNCH_AT_LOGIN))
}

#[cfg(test)]
mod lib_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn arm_failure_surfaces_only_when_onboarded() {
        // Plan 039 task 31 — a hotkey-arm failure surfaces to the user only on
        // the post-onboarding boot path. The wizard's expected first-failure
        // (which triggers the Input Monitoring prompt) must stay silent.
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let sink = calls.clone();
        let present: error_presenter::PresentError =
            std::sync::Arc::new(move |_err: &MuniError| {
                *sink.lock().expect("poisoned") += 1;
            });

        // Returning user (onboarding complete) → surfaced.
        present_arm_failure(true, &MuniError::InputMonitoringDenied, &present);
        assert_eq!(
            *calls.lock().expect("poisoned"),
            1,
            "onboarded arm failure must reach the presenter"
        );

        // Wizard path (onboarding not complete) → silent.
        present_arm_failure(false, &MuniError::InputMonitoringDenied, &present);
        assert_eq!(
            *calls.lock().expect("poisoned"),
            1,
            "the wizard's expected first-failure must stay silent"
        );
    }

    #[test]
    fn sentry_gate_matches_safe_mode_on_launch_count() {
        // Plan 039 task 35 — the pre-runtime Sentry gate must evaluate the
        // crash-loop predicate on the same value the safe-mode gate sees. With
        // the counter at threshold-1 on disk, `setup`'s `record_launch` bumps it
        // to `threshold` and boots safe mode; this gate reads the pre-bump value
        // and must anticipate the +1, also seeing `threshold` → skip init.
        use crate::boot_health::{is_crash_loop, SAFE_MODE_THRESHOLD};

        let on_disk = SAFE_MODE_THRESHOLD - 1;
        assert!(
            is_crash_loop(effective_launch_count(on_disk)),
            "Sentry gate must skip init on the launch that boots safe mode"
        );

        // One below the trip point: neither gate fires.
        let on_disk = SAFE_MODE_THRESHOLD - 2;
        assert!(
            !is_crash_loop(effective_launch_count(on_disk)),
            "below threshold-1, neither gate should trip"
        );
    }

    /// Feature 038 — the mandatory transition guard on the keyed-dictation
    /// handler. Only a *change* in the held flag forwards an edge; a duplicate
    /// `Pressed`/`Released` (auto-repeat, or the plugin double-firing) is
    /// swallowed so it can never reach the untouched hold/tap/toggle machine —
    /// where a stray `false` reads as release→commit and a spurious pair as
    /// tap→toggle.
    #[test]
    fn is_dictation_transition_forwards_only_real_edges() {
        let held = AtomicBool::new(false);

        // Rising edge: released → pressed forwards once.
        assert!(is_dictation_transition(&held, true));
        // Duplicate Pressed (auto-repeat) is swallowed.
        assert!(!is_dictation_transition(&held, true));
        assert!(!is_dictation_transition(&held, true));
        // Falling edge: pressed → released forwards once.
        assert!(is_dictation_transition(&held, false));
        // Duplicate Released is swallowed.
        assert!(!is_dictation_transition(&held, false));
        // A fresh press after a clean release forwards again.
        assert!(is_dictation_transition(&held, true));
    }

    #[test]
    fn session_state_maps_one_to_one_to_tray_state() {
        assert_eq!(session_to_tray_state(SessionState::Idle), TrayState::Idle);
        assert_eq!(
            session_to_tray_state(SessionState::Listening),
            TrayState::Listening
        );
        assert_eq!(
            session_to_tray_state(SessionState::Cleaning),
            TrayState::Cleaning
        );
        assert_eq!(
            session_to_tray_state(SessionState::Recovering),
            TrayState::Recovering
        );
        assert_eq!(session_to_tray_state(SessionState::Error), TrayState::Error);
    }

    /// Feature 037 regression: an ABSENT re-paste key must resolve to the
    /// enabled `Ctrl+Cmd+V` default — NOT disabled. The original boot loader
    /// used `store.get(key)?`, which conflated an absent key with an explicit
    /// `null` and silently shipped the re-paste hotkey OFF on every fresh
    /// install (the Shortcuts UI still showed `⌃⌘V` from the TS default, so the
    /// backend and UI disagreed). Only a stored `null` — clear-to-disable —
    /// may resolve to disabled.
    #[test]
    fn resolve_repaste_binding_absent_key_uses_enabled_default() {
        // Absent key (never set) → enabled default, the out-of-the-box hotkey.
        assert_eq!(
            resolve_repaste_binding(None),
            Some(hotkey_binding::RepasteBinding::default_repaste()),
            "an absent re-paste key must fall back to the enabled Ctrl+Cmd+V default"
        );
        // Explicit stored `null` → disabled (the user's clear-to-disable choice).
        assert_eq!(
            resolve_repaste_binding(Some(serde_json::Value::Null)),
            None,
            "a stored JSON null is clear-to-disable and must stay disabled"
        );
        // A valid stored object round-trips to exactly that binding.
        let custom = hotkey_binding::RepasteBinding::default_repaste();
        let value = serde_json::to_value(&custom).expect("serialises");
        assert_eq!(resolve_repaste_binding(Some(value)), Some(custom));
        // A malformed value degrades to disabled rather than panicking at boot.
        assert_eq!(
            resolve_repaste_binding(Some(serde_json::json!({ "garbage": true }))),
            None,
            "a malformed stored value degrades to disabled, never a panic"
        );
    }

    /// Plan 039 task 49(a) — a stored re-paste value that is valid JSON but
    /// fails the binding rules resolves to DISABLED (never registered), not to
    /// the enabled default. This is the boot-heal input from task 49's VALIDATE
    /// (`{"mods":[],"key":"KeyA"}` — a bare key). A reserved paste combo (⌘V)
    /// likewise degrades to disabled.
    #[test]
    fn resolve_repaste_binding_invalid_binding_disables() {
        // Valid JSON, but a bare key (no modifier) fails validation → disabled.
        let bare_key = serde_json::json!({ "mods": [], "key": "KeyA" });
        assert_eq!(
            resolve_repaste_binding(Some(bare_key)),
            None,
            "a structurally invalid stored binding must resolve to disabled"
        );
        // ⌘V is a reserved self-injected combo → disabled, never registered.
        let cmd_v = serde_json::json!({ "mods": ["command"], "key": "KeyV" });
        assert_eq!(resolve_repaste_binding(Some(cmd_v)), None);
        // A structurally valid stored binding still round-trips.
        let ok = serde_json::json!({ "mods": ["control", "command"], "key": "KeyV" });
        assert_eq!(
            resolve_repaste_binding(Some(ok)),
            Some(hotkey_binding::RepasteBinding::default_repaste())
        );
    }

    /// Plan 039 task 48(b) — the re-paste in-flight guard: a second press while
    /// one is running is dropped (exactly one paste), and the slot frees once the
    /// guard drops so a later, separate press proceeds.
    #[test]
    fn repaste_in_flight_guard_drops_reentrant_press() {
        let flag = Arc::new(AtomicBool::new(false));
        // First press claims the slot.
        assert!(try_claim_repaste(&flag), "first press claims the slot");
        // A second, overlapping press is dropped.
        assert!(
            !try_claim_repaste(&flag),
            "a re-entrant press must be dropped while one is in flight"
        );
        // Simulate the running task ending: the RAII guard clears the flag.
        {
            let _guard = RepasteInFlightGuard(Arc::clone(&flag));
        }
        assert!(
            !flag.load(Ordering::Acquire),
            "the guard must clear the flag on drop"
        );
        // A later, non-overlapping press proceeds.
        assert!(
            try_claim_repaste(&flag),
            "a fresh press after completion claims the slot again"
        );
    }

    /// Plan 039 task 51(c) — the recording-suppression watchdog generation
    /// counter: a `bump` invalidates any pending watchdog (its captured
    /// generation no longer matches `current`), while the watchdog whose
    /// generation is still current is the one allowed to fire.
    #[test]
    fn recording_watchdog_generation_invalidates_stale_arms() {
        let watchdog = RecordingWatchdog::default();
        // Arm #1.
        let gen1 = watchdog.bump();
        assert_eq!(watchdog.current(), gen1);
        // A later toggle (end, or re-arm) bumps the generation.
        let gen2 = watchdog.bump();
        assert_ne!(gen1, gen2);
        // The stale arm (gen1) must NOT fire; the current arm (gen2) may.
        assert_ne!(watchdog.current(), gen1, "stale watchdog is invalidated");
        assert_eq!(watchdog.current(), gen2, "the newest arm is current");
    }

    /// Plan 039 task 51(d) — the controller `apply` version counter serialises
    /// concurrent rebinds: a dispatch that captured an older version must skip
    /// when a newer `apply` has since bumped the counter, so two racing rebinds
    /// can't interleave a stale unregister/register. Exercises the exact
    /// arithmetic both controllers' dispatch closures run.
    #[test]
    fn rebind_version_counter_supersedes_stale_dispatch() {
        let counter = AtomicU64::new(0);

        // apply #1: bump to 1 and capture v1. It is the only in-flight rebind, so
        // its dispatch is current and must proceed.
        let v1 = counter.fetch_add(1, Ordering::AcqRel) + 1;
        assert!(
            !rebind_superseded(v1, &counter),
            "the only in-flight rebind must not be considered superseded"
        );

        // apply #2 (a concurrent rebind) bumps to 2 and captures v2 before v1's
        // dispatch got to run.
        let v2 = counter.fetch_add(1, Ordering::AcqRel) + 1;
        assert!(
            rebind_superseded(v1, &counter),
            "v1 was superseded by the newer v2 and must skip"
        );
        assert!(
            !rebind_superseded(v2, &counter),
            "v2 is the newest rebind and must proceed"
        );
    }

    /// Plan 039 task 51(d) — the ordering contract both controllers' dispatch
    /// closures execute. Pins the fix for the interleave the version counter
    /// exists to handle: a *superseded* rebind must STILL unregister its old
    /// accel (otherwise a racing Z→A / A→B pair leaves both Z and B live), while
    /// skipping only the register half. Exercises `plan_rebind_steps` directly —
    /// the exact seam the closures loop over.
    #[test]
    fn superseded_rebind_still_unregisters_old_accel_but_skips_register() {
        // The losing rebind of a Z→A / A→B race: it captured old=Z, new=A, and
        // was superseded before its dispatch ran. It MUST unregister Z (so Z is
        // not leaked) and MUST NOT register A (the newer B owns the live slot).
        assert_eq!(
            plan_rebind_steps(Some("Z".into()), Some("A".into()), true),
            vec![RebindStep::Unregister("Z".into())],
            "a superseded rebind must unregister its old accel and skip register"
        );

        // The winning (newest) rebind proceeds fully: unregister old, register new.
        assert_eq!(
            plan_rebind_steps(Some("A".into()), Some("B".into()), false),
            vec![
                RebindStep::Unregister("A".into()),
                RebindStep::Register("B".into())
            ],
            "the current rebind must unregister the old accel then register the new"
        );

        // A disable / modifier-only rebind: unregister old, then clear the
        // transition tracker (no keyed accel is live afterwards).
        assert_eq!(
            plan_rebind_steps(Some("Z".into()), None, false),
            vec![RebindStep::Unregister("Z".into()), RebindStep::ClearHeld],
            "a disable rebind must unregister then clear the held tracker"
        );

        // First-ever bind (nothing previously registered): register only.
        assert_eq!(
            plan_rebind_steps(None, Some("X".into()), false),
            vec![RebindStep::Register("X".into())],
            "a first bind with no prior accel must register only"
        );

        // A superseded first-ever bind is a full no-op: nothing to unregister,
        // and the newer rebind owns registration.
        assert_eq!(
            plan_rebind_steps(None, Some("X".into()), true),
            Vec::<RebindStep>::new(),
            "a superseded bind with no old accel must do nothing"
        );
    }

    /// Plan 039 task 48 — drive `spawn_repaste` end-to-end against a counting
    /// injector and a seeded history store: two back-to-back presses (the second
    /// arriving while the first is in flight) must yield exactly ONE paste, of
    /// the newest history record. This pins the double-press guard at the real
    /// `spawn_repaste` seam (not just the `try_claim_repaste` unit), closing the
    /// task-48 VALIDATE case that needs no live `AppHandle`. Plain `#[test]`: the
    /// paste tasks run on tauri's own default async runtime (lazily initialised
    /// by `spawn`), so this must not touch the process-global runtime handle.
    #[test]
    fn spawn_repaste_double_press_yields_exactly_one_paste() {
        use history_store::{HistoryStore, NewDictationRecord, SERVED_BY_GLADIA_PRIMARY};

        // Injector that records every paste and holds the first one long enough
        // for the second press to arrive while it is in flight.
        struct CountingInjector {
            pasted: Mutex<Vec<String>>,
            hold: Duration,
        }
        #[async_trait::async_trait]
        impl PlatformInjector for CountingInjector {
            async fn paste(&self, text: &str) -> Result<(), MuniError> {
                tokio::time::sleep(self.hold).await;
                self.pasted.lock().expect("poisoned").push(text.to_string());
                Ok(())
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let store = HistoryStore::open(HistoryStore::default_path(dir.path())).expect("open store");
        store
            .insert(NewDictationRecord {
                raw_text: "raw newest".into(),
                cleaned_text: "newest dictation".into(),
                target_app_bundle_id: None,
                served_by: SERVED_BY_GLADIA_PRIMARY.into(),
            })
            .expect("seed history");
        let history = Some(Arc::new(store));

        let injector = Arc::new(CountingInjector {
            pasted: Mutex::new(Vec::new()),
            hold: Duration::from_millis(80),
        });
        let dyn_injector: Arc<dyn PlatformInjector> = injector.clone();
        let in_flight = Arc::new(AtomicBool::new(false));

        // Two rapid presses: the first claims the slot and holds it across the
        // paste; the second must be dropped by the in-flight guard.
        spawn_repaste(
            Arc::clone(&dyn_injector),
            history.clone(),
            Arc::clone(&in_flight),
        );
        spawn_repaste(
            Arc::clone(&dyn_injector),
            history.clone(),
            Arc::clone(&in_flight),
        );

        // Wait (wall clock) past the hold + guard release, then confirm exactly
        // one paste landed.
        std::thread::sleep(Duration::from_millis(300));
        let pasted = injector.pasted.lock().expect("poisoned").clone();
        assert_eq!(
            pasted,
            vec!["newest dictation".to_string()],
            "two overlapping re-paste presses must produce exactly one paste of the newest record"
        );

        // The slot must be free again once the single paste completed, so a later
        // non-overlapping press proceeds.
        assert!(
            !in_flight.load(Ordering::Acquire),
            "the in-flight slot must be released after the paste task ends"
        );
    }

    /// Plan 039 task 39(a)/(d) — drift guard mirroring
    /// `settings_default_matches_injector_default`: the boot-time
    /// `general.launch_at_login` fallback used when the key is absent,
    /// unreadable, or malformed must be the exact same value
    /// `settings::default_for` hands the Settings UI — never a hardcoded
    /// `true`. If the settings-layer default ever changes, this pins the
    /// boot fallback to change with it rather than silently drifting.
    #[test]
    fn launch_at_login_pref_falls_back_to_settings_default() {
        let want = settings::default_for(settings::KEY_GENERAL_LAUNCH_AT_LOGIN)
            .and_then(|v| v.as_bool())
            .expect("KEY_GENERAL_LAUNCH_AT_LOGIN has a bool default");

        // Unset (never stored) → settings default.
        assert_eq!(resolve_launch_at_login_pref(None), want);
        // Malformed stored value → settings default, not a panic.
        assert_eq!(
            resolve_launch_at_login_pref(Some(serde_json::json!("not-a-bool"))),
            want
        );
        // Explicit stored values always round-trip, regardless of default.
        assert!(resolve_launch_at_login_pref(Some(serde_json::json!(true))));
        assert!(!resolve_launch_at_login_pref(Some(serde_json::json!(
            false
        ))));
    }

    /// §13 dev-build guard: paths under any `target/` segment must be
    /// refused for Login Items. Covers the Tauri dev runner output
    /// (`target/debug/Muni-dev.app/...`) and a freshly-built but
    /// not-yet-installed release bundle (`target/release/bundle/...`).
    #[test]
    fn is_dev_bundle_path_rejects_target_subtrees() {
        let dev = PathBuf::from(
            "/Users/x/code/muni/apps/desktop/src-tauri/target/debug/Muni-dev.app/Contents/MacOS/muni",
        );
        assert!(is_dev_bundle_path(&dev));

        let release_uninstalled = PathBuf::from(
            "/Users/x/code/muni/apps/desktop/src-tauri/target/release/bundle/macos/Muni.app/Contents/MacOS/muni",
        );
        assert!(is_dev_bundle_path(&release_uninstalled));

        let raw_debug =
            PathBuf::from("/Users/x/code/muni/apps/desktop/src-tauri/target/debug/muni");
        assert!(is_dev_bundle_path(&raw_debug));
    }

    /// Installed-bundle paths must NOT be flagged as dev — those are
    /// the locations where Launch at Login is supposed to work.
    #[test]
    fn is_dev_bundle_path_accepts_installed_paths() {
        let installed = PathBuf::from("/Applications/Muni.app/Contents/MacOS/muni");
        assert!(!is_dev_bundle_path(&installed));

        let user_apps = PathBuf::from("/Users/x/Applications/Muni.app/Contents/MacOS/muni");
        assert!(!is_dev_bundle_path(&user_apps));

        // A user happens to have a directory called "targeting" — must
        // not collide with the `target` segment match.
        let lookalike = PathBuf::from("/Users/x/targeting/Muni.app/Contents/MacOS/muni");
        assert!(!is_dev_bundle_path(&lookalike));
    }

    // ---- feature 020: LID provider rollback smoke tests ------------------
    //
    // These tests guard the dual-path architecture (audio-LID default;
    // text-LID kept alive as a rollback) against silent rot. They assert
    // the *factory dispatch* — no network calls, no API keys required.
    // If anyone deletes `groq_lid.rs` / `gemini_lid.rs` (or removes
    // a factory arm) without first deleting these tests, `cargo test`
    // fails locally before the regression ships. See plan 020 Task 16
    // for the rationale.

    #[test]
    fn text_lid_groq_provider_builds_classifier_for_rollback() {
        let client = build_text_lid_classifier_for_provider(None, "groq", None).expect(
            "groq factory must construct without an API key (key is resolved at classify time)",
        );
        let label = client.provider_label();
        assert!(
            label.starts_with("groq:"),
            "expected `groq:` provider label, got {label:?}"
        );
    }

    #[test]
    fn text_lid_gemini_provider_builds_classifier_for_rollback() {
        let client = build_text_lid_classifier_for_provider(None, "gemini", None).expect(
            "gemini factory must construct without an API key (key is resolved at classify time)",
        );
        let label = client.provider_label();
        assert!(
            label.starts_with("gemini:"),
            "expected `gemini:` provider label, got {label:?}"
        );
    }

    #[test]
    fn audio_lid_provider_does_not_match_text_lid_dispatch() {
        // The audio-LID slug must NOT accidentally end up wired into
        // the text-LID dispatcher (which would mis-segment usage rows
        // and confuse the rollback rule). Feature 020 default flip
        // also makes the empty provider yield to audio.
        assert!(build_text_lid_classifier_for_provider(
            None,
            AUDIO_LID_PROVIDER_AUDIO_WHISPER_TINY,
            None
        )
        .is_none());
        assert!(build_text_lid_classifier_for_provider(None, "", None).is_none());
        // An unknown slug also yields to audio (not silently falling
        // through to groq), since audio is now the safe default.
        assert!(build_text_lid_classifier_for_provider(None, "soniox", None).is_none());
    }

    // ---- feature 023 (backlog 0040): VAD gate boot wiring -----------------
    //
    // `MUNI_VAD_*` env vars are process-global. Serialize the env-var
    // tests so parallel workers can't observe each other's writes —
    // mirrors `ASR_BACKEND_ENV_LOCK` above.
    static VAD_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn vad_gate_disabled_via_env_yields_no_detector() {
        let _guard = VAD_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(MUNI_VAD_GATE_ENV, "off");
        let detector = build_vad_detector();
        std::env::remove_var(MUNI_VAD_GATE_ENV);
        assert!(
            detector.is_none(),
            "MUNI_VAD_GATE=off must yield None (gate disabled)"
        );
    }

    #[test]
    fn vad_gate_default_enabled_builds_silero_detector() {
        let _guard = VAD_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(MUNI_VAD_GATE_ENV);
        std::env::remove_var(MUNI_VAD_THRESHOLD_ENV);
        std::env::remove_var(MUNI_VAD_MIN_SPEECH_MS_ENV);
        let detector = build_vad_detector();
        assert!(detector.is_some(), "default-on must build the detector");
        let d = detector.unwrap();
        assert_eq!(d.provider_label(), "silero_v5");
    }

    #[test]
    fn vad_threshold_env_clamps_invalid_values_to_default() {
        let _guard = VAD_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(MUNI_VAD_THRESHOLD_ENV, "5.0");
        let t = resolve_vad_threshold();
        std::env::remove_var(MUNI_VAD_THRESHOLD_ENV);
        assert_eq!(t, vad::VAD_DEFAULT_THRESHOLD);

        std::env::set_var(MUNI_VAD_THRESHOLD_ENV, "-0.1");
        let t = resolve_vad_threshold();
        std::env::remove_var(MUNI_VAD_THRESHOLD_ENV);
        assert_eq!(t, vad::VAD_DEFAULT_THRESHOLD);

        std::env::set_var(MUNI_VAD_THRESHOLD_ENV, "notafloat");
        let t = resolve_vad_threshold();
        std::env::remove_var(MUNI_VAD_THRESHOLD_ENV);
        assert_eq!(t, vad::VAD_DEFAULT_THRESHOLD);
    }

    #[test]
    fn vad_threshold_env_accepts_valid_value() {
        let _guard = VAD_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(MUNI_VAD_THRESHOLD_ENV, "0.7");
        let t = resolve_vad_threshold();
        std::env::remove_var(MUNI_VAD_THRESHOLD_ENV);
        assert!((t - 0.7).abs() < 1e-6, "expected 0.7, got {t}");
    }

    #[test]
    fn vad_min_speech_ms_env_rejects_zero() {
        let _guard = VAD_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(MUNI_VAD_MIN_SPEECH_MS_ENV, "0");
        let m = resolve_vad_min_speech_ms();
        std::env::remove_var(MUNI_VAD_MIN_SPEECH_MS_ENV);
        assert_eq!(m, vad::VAD_DEFAULT_MIN_SPEECH_MS);
    }

    // ---- feature 024 (backlog 0042): streaming VAD factory wiring --------

    #[test]
    fn streaming_vad_factory_default_on_post_dogfood() {
        let _guard = VAD_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(MUNI_VAD_STREAM_HYBRID_ENV);
        std::env::remove_var(MUNI_VAD_TRIM_RELEASE_BUFFER_ENV);
        let factory = build_streaming_vad_factory();
        let factory = factory.expect("default-on post-dogfood: both unset → factory must be Some");
        let detector = (factory)();
        assert_eq!(detector.provider_label(), "silero_v5_stream");
    }

    #[test]
    fn streaming_vad_factory_enabled_when_only_hybrid_on_trim_off() {
        let _guard = VAD_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(MUNI_VAD_STREAM_HYBRID_ENV, "on");
        std::env::set_var(MUNI_VAD_TRIM_RELEASE_BUFFER_ENV, "off");
        let factory = build_streaming_vad_factory();
        std::env::remove_var(MUNI_VAD_STREAM_HYBRID_ENV);
        std::env::remove_var(MUNI_VAD_TRIM_RELEASE_BUFFER_ENV);
        let factory = factory.expect("hybrid on (trim off) → factory must be Some");
        let detector = (factory)();
        assert_eq!(detector.provider_label(), "silero_v5_stream");
    }

    #[test]
    fn streaming_vad_factory_enabled_when_only_trim_on_hybrid_off() {
        let _guard = VAD_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(MUNI_VAD_STREAM_HYBRID_ENV, "off");
        std::env::set_var(MUNI_VAD_TRIM_RELEASE_BUFFER_ENV, "on");
        let factory = build_streaming_vad_factory();
        std::env::remove_var(MUNI_VAD_STREAM_HYBRID_ENV);
        std::env::remove_var(MUNI_VAD_TRIM_RELEASE_BUFFER_ENV);
        assert!(
            factory.is_some(),
            "trim-release on (hybrid off) → factory must be Some"
        );
    }

    #[test]
    fn streaming_vad_min_silence_clamps_out_of_range() {
        let _guard = VAD_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(MUNI_VAD_STREAM_MIN_SILENCE_MS_ENV, "10");
        let v = resolve_vad_stream_min_silence_ms();
        std::env::remove_var(MUNI_VAD_STREAM_MIN_SILENCE_MS_ENV);
        assert_eq!(
            v,
            vad::STREAM_DEFAULT_MIN_SILENCE_MS,
            "below sanity floor (50 ms) → falls back to default"
        );
    }

    // ---- feature 025 (backlog 0046): audio-LID silence gate wiring -------

    #[test]
    fn resolve_vad_audio_lid_gate_defaults_on_when_unset() {
        let _guard = VAD_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(MUNI_VAD_AUDIO_LID_GATE_ENV);
        assert!(
            resolve_vad_audio_lid_gate_enabled(),
            "unset env must default to on (post-dogfood 2026-05-21)"
        );
    }

    #[test]
    fn resolve_vad_audio_lid_gate_treats_off_keywords_as_disabled() {
        let _guard = VAD_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for value in ["off", "0", "false", "no", "OFF", "False", "No", " off "] {
            std::env::set_var(MUNI_VAD_AUDIO_LID_GATE_ENV, value);
            let enabled = resolve_vad_audio_lid_gate_enabled();
            std::env::remove_var(MUNI_VAD_AUDIO_LID_GATE_ENV);
            assert!(
                !enabled,
                "value {value:?} must disable the gate (case-insensitive, trimmed)"
            );
        }
    }

    #[test]
    fn resolve_vad_audio_lid_gate_treats_other_values_as_enabled() {
        let _guard = VAD_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        for value in ["on", "1", "true", "yes", "banana", ""] {
            std::env::set_var(MUNI_VAD_AUDIO_LID_GATE_ENV, value);
            let enabled = resolve_vad_audio_lid_gate_enabled();
            std::env::remove_var(MUNI_VAD_AUDIO_LID_GATE_ENV);
            assert!(
                enabled,
                "value {value:?} must NOT disable the gate (default-on polarity)"
            );
        }
    }

    #[test]
    fn build_streaming_vad_factory_returns_some_when_only_audio_lid_gate_on() {
        let _guard = VAD_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(MUNI_VAD_STREAM_HYBRID_ENV, "off");
        std::env::set_var(MUNI_VAD_TRIM_RELEASE_BUFFER_ENV, "off");
        std::env::set_var(MUNI_VAD_AUDIO_LID_GATE_ENV, "on");
        let factory = build_streaming_vad_factory();
        std::env::remove_var(MUNI_VAD_STREAM_HYBRID_ENV);
        std::env::remove_var(MUNI_VAD_TRIM_RELEASE_BUFFER_ENV);
        std::env::remove_var(MUNI_VAD_AUDIO_LID_GATE_ENV);
        let factory = factory.expect("audio-LID gate on (others off) → factory must be Some");
        let detector = (factory)();
        assert_eq!(detector.provider_label(), "silero_v5_stream");
    }

    #[test]
    fn build_streaming_vad_factory_disabled_when_all_three_kill_switches_off() {
        let _guard = VAD_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(MUNI_VAD_STREAM_HYBRID_ENV, "off");
        std::env::set_var(MUNI_VAD_TRIM_RELEASE_BUFFER_ENV, "off");
        std::env::set_var(MUNI_VAD_AUDIO_LID_GATE_ENV, "off");
        let factory = build_streaming_vad_factory();
        std::env::remove_var(MUNI_VAD_STREAM_HYBRID_ENV);
        std::env::remove_var(MUNI_VAD_TRIM_RELEASE_BUFFER_ENV);
        std::env::remove_var(MUNI_VAD_AUDIO_LID_GATE_ENV);
        assert!(
            factory.is_none(),
            "all three kill switches explicit off → factory must be None"
        );
    }
}
