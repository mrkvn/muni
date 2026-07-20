//! Modifier-only hotkey listener (press-and-hold + plan 030 tap-to-toggle).
//!
//! Mirrors Swift v1's `HotkeyManager` (Control + Option held, 150 ms debounce on
//! press, immediate fire on release, cancellation if released during debounce)
//! and adds a sibling **tap-to-toggle** gesture on the same modifiers:
//!
//! - **Quick tap** (modifiers down → up within `MUNI_TOGGLE_TAP_THRESHOLD_MS`,
//!   default 80 ms) → start a locked session ([`HotkeyMode::ToggleLocked`]).
//! - **Medium release** (80 – 150 ms) → cancel (preserves today's
//!   "I changed my mind" dead zone).
//! - **Hold past 150 ms** → press-and-hold ([`HotkeyMode::Ptt`]), unchanged.
//!
//! A locked session ends on:
//! - **Re-tap Ctrl+Option** → `release_tx.send(ReleaseKind::Commit)`.
//! - **Enter** (main or numpad, via the boot-installed KeyDown monitor) →
//!   `CommitAndSubmit` — commits *and* presses Enter in the focused app
//!   after the paste so the text is submitted (e.g. the chat message sends).
//! - **Esc** (scoped `CGEventTap` installed on toggle-start) → `Cancel`.
//! - **60 s of silence** (`MUNI_TOGGLE_MAX_DURATION_S`) → `Commit`. The
//!   silence watchdog lives in the session orchestrator (which owns the
//!   audio stream); the state machine just receives a [`HotkeyMsg::SilenceTimeout`]
//!   when the threshold is crossed and commits the toggle.
//!
//! macOS uses a low-level `CGEventTap` filtered to `FlagsChanged`, attached to
//! the current `CFRunLoop`. This is the same approach Swift v1 used via
//! `NSEvent.addGlobalMonitorForEvents(matching: .flagsChanged)`. Windows is a
//! stub for v2 — see plan §002 R7.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tauri::{AppHandle, Emitter};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tokio::sync::{broadcast, mpsc};

use crate::error::MuniError;
use crate::hotkey_binding::DictationBinding;
use crate::session::{HotkeyMode, ReleaseKind};

/// Live, shared configuration for the dictation flags-tap (Feature 037).
///
/// The single `CGEventTap` installed at boot is **leaked** for the process
/// lifetime and never reinstalled. To let the user rebind the dictation
/// trigger without tearing that tap down, the tap closure reads this shared
/// state through an `Arc` on **every** event: a couple of relaxed-cost atomic
/// loads plus a mask-AND. `set_dictation_hotkey` mutates the atomics in place
/// via [`Self::apply`], so a rebind takes effect on the very next event.
///
/// This is the exact live-reconfig pattern used by `EnglishFastModeFlag`
/// (seeded from the store at boot, mutated inline on a settings change), lifted
/// onto the hot input path where blocking is forbidden.
pub struct HotkeyTriggerState {
    /// OR of the required modifier flag bits. Matched with **subset**
    /// semantics — `(live_flags & required) == required` — so transient bits
    /// (caps-lock, numeric-pad) that may also be latched never block a match
    /// (see learned note 025).
    required_flags: AtomicU64,
    /// Last `both_held` value forwarded to the state machine. `FlagsChanged`
    /// fires constantly while typing; forwarding only on a transition keeps the
    /// state machine seeing one edge per real gesture (the debounce the plan
    /// calls for).
    last_sent: AtomicBool,
    /// Set while the Shortcuts recorder is armed. The dictation trigger goes
    /// silent so pressing the *current* chord to re-record it doesn't also fire
    /// dictation — the webview recorder captures it instead.
    suppressed: AtomicBool,
    /// Set when the active binding carries a key (a modifier+key combo). A
    /// keyed binding is driven by a consuming global shortcut, not the
    /// flags-tap, so the tap must go silent entirely — otherwise both edge
    /// sources would fire. Checked on the hot path as a single relaxed-cost
    /// atomic load; see [`Self::on_flags`].
    key_bound: AtomicBool,
}

/// Resolve the required-modifier mask for a binding. `required_flags_mask`
/// only exists on macOS (it depends on `core-graphics`); off-macOS the tap
/// never runs, so a zero mask is inert.
#[cfg(target_os = "macos")]
fn required_flags_of(binding: &DictationBinding) -> u64 {
    binding.required_flags_mask()
}

#[cfg(not(target_os = "macos"))]
fn required_flags_of(_binding: &DictationBinding) -> u64 {
    0
}

impl HotkeyTriggerState {
    /// Seed the shared state from a stored/decoded binding.
    pub fn from_binding(binding: &DictationBinding) -> Self {
        Self {
            required_flags: AtomicU64::new(required_flags_of(binding)),
            last_sent: AtomicBool::new(false),
            suppressed: AtomicBool::new(false),
            key_bound: AtomicBool::new(binding.key().is_some()),
        }
    }

    /// Suppress (or resume) the dictation trigger while the recorder is armed.
    /// Enabling resets the edge tracker so a chord held *through* recording
    /// can't emit a spurious release edge into the state machine on resume.
    pub fn set_suppressed(&self, suppressed: bool) {
        if suppressed {
            self.last_sent.store(false, Ordering::Release);
        }
        self.suppressed.store(suppressed, Ordering::Release);
    }

    /// Live-apply a rebind. The leaked tap picks up the new mask on its next
    /// event — no tap reinstall. `last_sent` is left untouched: it tracks
    /// transient runtime state, not configuration.
    pub fn apply(&self, binding: &DictationBinding) {
        self.required_flags
            .store(required_flags_of(binding), Ordering::Release);
        self.key_bound
            .store(binding.key().is_some(), Ordering::Release);
    }

    /// The required-flags mask — for observability and tests.
    pub fn snapshot(&self) -> u64 {
        self.required_flags.load(Ordering::Acquire)
    }

    // --- tap-side helper (called from the leaked CFRunLoop closure) --------

    /// For a `FlagsChanged` event, return `Some(both_held)` when the subset
    /// match *transitions*. Returns `None` when nothing should be forwarded —
    /// the value is unchanged, or the recorder has suppressed the trigger.
    fn on_flags(&self, flags_bits: u64) -> Option<bool> {
        if self.suppressed.load(Ordering::Acquire) {
            return None;
        }
        // A keyed binding is driven by a consuming global shortcut instead;
        // the flags-tap must stay silent so exactly one edge source is live.
        if self.key_bound.load(Ordering::Acquire) {
            return None;
        }
        let required = self.required_flags.load(Ordering::Acquire);
        let both_held = (flags_bits & required) == required;
        if self.last_sent.swap(both_held, Ordering::AcqRel) == both_held {
            return None;
        }
        Some(both_held)
    }
}

/// Plan 030 follow-up — controller invoked by the state machine when a
/// toggle session starts (`true`) or ends (`false`).
///
/// Production wiring registers Esc + Enter as global shortcuts via
/// `tauri-plugin-global-shortcut` on `true`, unregisters on `false`.
/// The shortcuts CONSUME the keys while registered — focused apps
/// don't see Esc/Enter during the (typically few-second) session
/// window. Outside a session, normal key behaviour resumes.
///
/// Tests pass [`noop_toggle_shortcut_controller`] so the state-machine
/// logic stays decoupled from Tauri.
pub type ToggleShortcutController = Arc<dyn Fn(bool) + Send + Sync>;

/// No-op controller for tests. Production code in `HotkeyManager::start`
/// substitutes the real `tauri-plugin-global-shortcut`-backed
/// controller.
#[cfg(test)]
pub(crate) fn noop_toggle_shortcut_controller() -> ToggleShortcutController {
    Arc::new(|_| {})
}

/// Default press debounce. Mirrors Swift v1's 150 ms.
pub const DEFAULT_DEBOUNCE_MS: u64 = 150;

/// Plan 030 — env var that overrides the tap-window threshold in
/// milliseconds. Quick releases strictly less than the resolved value
/// are treated as a tap-toggle start; releases between this threshold
/// and [`DEFAULT_DEBOUNCE_MS`] sit in the load-bearing dead zone that
/// preserves today's "I changed my mind" semantics. Invalid, missing,
/// zero, or `>= DEFAULT_DEBOUNCE_MS` overrides fall back to the
/// default.
pub const TOGGLE_TAP_THRESHOLD_ENV: &str = "MUNI_TOGGLE_TAP_THRESHOLD_MS";
/// Plan 030 — default tap window. 80 ms is comfortably below the 150 ms
/// PTT debounce, leaving a ~70 ms dead zone so accidental short
/// presses don't silently start toggle sessions.
pub const DEFAULT_TOGGLE_TAP_THRESHOLD_MS: u64 = 80;

/// Env var that overrides the toggle-session silence threshold in
/// seconds. A toggle session auto-commits after this many seconds of
/// continuous silence (no audible speech). Capped at
/// [`MAX_TOGGLE_DURATION_S`] so a runaway sleeping session can't grow
/// without bound.
pub const TOGGLE_MAX_DURATION_ENV: &str = "MUNI_TOGGLE_MAX_DURATION_S";
/// Default 60 s of silence before auto-commit. A user who walks away
/// mid-session lands on the normal cleanup/paste pipeline instead of
/// an ever-growing audio buffer.
pub const DEFAULT_TOGGLE_MAX_DURATION_S: u64 = 60;
/// Hard upper bound on the env-override. Beyond 10 minutes of silence
/// the audio buffer growth becomes a real memory concern.
const MAX_TOGGLE_DURATION_S: u64 = 600;

/// Plan 030 — env var to disable the tap-to-toggle gesture entirely.
/// Set to `0` to restore exact pre-feature behaviour (any other value,
/// including unset / empty / non-numeric, leaves toggle enabled).
/// In-field rollback knob; not a feature flag we expect to flip
/// long-term.
pub const TOGGLE_ENABLED_ENV: &str = "MUNI_TOGGLE_ENABLED";

/// Capacity for the press/release broadcast channels. Subscribers are the
/// frontend forwarder + (eventually) the session orchestrator + tests; 8 slots
/// is well above the steady-state.
const BROADCAST_CAPACITY: usize = 8;

/// Tauri event name fired on hotkey press (post-debounce).
pub const EVENT_PRESSED: &str = "hotkey://pressed";

/// Tauri event name fired on hotkey release.
pub const EVENT_RELEASED: &str = "hotkey://released";

/// Plan 030 — resolve the tap-window threshold from the env, falling
/// back to [`DEFAULT_TOGGLE_TAP_THRESHOLD_MS`] on anything invalid.
pub fn resolve_toggle_tap_threshold_ms() -> u64 {
    parse_toggle_tap_threshold_ms(std::env::var(TOGGLE_TAP_THRESHOLD_ENV).ok())
}

/// Pure-function counterpart to [`resolve_toggle_tap_threshold_ms`].
/// Rejects values ≥ [`DEFAULT_DEBOUNCE_MS`] because a tap window that
/// large collapses the dead zone and silently changes the meaning of
/// accidental short presses. Falls back to the default on any rejected
/// override.
pub(crate) fn parse_toggle_tap_threshold_ms(raw: Option<String>) -> u64 {
    raw.and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|v| *v > 0 && *v < DEFAULT_DEBOUNCE_MS)
        .unwrap_or(DEFAULT_TOGGLE_TAP_THRESHOLD_MS)
}

/// Plan 030 — resolve the toggle-session hard cap from the env.
pub fn resolve_toggle_max_duration_s() -> u64 {
    parse_toggle_max_duration_s(std::env::var(TOGGLE_MAX_DURATION_ENV).ok())
}

/// Pure-function counterpart to [`resolve_toggle_max_duration_s`].
/// Rejects 0 (would commit instantly), empty, non-numeric, and values
/// above [`MAX_TOGGLE_DURATION_S`].
pub(crate) fn parse_toggle_max_duration_s(raw: Option<String>) -> u64 {
    raw.and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|v| *v > 0 && *v <= MAX_TOGGLE_DURATION_S)
        .unwrap_or(DEFAULT_TOGGLE_MAX_DURATION_S)
}

/// Plan 030 — whether tap-to-toggle is enabled. Returns `true` by
/// default; only the literal `"0"` (after trim) disables.
pub fn resolve_toggle_enabled() -> bool {
    parse_toggle_enabled(std::env::var(TOGGLE_ENABLED_ENV).ok())
}

pub(crate) fn parse_toggle_enabled(raw: Option<String>) -> bool {
    !matches!(raw.as_deref().map(str::trim), Some("0"))
}

/// Inputs to the state machine. Produced by the platform listener and by
/// public API methods on `HotkeyManager`.
#[derive(Debug, Clone, Copy)]
enum HotkeyMsg {
    FlagsChanged {
        both_held: bool,
    },
    /// Plan 030 — fired by the boot-installed Esc `CGEventTap` on every
    /// Esc KeyDown. Ignored when no toggle session is in flight; cancels
    /// the active session otherwise.
    EscPressed,
    /// Plan 030 follow-up — fired by the boot-installed KeyDown tap on
    /// every Enter/Return KeyDown (main *or* numpad). Ignored when no
    /// toggle session is in flight; otherwise **commits and submits** the
    /// active session — like the Ctrl+Option re-tap, but it also presses
    /// Enter in the focused app after the paste so the text is sent
    /// (see [`ReleaseKind::CommitAndSubmit`]).
    EnterPressed,
    /// Fired by the session orchestrator's silence watchdog when the
    /// active toggle session has gone without audible speech for
    /// [`HotkeyConfig::max_toggle_duration_s`] seconds. Ignored when no
    /// toggle session is in flight; commits the active session otherwise.
    SilenceTimeout,
    // Pause/Resume support arrives in Phase 8 (settings) and Phase 6 (tray).
    #[allow(dead_code)]
    SetEnabled(bool),
    Shutdown,
}

/// Typed feed for driving the dictation state machine from a keyed global
/// shortcut (Feature 038). Wraps the module-private [`HotkeyMsg`] channel so
/// `lib.rs`'s `DictationShortcutController` can forward `Pressed`/`Released`
/// edges without the private message type ever leaking out of this module.
///
/// Cheap to clone (it holds an `mpsc` sender), so the controller can capture
/// it into its shortcut handler closure.
#[derive(Clone)]
pub struct DictationEdgeSender(mpsc::UnboundedSender<HotkeyMsg>);

impl DictationEdgeSender {
    /// Forward a dictation edge into the state machine. `both_held` mirrors the
    /// flags-tap contract: `true` on the shortcut's `Pressed` edge, `false` on
    /// `Released`. A closed channel (manager torn down) drops the edge silently.
    pub fn send_edge(&self, both_held: bool) {
        let _ = self.0.send(HotkeyMsg::FlagsChanged { both_held });
    }
}

/// Configuration for the state machine. Resolved once at boot from the
/// env so the resolver is the single source of truth.
#[derive(Debug, Clone, Copy)]
pub struct HotkeyConfig {
    pub debounce_ms: u64,
    pub tap_threshold_ms: u64,
    pub max_toggle_duration_s: u64,
    pub toggle_enabled: bool,
}

impl HotkeyConfig {
    /// Resolve all knobs from the env at boot.
    pub fn from_env() -> Self {
        Self {
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            tap_threshold_ms: resolve_toggle_tap_threshold_ms(),
            max_toggle_duration_s: resolve_toggle_max_duration_s(),
            toggle_enabled: resolve_toggle_enabled(),
        }
    }
}

/// Push-to-talk + tap-to-toggle hotkey listener.
///
/// Construct via [`HotkeyManager::start`], which installs the platform
/// listener, spawns the async state machine, and forwards press/release events
/// onto the Tauri event bus. Drop the value to tear the listener down.
///
/// Subscribers (e.g. the session orchestrator) call
/// [`HotkeyManager::subscribe_press`] / [`HotkeyManager::subscribe_release`]
/// to observe events from Rust without going through the frontend.
pub struct HotkeyManager {
    msg_tx: mpsc::UnboundedSender<HotkeyMsg>,
    // press_tx / release_tx are held so future phases can call
    // `subscribe_press()` / `subscribe_release()` without going through Tauri
    // events. Phase 17's `DictationSession` is the first consumer.
    #[allow(dead_code)]
    press_tx: broadcast::Sender<HotkeyMode>,
    #[allow(dead_code)]
    release_tx: broadcast::Sender<ReleaseKind>,
    /// Silence threshold for the session orchestrator's toggle-session
    /// watchdog. Exposed via [`Self::silence_threshold`] so the
    /// orchestrator and the state machine agree on a single
    /// boot-resolved value.
    silence_threshold: std::time::Duration,
}

impl HotkeyManager {
    /// Install the platform listener and start the state machine.
    ///
    /// On macOS this performs an Accessibility pre-flight check and installs a
    /// `CGEventTap` on the current run loop. If permission is missing or the
    /// tap cannot be installed an error is returned and no resources are
    /// leaked.
    pub fn start(
        app: AppHandle,
        config: HotkeyConfig,
        trigger: Arc<HotkeyTriggerState>,
    ) -> Result<Self, MuniError> {
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        let (press_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (release_tx, _) = broadcast::channel(BROADCAST_CAPACITY);

        // Plan 030 follow-up — toggle-session-scoped global shortcut
        // for Esc (cancel) and Enter (commit). See the
        // `ToggleShortcutController` doc for the trade-off rationale.
        let controller = build_global_shortcut_controller(app.clone(), msg_tx.clone());

        // Platform listener FIRST, then spawn the async tasks — and only on a
        // successful install (plan 039 task 36). The previous order spawned the
        // state machine + both forwarders and THEN `?`-returned on an install
        // failure, leaking all three tasks. That leak fired on every onboarding
        // retry, where the CGEventTap install is *designed* to fail until the
        // user grants Input Monitoring. `install_then` structurally guarantees
        // the tasks are spawned iff the listener installs; any events the tap
        // delivers into `msg_tx` before the state machine starts just buffer in
        // the unbounded channel and drain once it's up. The trigger state is
        // moved into the leaked tap closure so live rebinds land through its
        // atomics with no tap reinstall.
        install_then(
            || install_platform_listener(msg_tx.clone(), trigger),
            || {
                spawn_state_machine(
                    msg_rx,
                    config,
                    press_tx.clone(),
                    release_tx.clone(),
                    controller,
                );
                spawn_press_forwarder(app.clone(), press_tx.subscribe(), EVENT_PRESSED);
                spawn_release_forwarder(app.clone(), release_tx.subscribe(), EVENT_RELEASED);
            },
        )?;

        log::info!(
            target: "hotkey",
            "HotkeyManager started (debounce_ms={} tap_threshold_ms={} max_toggle_duration_s={} toggle_enabled={})",
            config.debounce_ms,
            config.tap_threshold_ms,
            config.max_toggle_duration_s,
            config.toggle_enabled,
        );

        Ok(Self {
            msg_tx,
            press_tx,
            release_tx,
            silence_threshold: std::time::Duration::from_secs(config.max_toggle_duration_s),
        })
    }

    /// Pause / resume hotkey processing without uninstalling the listener.
    ///
    /// When disabling while a press is active, fires a release so downstream
    /// consumers don't end up with a dangling "still pressed" view. First
    /// consumer is the tray Pause/Resume action wired in Phase 6.
    #[allow(dead_code)]
    pub fn set_enabled(&self, enabled: bool) {
        let _ = self.msg_tx.send(HotkeyMsg::SetEnabled(enabled));
    }

    /// Subscribe to press events from Rust. Each subscriber gets its own
    /// queue. First consumer is the session orchestrator added in Phase 17.
    #[allow(dead_code)]
    pub fn subscribe_press(&self) -> broadcast::Receiver<HotkeyMode> {
        self.press_tx.subscribe()
    }

    /// Subscribe to release events from Rust. First consumer is the session
    /// orchestrator added in Phase 17.
    #[allow(dead_code)]
    pub fn subscribe_release(&self) -> broadcast::Receiver<ReleaseKind> {
        self.release_tx.subscribe()
    }

    /// Configured silence threshold for the toggle-session watchdog —
    /// the session orchestrator reads this once at wire-up time so the
    /// state machine and the orchestrator share a single boot-resolved
    /// value (`MUNI_TOGGLE_MAX_DURATION_S` with default 60 s).
    pub fn silence_threshold(&self) -> std::time::Duration {
        self.silence_threshold
    }

    /// Closure that signals the state machine to commit the active
    /// toggle session because the silence watchdog has fired. Safe to
    /// call when no toggle session is in flight — the state machine
    /// no-ops outside an active session. The closure is cheap to
    /// clone (it captures an mpsc sender).
    pub fn silence_timeout_signaler(&self) -> Arc<dyn Fn() + Send + Sync> {
        let tx = self.msg_tx.clone();
        Arc::new(move || {
            let _ = tx.send(HotkeyMsg::SilenceTimeout);
        })
    }

    /// Hand out a typed edge feed for the keyed-shortcut dictation trigger
    /// (Feature 038). The returned [`DictationEdgeSender`] lets `lib.rs`'s
    /// `DictationShortcutController` push `Pressed`/`Released` edges into the
    /// same state machine the flags-tap feeds, while keeping [`HotkeyMsg`]
    /// private to this module.
    ///
    /// Consumed by `lib.rs`'s `DictationShortcutController`, built in
    /// `arm_hotkey_pipeline` where the manager (and thus `msg_tx`) exists.
    pub fn dictation_edge_sender(&self) -> DictationEdgeSender {
        DictationEdgeSender(self.msg_tx.clone())
    }
}

impl Drop for HotkeyManager {
    fn drop(&mut self) {
        let _ = self.msg_tx.send(HotkeyMsg::Shutdown);
    }
}

/// Install the platform listener, then — only on success — run `on_installed`
/// (which spawns the state machine + press/release forwarders). Factored out of
/// [`HotkeyManager::start`] so the "no task leak on install failure" invariant
/// (plan 039 task 36) is unit-testable and structurally enforced: a failing
/// install returns `Err` having run nothing, so zero async tasks are spawned;
/// on success the hook runs exactly once.
fn install_then(
    install: impl FnOnce() -> Result<(), MuniError>,
    on_installed: impl FnOnce(),
) -> Result<(), MuniError> {
    install()?;
    on_installed();
    Ok(())
}

fn spawn_state_machine(
    msg_rx: mpsc::UnboundedReceiver<HotkeyMsg>,
    config: HotkeyConfig,
    press_tx: broadcast::Sender<HotkeyMode>,
    release_tx: broadcast::Sender<ReleaseKind>,
    toggle_shortcut_controller: ToggleShortcutController,
) {
    tauri::async_runtime::spawn(async move {
        run_state_machine(
            msg_rx,
            config,
            press_tx,
            release_tx,
            toggle_shortcut_controller,
        )
        .await;
    });
}

/// Plan 030 follow-up — build the production
/// [`ToggleShortcutController`] backed by `tauri-plugin-global-shortcut`.
///
/// On `register`, attaches handlers for Esc, Enter, and numpad Enter that
/// synthesise `EscPressed` / `EnterPressed` into the state-machine mpsc —
/// registering them also consumes the keypress so Enter doesn't leak into
/// the focused app. On `unregister`, removes all three. All plugin calls
/// hop to the main
/// thread via `app.run_on_main_thread` because the underlying
/// `global_hotkey` crate uses Carbon APIs that are main-thread-only
/// on macOS.
///
/// Errors are logged at WARN — the state machine doesn't observe
/// failures; if registration fails the user simply can't use
/// Esc/Enter for that session, but re-tap and the silence-watchdog
/// timeout still commit normally.
fn build_global_shortcut_controller(
    app: AppHandle,
    msg_tx: mpsc::UnboundedSender<HotkeyMsg>,
) -> ToggleShortcutController {
    Arc::new(move |register: bool| {
        let app = app.clone();
        let msg_tx = msg_tx.clone();
        let app_for_dispatch = app.clone();
        let dispatch = move || {
            let gs = app_for_dispatch.global_shortcut();
            if register {
                let msg_tx_esc = msg_tx.clone();
                if let Err(err) = gs.on_shortcut("Escape", move |_app, _shortcut, event| {
                    if event.state == ShortcutState::Pressed {
                        log::info!(
                            target: "hotkey",
                            "global-shortcut Escape pressed"
                        );
                        let _ = msg_tx_esc.send(HotkeyMsg::EscPressed);
                    }
                }) {
                    log::warn!(
                        target: "hotkey",
                        "global-shortcut register Escape failed: {err}"
                    );
                }
                // Register BOTH the main Return and the numpad Enter so the
                // commit-and-submit gesture fires from either key. Registering
                // them as shortcuts also *consumes* the keypress, so the raw
                // Enter never leaks into the focused app as a stray newline
                // before our paste lands (the NSEvent monitor in
                // `install_keydown_monitor` only observes — it can't consume).
                for enter_accel in ["Enter", "NumpadEnter"] {
                    let msg_tx_enter = msg_tx.clone();
                    if let Err(err) = gs.on_shortcut(enter_accel, move |_app, _shortcut, event| {
                        if event.state == ShortcutState::Pressed {
                            log::info!(
                                target: "hotkey",
                                "global-shortcut {enter_accel} pressed"
                            );
                            let _ = msg_tx_enter.send(HotkeyMsg::EnterPressed);
                        }
                    }) {
                        log::warn!(
                            target: "hotkey",
                            "global-shortcut register {enter_accel} failed: {err}"
                        );
                    }
                }
                log::info!(
                    target: "hotkey",
                    "toggle shortcuts registered (Escape + Enter + NumpadEnter consumed until session ends)"
                );
            } else {
                let _ = gs.unregister("Escape");
                let _ = gs.unregister("Enter");
                let _ = gs.unregister("NumpadEnter");
                log::info!(target: "hotkey", "toggle shortcuts unregistered");
            }
        };
        if let Err(err) = app.run_on_main_thread(dispatch) {
            log::warn!(
                target: "hotkey",
                "run_on_main_thread for toggle-shortcut {} failed: {err}",
                if register { "register" } else { "unregister" }
            );
        }
    })
}

fn spawn_press_forwarder(
    app: AppHandle,
    mut rx: broadcast::Receiver<HotkeyMode>,
    event_name: &'static str,
) {
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                // Forward the press to the frontend dev overlay as a
                // no-payload event — the React layer doesn't care which
                // gesture started the session, and keeping the event
                // shape unit-typed preserves backward compatibility with
                // the existing `useHotkey` hook.
                Ok(_) => {
                    if let Err(err) = app.emit(event_name, ()) {
                        log::warn!(target: "hotkey", "emit {event_name} failed: {err}");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    log::warn!(target: "hotkey", "{event_name} forwarder lagged by {skipped}");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn spawn_release_forwarder(
    app: AppHandle,
    mut rx: broadcast::Receiver<ReleaseKind>,
    event_name: &'static str,
) {
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(_) => {
                    if let Err(err) = app.emit(event_name, ()) {
                        log::warn!(target: "hotkey", "emit {event_name} failed: {err}");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    log::warn!(target: "hotkey", "{event_name} forwarder lagged by {skipped}");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(target_os = "macos")]
fn install_platform_listener(
    sender: mpsc::UnboundedSender<HotkeyMsg>,
    trigger: Arc<HotkeyTriggerState>,
) -> Result<(), MuniError> {
    macos::install(sender, trigger)
}

#[cfg(target_os = "windows")]
fn install_platform_listener(
    _sender: mpsc::UnboundedSender<HotkeyMsg>,
    _trigger: Arc<HotkeyTriggerState>,
) -> Result<(), MuniError> {
    // Windows port is scheduled for a future Muni v2. For now the tap install
    // is a no-op so the rest of the app boots; pressing the hotkey will simply
    // never fire. Phase 1's manual validation target is macOS only.
    log::warn!(target: "hotkey", "Windows hotkey listener is not yet implemented");
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn install_platform_listener(
    _sender: mpsc::UnboundedSender<HotkeyMsg>,
    _trigger: Arc<HotkeyTriggerState>,
) -> Result<(), MuniError> {
    log::warn!(target: "hotkey", "Hotkey listener unsupported on this platform");
    Ok(())
}

/// State-machine state.
///
/// Plan 030 — extends Swift v1's two-flag model with fields that drive
/// the tap-vs-hold discrimination:
///
/// - `tap_window_deadline` — armed alongside `deadline` on modifier
///   down. If the user releases before this fires they get a toggle
///   session; if they release between this and `deadline` they cancel
///   today's "I changed my mind" dead zone; if they hold past
///   `deadline` they get a normal PTT press.
/// - `toggle_active` — true while a locked toggle session owns the
///   press/release contract. Re-tap, Esc, and the silence-watchdog
///   timeout (driven by the session orchestrator and delivered via
///   [`HotkeyMsg::SilenceTimeout`]) all tear this down.
struct State {
    enabled: bool,
    is_active: bool,
    deadline: Option<tokio::time::Instant>,
    /// Plan 030 — armed alongside `deadline` on modifier down. Cleared
    /// once the press resolves into a tap, a cancel, or a PTT press.
    tap_window_deadline: Option<tokio::time::Instant>,
    /// Plan 030 — true while a locked toggle session is in flight.
    /// Mutually exclusive with `is_active` (which tracks PTT presses).
    toggle_active: bool,
    /// Plan 030 — toggle gesture disabled entirely (the
    /// `MUNI_TOGGLE_ENABLED=0` rollback knob). When `false`, the tap
    /// window is never armed and behaviour matches the pre-feature
    /// state machine exactly.
    toggle_enabled: bool,
    /// Plan 030 — cached tap-window duration (avoids re-reading the
    /// config on every flag change).
    tap_threshold: tokio::time::Duration,
    /// Plan 030 follow-up — invoked with `true` on toggle session
    /// start and `false` on end. Production wires this to
    /// `tauri-plugin-global-shortcut` to dynamically register Esc/
    /// Enter only during the locked-session window.
    toggle_shortcut_controller: ToggleShortcutController,
}

impl State {
    fn new(config: HotkeyConfig, toggle_shortcut_controller: ToggleShortcutController) -> Self {
        Self {
            enabled: true,
            is_active: false,
            deadline: None,
            tap_window_deadline: None,
            toggle_active: false,
            toggle_enabled: config.toggle_enabled,
            tap_threshold: tokio::time::Duration::from_millis(config.tap_threshold_ms),
            toggle_shortcut_controller,
        }
    }
}

async fn run_state_machine(
    mut msg_rx: mpsc::UnboundedReceiver<HotkeyMsg>,
    config: HotkeyConfig,
    press_tx: broadcast::Sender<HotkeyMode>,
    release_tx: broadcast::Sender<ReleaseKind>,
    toggle_shortcut_controller: ToggleShortcutController,
) {
    let debounce = tokio::time::Duration::from_millis(config.debounce_ms);
    let mut state = State::new(config, toggle_shortcut_controller);

    loop {
        // Build the select! arms dynamically: PTT debounce and tap
        // window each fire from their respective deadline when armed.
        let msg = next_event(&mut msg_rx, &mut state, &press_tx).await;

        let Some(msg) = msg else { break };

        match msg {
            HotkeyMsg::Shutdown => {
                fire_release_if_active(&mut state, &release_tx, "shutdown");
                fire_commit_if_toggle_active(&mut state, &release_tx, "shutdown");
                break;
            }
            HotkeyMsg::SetEnabled(enabled) => {
                on_set_enabled(&mut state, enabled, &release_tx);
            }
            HotkeyMsg::FlagsChanged { both_held } => {
                on_flags_changed(&mut state, both_held, debounce, &press_tx, &release_tx);
            }
            HotkeyMsg::EscPressed => {
                on_cancel_signal(&mut state, &release_tx, &msg);
            }
            HotkeyMsg::EnterPressed => {
                on_enter_commit(&mut state, &release_tx);
            }
            HotkeyMsg::SilenceTimeout => {
                on_silence_timeout(&mut state, &release_tx);
            }
        }
    }
}

/// Race the next state-machine event against any armed deadlines. The
/// deadline branches drain inline (debounce → press) and continue the
/// loop; the msg branch returns the next inbound message to the main
/// loop.
///
/// Pulled out of `run_state_machine` so the select! body stays flat —
/// the alternative (nested `if state.deadline.is_some()` arms) ballooned
/// once the tap window discrimination entered the picture.
async fn next_event(
    msg_rx: &mut mpsc::UnboundedReceiver<HotkeyMsg>,
    state: &mut State,
    press_tx: &broadcast::Sender<HotkeyMode>,
) -> Option<HotkeyMsg> {
    loop {
        let debounce_deadline = state.deadline;
        let tap_deadline = state.tap_window_deadline;

        tokio::select! {
            biased;

            () = async {
                tokio::time::sleep_until(debounce_deadline.unwrap()).await
            }, if debounce_deadline.is_some() => {
                on_debounce_expired(state, press_tx);
                continue;
            }
            () = async {
                tokio::time::sleep_until(tap_deadline.unwrap()).await
            }, if tap_deadline.is_some() => {
                // Tap window expired without a release — the user is
                // holding, so the PTT debounce path takes over. No
                // event fires here; just clear the tap window.
                state.tap_window_deadline = None;
                continue;
            }
            msg = msg_rx.recv() => return msg,
        }
    }
}

/// Handle a `FlagsChanged` event (mirrors Swift `evaluate(flags:)`).
fn on_flags_changed(
    state: &mut State,
    both_held: bool,
    debounce: tokio::time::Duration,
    press_tx: &broadcast::Sender<HotkeyMode>,
    release_tx: &broadcast::Sender<ReleaseKind>,
) {
    if !state.enabled {
        return;
    }

    // Plan 030 — a locked toggle session intercepts both directions
    // before any of the PTT-shape transitions below. The user is in
    // "session locked" mode; the only meaningful gesture here is a
    // re-tap, which fires on the down-edge of Ctrl+Option.
    if state.toggle_active {
        if both_held {
            // Re-tap. Commit and tear down the toggle.
            log::info!(target: "hotkey", "toggle re-tap committed");
            tear_down_toggle(state);
            let _ = release_tx.send(ReleaseKind::Commit);
        }
        // both_held=false during a locked session = user lifting the
        // modifiers after re-tap (or just brushing them). Ignore.
        return;
    }

    match (both_held, state.is_active, state.deadline.is_some()) {
        (true, false, false) => {
            // Begin hold: schedule the debounced press. Plan 030 — when
            // the toggle gesture is enabled, also arm the shorter tap
            // window. The two race; whichever resolves first decides
            // the gesture.
            let now = tokio::time::Instant::now();
            state.deadline = Some(now + debounce);
            if state.toggle_enabled {
                state.tap_window_deadline = Some(now + state.tap_threshold);
            }
        }
        (true, _, _) => {
            // Modifiers still held while we're either already active or
            // already debouncing — nothing to do.
        }
        (false, true, _) => {
            // Release after a successful PTT press.
            state.is_active = false;
            state.deadline = None;
            state.tap_window_deadline = None;
            log::info!(target: "hotkey", "Hotkey released");
            let _ = release_tx.send(ReleaseKind::Commit);
        }
        (false, false, true) => {
            // Released within the PTT debounce window. Plan 030 —
            // discriminate based on whether the tap window already
            // expired:
            //   - tap_window_deadline still armed (user released
            //     before the tap threshold) → start a toggle session.
            //   - tap_window_deadline already None (user released
            //     in the 80-150 ms dead zone) → preserve today's
            //     "I changed my mind" cancel.
            if state.toggle_enabled && state.tap_window_deadline.is_some() {
                start_toggle_session(state, press_tx);
            }
            state.tap_window_deadline = None;
            state.deadline = None;
        }
        (false, false, false) => {
            // Nothing was pending; nothing to do.
        }
    }
}

/// Plan 030 — promote the in-flight press into a locked toggle session.
///
/// The Esc CGEventTap is installed alongside the PTT FlagsChanged tap
/// at boot (see `install_platform_listener`), leaked for the process
/// lifetime — the state machine gates on `toggle_active` so `EscPressed`
/// is silently dropped outside a toggle session. This sidesteps the
/// Send-bound problem of trying to hold a `CGEventTap` handle inside
/// the state-machine task (Tauri's async_runtime requires `Send`
/// futures; CGEventTap is not `Send`).
fn start_toggle_session(state: &mut State, press_tx: &broadcast::Sender<HotkeyMode>) {
    state.toggle_active = true;
    // Plan 030 follow-up — register Esc/Enter as global shortcuts.
    // The controller hops to the main thread and calls the
    // tauri-plugin-global-shortcut API. Failure is non-fatal — the
    // user can still commit via re-tap or the silence-watchdog timeout.
    (state.toggle_shortcut_controller)(true);
    log::info!(target: "hotkey", "tap-to-toggle session started");
    let _ = press_tx.send(HotkeyMode::ToggleLocked);
}

/// Plan 030 — tear down an active toggle session. Unregisters the
/// Esc/Enter global shortcuts so they behave normally again outside
/// the session.
fn tear_down_toggle(state: &mut State) {
    state.toggle_active = false;
    (state.toggle_shortcut_controller)(false);
}

/// Silence watchdog timeout fired by the session orchestrator. Commit
/// whatever audio we have. Mirrors the user-driven re-tap path so the
/// session lands on the normal cleanup/paste pipeline.
fn on_silence_timeout(state: &mut State, release_tx: &broadcast::Sender<ReleaseKind>) {
    if !state.toggle_active {
        // The watchdog is supposed to be torn down alongside the
        // session, but if a late event races teardown we'd rather
        // no-op than double-fire a commit.
        return;
    }
    log::info!(target: "hotkey", "toggle session committed after silence timeout");
    tear_down_toggle(state);
    let _ = release_tx.send(ReleaseKind::Commit);
}

/// Plan 030 — handle an Esc keypress during a toggle session. Signals
/// the user wants the session discarded.
fn on_cancel_signal(
    state: &mut State,
    release_tx: &broadcast::Sender<ReleaseKind>,
    msg: &HotkeyMsg,
) {
    if !state.toggle_active {
        log::debug!(
            target: "hotkey",
            "cancel signal received with no toggle session — ignoring ({msg:?})"
        );
        return;
    }
    log::info!(target: "hotkey", "toggle session cancelled ({msg:?})");
    tear_down_toggle(state);
    let _ = release_tx.send(ReleaseKind::Cancel);
}

/// Plan 030 follow-up — handle an Enter/Return keypress during a
/// toggle session. Commits the session through the normal cleanup +
/// paste + history pipeline, identical to a Ctrl+Option re-tap. The
/// Enter keypress itself is NOT consumed (the tap is ListenOnly), so
/// the focused app still receives the newline — by design, since the
/// dictation result will paste at the resulting cursor position. The
/// trade-off mirrors today's Esc behaviour with respect to vim
/// Insert-mode etc.
fn on_enter_commit(state: &mut State, release_tx: &broadcast::Sender<ReleaseKind>) {
    if !state.toggle_active {
        log::debug!(
            target: "hotkey",
            "Enter received with no toggle session — ignoring"
        );
        return;
    }
    log::info!(target: "hotkey", "toggle session committed via Enter");
    tear_down_toggle(state);
    // Enter is the "commit *and* submit" gesture: finish the dictation
    // AND press Enter in the focused app after the paste (send the chat
    // message, etc.). The Ctrl+Option re-tap path sends a plain `Commit`.
    let _ = release_tx.send(ReleaseKind::CommitAndSubmit);
}

fn on_debounce_expired(state: &mut State, press_tx: &broadcast::Sender<HotkeyMode>) {
    state.deadline = None;
    // The tap window must have already fired (or never been armed)
    // before the debounce — if it hadn't, the inner select! would have
    // resolved on the shorter deadline first. Clear it defensively.
    state.tap_window_deadline = None;
    if state.is_active {
        return;
    }
    state.is_active = true;
    log::info!(target: "hotkey", "Hotkey pressed");
    let _ = press_tx.send(HotkeyMode::Ptt);
}

fn on_set_enabled(state: &mut State, enabled: bool, release_tx: &broadcast::Sender<ReleaseKind>) {
    state.enabled = enabled;
    if !enabled {
        // Cancel any pending debounce / tap window (no press will land
        // while disabled), and synthesize a release if we were already
        // active so downstream view state stays consistent — mirrors
        // Swift's `setEnabled(false)` guard. Plan 030 — a disabled
        // listener also commits an in-flight toggle session for the
        // same "if we have audio, ship it" reason.
        state.deadline = None;
        state.tap_window_deadline = None;
        fire_release_if_active(state, release_tx, "set_enabled(false)");
        fire_commit_if_toggle_active(state, release_tx, "set_enabled(false)");
    }
}

fn fire_release_if_active(
    state: &mut State,
    release_tx: &broadcast::Sender<ReleaseKind>,
    reason: &'static str,
) {
    if !state.is_active {
        return;
    }
    state.is_active = false;
    state.deadline = None;
    state.tap_window_deadline = None;
    log::info!(target: "hotkey", "Hotkey released ({reason})");
    let _ = release_tx.send(ReleaseKind::Commit);
}

/// Plan 030 — commit any in-flight toggle session on shutdown / disable.
/// Mirrors the PTT "if we have audio, ship it" rule.
fn fire_commit_if_toggle_active(
    state: &mut State,
    release_tx: &broadcast::Sender<ReleaseKind>,
    reason: &'static str,
) {
    if !state.toggle_active {
        return;
    }
    log::info!(target: "hotkey", "toggle session committed ({reason})");
    tear_down_toggle(state);
    let _ = release_tx.send(ReleaseKind::Commit);
}

// -- macOS ------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod macos {
    use std::sync::Arc;

    use super::{HotkeyMsg, HotkeyTriggerState};
    use crate::error::MuniError;
    use core_foundation::{
        base::TCFType,
        runloop::{kCFRunLoopCommonModes, CFRunLoop, CFRunLoopSource},
        string::CFString,
    };
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
        CallbackResult,
    };
    use tokio::sync::mpsc::UnboundedSender;

    /// macOS virtual keycode for the Escape key. Stable Apple-defined
    /// constant; see `HIToolbox/Events.h` / `kVK_Escape`.
    const KV_ESCAPE: i64 = 0x35;

    /// macOS virtual keycode for the Return/Enter key (main keyboard).
    /// `kVK_Return` in `HIToolbox/Events.h`.
    const KV_RETURN: i64 = 0x24;

    /// macOS virtual keycode for the numpad Enter key.
    /// `kVK_ANSI_KeypadEnter` in `HIToolbox/Events.h`. Bound alongside the
    /// main Return so the "press Enter to finish" gesture works regardless
    /// of which Enter the user reaches for — both are unambiguously a
    /// "commit and submit" intent during a locked toggle session, and the
    /// `toggle_active` guard means neither fires when no session is in
    /// flight.
    const KV_NUMPAD_RETURN: i64 = 0x4C;

    /// Map a raw macOS virtual keycode to the [`HotkeyMsg`] it triggers, or
    /// `None` for keys we don't observe. Extracted from the KeyDown monitor
    /// dispatch so the keycode bindings (Esc, both Enter keys) are unit-
    /// testable without a live `NSEvent` stream.
    fn keycode_to_msg(keycode: u16) -> Option<HotkeyMsg> {
        match i64::from(keycode) {
            KV_ESCAPE => Some(HotkeyMsg::EscPressed),
            KV_RETURN | KV_NUMPAD_RETURN => Some(HotkeyMsg::EnterPressed),
            _ => None,
        }
    }

    // `KEYBOARD_EVENT_KEYCODE_FIELD` removed — was needed by the
    // CGEventTap KeyDown path, which we replaced with NSEvent global
    // monitor (see `install_keydown_monitor`). NSEvent exposes
    // `keyCode` directly so we don't need the CG field index.

    pub(super) fn install(
        sender: UnboundedSender<HotkeyMsg>,
        trigger: Arc<HotkeyTriggerState>,
    ) -> Result<(), MuniError> {
        // CGEventTap reading at kCGSessionEventTap requires Input Monitoring
        // on macOS 10.15+. Probe explicitly so we can surface a typed error
        // pointing the user at the right Privacy & Security pane.
        if !is_input_monitoring_granted() {
            log::error!(target: "hotkey", "Input Monitoring permission not granted");
            return Err(MuniError::InputMonitoringDenied);
        }

        // Plan 030 follow-up — KeyDown observation needs a different
        // API than CGEventTap.
        //
        // **Why not CGEventTap for KeyDown.** Initial implementation
        // tried a CGEventTap (both Session and HID locations) with
        // KeyDown in the event mask. Dogfood with verbose diagnostic
        // logging showed FlagsChanged events flowed correctly but
        // KeyDown events were NEVER delivered to the callback —
        // despite Input Monitoring AND Accessibility both granted at
        // both tap locations. macOS appears to apply additional
        // gating to KeyDown delivery via CGEventTap that isn't
        // documented; the workaround standard for dictation apps is
        // `NSEvent.addGlobalMonitorForEventsMatchingMask:handler:`
        // (Cocoa AppKit API), which reliably observes global KeyDown
        // events when Accessibility is granted.
        //
        // We split: FlagsChanged stays on CGEventTap (proven to
        // work), KeyDown moves to NSEvent global monitor. Both leak
        // for the process lifetime; the state machine gates on
        // `toggle_active` so Esc/Enter are silently dropped outside
        // a toggle session.
        let ax_granted = request_accessibility_with_prompt();
        if ax_granted {
            log::info!(
                target: "hotkey",
                "Accessibility granted — Esc cancel + Enter commit available during toggle sessions"
            );
        } else {
            log::warn!(
                target: "hotkey",
                "Accessibility NOT granted — Esc cancel + Enter commit will not work. \
                 Grant in System Settings → Privacy & Security → Accessibility (add Muni and check the box), then restart Muni. \
                 Ctrl+Option re-tap and the silence-watchdog timeout still commit toggle sessions without Accessibility."
            );
        }

        install_flags_tap(sender.clone(), trigger)?;

        // KeyDown observation via NSEvent global monitor. Failure is
        // non-fatal — the user can still commit via re-tap or the
        // silence-watchdog timeout.
        if let Err(err) = install_keydown_monitor(sender) {
            log::warn!(
                target: "hotkey",
                "NSEvent KeyDown monitor install failed (toggle re-tap and silence-watchdog timeout still work): {}",
                err.user_message()
            );
        }

        Ok(())
    }

    /// Plan 030 follow-up — request Accessibility access with the
    /// system prompt. On first call when not granted, macOS shows a
    /// dialog directing the user to System Settings. Returns the
    /// current grant state (which is `false` on a fresh prompt — the
    /// user has to grant and restart for it to flip to `true`).
    ///
    /// Uses `AXIsProcessTrustedWithOptions` from the
    /// `ApplicationServices` framework with the
    /// `kAXTrustedCheckOptionPrompt` option set to `true` so the
    /// prompt fires.
    fn request_accessibility_with_prompt() -> bool {
        use core_foundation::base::TCFType;
        use core_foundation::boolean::CFBoolean;
        use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
        use core_foundation::string::{CFString, CFStringRef};

        #[link(name = "ApplicationServices", kind = "framework")]
        unsafe extern "C" {
            fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
            static kAXTrustedCheckOptionPrompt: CFStringRef;
        }

        // SAFETY: `kAXTrustedCheckOptionPrompt` is a stable
        // Apple-defined CFStringRef constant from ApplicationServices
        // framework; safe to wrap under get-rule (no retain transfer).
        let key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
        let value = CFBoolean::true_value();
        let dict = CFDictionary::from_CFType_pairs(&[(key, value)]);
        // SAFETY: dict is a valid CFDictionaryRef built above. The C
        // function reads it and returns; ownership remains with the
        // Rust dict which drops at end of scope.
        unsafe { AXIsProcessTrustedWithOptions(dict.as_concrete_TypeRef()) }
    }

    /// FlagsChanged `CGEventTap`. This is the load-bearing observation for
    /// press-and-hold + tap-to-toggle start/re-tap — it works reliably under
    /// Input Monitoring at `Session` location on every tested macOS version.
    /// Which modifiers count as the trigger is read from the shared
    /// [`HotkeyTriggerState`] on every event, so rebinding never reinstalls the
    /// tap (which is leaked for the process lifetime).
    fn install_flags_tap(
        sender: UnboundedSender<HotkeyMsg>,
        trigger: Arc<HotkeyTriggerState>,
    ) -> Result<(), MuniError> {
        let tap = CGEventTap::new(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::ListenOnly,
            vec![CGEventType::FlagsChanged],
            move |_proxy, event_type, event| {
                if let CGEventType::FlagsChanged = event_type {
                    if let Some(both_held) = trigger.on_flags(event.get_flags().bits()) {
                        let _ = sender.send(HotkeyMsg::FlagsChanged { both_held });
                    }
                }
                // ListenOnly taps cannot rewrite or drop events — return value
                // is ignored by the system, but we still return `Keep` so the
                // tap is well-formed if the option is ever changed.
                CallbackResult::Keep
            },
        )
        .map_err(|()| {
            log::error!(target: "hotkey", "FlagsChanged CGEventTap::new returned an error");
            MuniError::HotkeyTapInstallFailed
        })?;
        attach_and_leak_tap(tap, "FlagsChanged")
    }

    /// Install BOTH a global and a local `NSEvent` monitor for
    /// KeyDown events. We need both because:
    ///
    /// - `addGlobalMonitorForEventsMatchingMask:handler:` observes
    ///   KeyDown events sent to **other apps** (when Muni is NOT the
    ///   focused app). Standard dictation use case: user has a text
    ///   editor focused, taps Ctrl+Option, presses Esc — Esc goes to
    ///   the text editor, global monitor sees it.
    /// - `addLocalMonitorForEventsMatchingMask:handler:` observes
    ///   KeyDown events sent to **our own app** (when Muni IS the
    ///   focused app, e.g. dev mode with the Tauri window visible, or
    ///   the user has clicked into the Settings/History window).
    ///   Without the local monitor, Esc/Enter pressed while Muni has
    ///   focus would be invisible.
    ///
    /// **Why NSEvent and not CGEventTap.** Dogfood with verbose
    /// per-event logging confirmed that a CGEventTap with KeyDown in
    /// the event mask receives FlagsChanged events but never receives
    /// KeyDown events — at both Session and HID locations, with both
    /// Input Monitoring AND Accessibility granted. macOS appears to
    /// gate KeyDown delivery via CGEventTap separately. The Cocoa
    /// AppKit NSEvent API works under Accessibility alone and is the
    /// standard approach for global keyboard observation in
    /// dictation/automation apps.
    ///
    /// Both monitor tokens are leaked for the process lifetime via
    /// `Retained::into_raw` — same pattern as the FlagsChanged tap.
    ///
    /// Keydowns are not logged at the callback level — they fire too
    /// frequently during normal typing. The state-machine
    /// `on_enter_commit` logs INFO only when Enter actually commits a
    /// toggle session.
    fn install_keydown_monitor(sender: UnboundedSender<HotkeyMsg>) -> Result<(), MuniError> {
        use block2::RcBlock;
        use objc2_app_kit::{NSEvent, NSEventMask};
        use std::ptr::NonNull;

        // Shared dispatch — same logic for both monitor types. Both
        // closures send via the same mpsc sender so the state machine
        // sees one unified event stream regardless of source.
        let dispatch = move |sender: UnboundedSender<HotkeyMsg>, event: NonNull<NSEvent>| {
            // SAFETY: `event` is a valid NSEvent pointer provided by
            // AppKit for the lifetime of this callback invocation.
            let event_ref = unsafe { event.as_ref() };
            let keycode: u16 = event_ref.keyCode();
            if let Some(msg) = keycode_to_msg(keycode) {
                let _ = sender.send(msg);
            }
        };

        // Global monitor — observes events sent to OTHER apps.
        let global_sender = sender.clone();
        let global_handler = RcBlock::new(move |event: NonNull<NSEvent>| {
            dispatch(global_sender.clone(), event);
        });
        let global_monitor = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(
            NSEventMask::KeyDown,
            &global_handler,
        );

        // Local monitor — observes events sent to OUR app. Handler
        // must return the event pointer (or null to consume); we
        // return the event unchanged so normal app handling
        // continues.
        let local_handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            dispatch(sender.clone(), event);
            // Return the event unchanged so AppKit continues normal
            // delivery to the focused responder.
            event.as_ptr()
        });
        // SAFETY: the block returns a valid NSEvent pointer
        // (the input event), satisfying the unsafe-call contract.
        let local_monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                NSEventMask::KeyDown,
                &local_handler,
            )
        };

        let global_ok = global_monitor.is_some();
        let local_ok = local_monitor.is_some();

        // Leak both tokens for the process lifetime. Without the
        // leak, the Retained drops at end of scope and AppKit stops
        // delivering. `Retained::into_raw` is the documented leak
        // primitive.
        if let Some(token) = global_monitor {
            let _leaked = objc2::rc::Retained::into_raw(token);
        }
        if let Some(token) = local_monitor {
            let _leaked = objc2::rc::Retained::into_raw(token);
        }

        log::info!(
            target: "hotkey",
            "NSEvent KeyDown monitors installed (global={global_ok}, local={local_ok})"
        );

        if !global_ok && !local_ok {
            log::error!(
                target: "hotkey",
                "Both NSEvent KeyDown monitors returned nil — Accessibility may not be granted"
            );
            return Err(MuniError::HotkeyTapInstallFailed);
        }

        Ok(())
    }

    /// Shared helper — wrap a `CGEventTap`'s mach port in a
    /// `CFRunLoopSource`, attach it to the current run loop, enable
    /// the tap, and leak it for the process lifetime.
    ///
    /// SAFETY: `tap.mach_port()` borrows for at least the call's
    /// duration; `CFMachPortCreateRunLoopSource` returns a +1 retained
    /// pointer that we immediately wrap in a `CFRunLoopSource` to
    /// manage drop semantics.
    fn attach_and_leak_tap(tap: CGEventTap<'static>, label: &'static str) -> Result<(), MuniError> {
        let source = unsafe {
            let port_ref = tap.mach_port().as_concrete_TypeRef();
            let source_ref = core_foundation_sys::mach_port::CFMachPortCreateRunLoopSource(
                std::ptr::null(),
                port_ref,
                0,
            );
            if source_ref.is_null() {
                return Err(MuniError::HotkeyTapInstallFailed);
            }
            CFRunLoopSource::wrap_under_create_rule(source_ref)
        };

        let mode = unsafe { CFString::wrap_under_get_rule(kCFRunLoopCommonModes) };
        CFRunLoop::get_current().add_source(&source, mode.as_concrete_TypeRef());
        tap.enable();

        // Leak the tap and the source. They must outlive the run loop
        // attachment; we never need to uninstall during a normal app
        // lifetime, and process exit reclaims the underlying mach
        // port.
        std::mem::forget(tap);
        std::mem::forget(source);

        log::info!(target: "hotkey", "macOS {label} CGEventTap installed");
        Ok(())
    }

    /// Requests Input Monitoring (`kIOHIDRequestTypeListenEvent`) — the TCC
    /// bucket macOS uses to gate keyboard event taps since Catalina.
    /// Mirrors Swift v1's input-monitoring check (Swift used `NSEvent`'s global
    /// monitor which is gated on the same permission).
    ///
    /// Uses `IOHIDRequestAccess` rather than `IOHIDCheckAccess` so that the
    /// first ever launch *prompts* the user (and therefore writes a TCC record
    /// keyed off the running bundle's designated requirement). Manual toggles
    /// in System Settings are unreliable on Sonoma+ for processes that have
    /// never requested access — the toggle exists but the grant doesn't fully
    /// activate. Subsequent calls return immediately with the cached decision.
    fn is_input_monitoring_granted() -> bool {
        const REQUEST_TYPE_LISTEN_EVENT: u32 = 1;

        #[link(name = "IOKit", kind = "framework")]
        unsafe extern "C" {
            fn IOHIDRequestAccess(request_type: u32) -> bool;
        }

        // SAFETY: must be called on the main thread per IOKit docs; Tauri's
        // `setup` callback (our only call site) runs on the main thread.
        unsafe { IOHIDRequestAccess(REQUEST_TYPE_LISTEN_EVENT) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Numpad Enter (`0x4C`) must commit the same as main Return —
        /// regression guard for the bug where only `0x24` was bound and
        /// numpad Enter was silently dropped.
        #[test]
        fn numpad_enter_maps_to_enter_pressed() {
            assert!(matches!(
                keycode_to_msg(0x4C),
                Some(HotkeyMsg::EnterPressed)
            ));
        }

        /// Main Return and Escape keep their existing bindings.
        #[test]
        fn main_return_and_escape_map_to_their_messages() {
            assert!(matches!(
                keycode_to_msg(0x24),
                Some(HotkeyMsg::EnterPressed)
            ));
            assert!(matches!(keycode_to_msg(0x35), Some(HotkeyMsg::EscPressed)));
        }

        /// An unbound key (e.g. `a` = `0x00`) produces no message.
        #[test]
        fn unbound_keycode_maps_to_none() {
            assert!(keycode_to_msg(0x00).is_none());
        }
    }
}

// -- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::broadcast::error::TryRecvError;

    #[test]
    fn install_then_runs_hook_once_on_success() {
        // Plan 039 task 36 — a successful install spawns the tasks exactly once.
        let ran = AtomicUsize::new(0);
        let result = install_then(
            || Ok(()),
            || {
                ran.fetch_add(1, Ordering::SeqCst);
            },
        );
        assert!(result.is_ok());
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn install_then_skips_hook_on_failure_no_task_leak() {
        // Plan 039 task 36 — a failed CGEventTap install (the *expected* path
        // during onboarding until Input Monitoring is granted) must NOT run the
        // spawn hook, so it can't leak the state machine + two forwarder tasks
        // it used to before this fix.
        //
        // This pins the `install_then` combinator's contract, which is the
        // STRUCTURAL enforcement: `start()` funnels ALL three spawns
        // (`spawn_state_machine` + both forwarders) into a single `on_installed`
        // closure passed to `install_then`, and the type of `install_then`
        // (`install()?; on_installed()`) makes it impossible to run that closure
        // before the listener installs. KNOWN GAP (reviewed, accepted): a
        // regression that moved the spawns OUT of the closure in `start()` would
        // not be caught here — driving `start()` itself needs an `AppHandle` (the
        // forwarders call `app.emit`), which requires Tauri mock-runtime infra
        // this crate doesn't carry. Keeping the spawns as one closure argument
        // keeps that regression a single obvious edit rather than a subtle one.
        let ran = AtomicUsize::new(0);
        let result = install_then(
            || Err(MuniError::HotkeyTapInstallFailed),
            || {
                ran.fetch_add(1, Ordering::SeqCst);
            },
        );
        assert!(matches!(result, Err(MuniError::HotkeyTapInstallFailed)));
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "no tasks may be spawned when the platform listener fails to install"
        );
    }

    /// Default config used by harness tests — mirrors the production
    /// defaults so test behaviour matches what the user will see.
    fn test_config() -> HotkeyConfig {
        HotkeyConfig {
            debounce_ms: 150,
            tap_threshold_ms: DEFAULT_TOGGLE_TAP_THRESHOLD_MS,
            max_toggle_duration_s: DEFAULT_TOGGLE_MAX_DURATION_S,
            toggle_enabled: true,
        }
    }

    struct Harness {
        msg_tx: mpsc::UnboundedSender<HotkeyMsg>,
        msg_rx: mpsc::UnboundedReceiver<HotkeyMsg>,
        press_tx: broadcast::Sender<HotkeyMode>,
        press_rx: broadcast::Receiver<HotkeyMode>,
        release_tx: broadcast::Sender<ReleaseKind>,
        release_rx: broadcast::Receiver<ReleaseKind>,
    }

    /// Create the channels needed to drive the state machine in isolation.
    fn harness() -> Harness {
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        let (press_tx, press_rx) = broadcast::channel(BROADCAST_CAPACITY);
        let (release_tx, release_rx) = broadcast::channel(BROADCAST_CAPACITY);
        Harness {
            msg_tx,
            msg_rx,
            press_tx,
            press_rx,
            release_tx,
            release_rx,
        }
    }

    /// Drain a press receiver and return every payload observed. Used
    /// by tests that need to assert on the press *mode* in addition to
    /// the count.
    async fn drain_presses(rx: &mut broadcast::Receiver<HotkeyMode>) -> Vec<HotkeyMode> {
        let mut out = Vec::new();
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        loop {
            match rx.try_recv() {
                Ok(mode) => out.push(mode),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
        out
    }

    /// Drain a release receiver and return every payload observed.
    async fn drain_releases(rx: &mut broadcast::Receiver<ReleaseKind>) -> Vec<ReleaseKind> {
        let mut out = Vec::new();
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        loop {
            match rx.try_recv() {
                Ok(kind) => out.push(kind),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
        out
    }

    /// Drive the state machine with `steps` and return both the full
    /// press payload trail and the full release payload trail. The
    /// legacy harness returned counts only; this returns payloads so
    /// plan-030 toggle vs PTT vs cancel distinctions are unambiguous
    /// in test assertions.
    async fn run_with_config(
        config: HotkeyConfig,
        steps: Vec<Step>,
    ) -> (Vec<HotkeyMode>, Vec<ReleaseKind>) {
        let Harness {
            msg_tx,
            msg_rx,
            press_tx,
            mut press_rx,
            release_tx,
            mut release_rx,
        } = harness();

        let handle = tokio::spawn(async move {
            run_state_machine(
                msg_rx,
                config,
                press_tx,
                release_tx,
                noop_toggle_shortcut_controller(),
            )
            .await;
        });

        for step in steps {
            match step {
                Step::Send(msg) => {
                    msg_tx.send(msg).expect("send");
                    tokio::task::yield_now().await;
                }
                Step::Sleep(ms) => {
                    tokio::time::sleep(tokio::time::Duration::from_millis(ms)).await;
                }
            }
        }

        let presses = drain_presses(&mut press_rx).await;
        let releases = drain_releases(&mut release_rx).await;

        drop(msg_tx);
        let _ = handle.await;

        (presses, releases)
    }

    /// Back-compat wrapper for the legacy count-only tests.
    async fn run(debounce_ms: u64, steps: Vec<Step>) -> (usize, usize) {
        let config = HotkeyConfig {
            debounce_ms,
            tap_threshold_ms: DEFAULT_TOGGLE_TAP_THRESHOLD_MS,
            max_toggle_duration_s: DEFAULT_TOGGLE_MAX_DURATION_S,
            toggle_enabled: true,
        };
        let (presses, releases) = run_with_config(config, steps).await;
        (presses.len(), releases.len())
    }

    enum Step {
        Send(HotkeyMsg),
        Sleep(u64),
    }

    fn flags(both_held: bool) -> HotkeyMsg {
        HotkeyMsg::FlagsChanged { both_held }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn hold_past_debounce_fires_press_then_release() {
        let (presses, releases) = run_with_config(
            test_config(),
            vec![
                Step::Send(flags(true)),
                Step::Sleep(160),
                Step::Send(flags(false)),
            ],
        )
        .await;

        assert_eq!(presses, vec![HotkeyMode::Ptt], "PTT press after debounce");
        assert_eq!(
            releases,
            vec![ReleaseKind::Commit],
            "PTT release commits the press"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn release_within_debounce_window_cancels_press() {
        // Plan 030 — release in the dead zone (between tap threshold
        // and debounce) preserves today's silent-cancel semantics. The
        // tap window for this config is 80 ms, debounce is 150 ms; a
        // 100 ms release sits inside the dead zone.
        let (presses, releases) = run_with_config(
            test_config(),
            vec![
                Step::Send(flags(true)),
                Step::Sleep(100),
                Step::Send(flags(false)),
                Step::Sleep(200),
            ],
        )
        .await;

        assert!(
            presses.is_empty(),
            "dead-zone release must NOT fire a press (got {presses:?})"
        );
        assert!(releases.is_empty(), "no press → no release");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn duplicate_held_messages_do_not_double_fire() {
        let (presses, releases) = run(
            150,
            vec![
                Step::Send(flags(true)),
                Step::Send(flags(true)),
                Step::Send(flags(true)),
                Step::Sleep(160),
                Step::Send(flags(true)),
                Step::Send(flags(false)),
            ],
        )
        .await;

        assert_eq!(presses, 1);
        assert_eq!(releases, 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn set_enabled_false_while_active_synthesizes_release() {
        let (presses, releases) = run(
            150,
            vec![
                Step::Send(flags(true)),
                Step::Sleep(160),
                Step::Send(HotkeyMsg::SetEnabled(false)),
            ],
        )
        .await;

        assert_eq!(presses, 1);
        assert_eq!(releases, 1, "disabling while active must fire release");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn set_enabled_false_during_debounce_cancels_press() {
        let (presses, releases) = run(
            150,
            vec![
                Step::Send(flags(true)),
                Step::Sleep(50),
                Step::Send(HotkeyMsg::SetEnabled(false)),
                Step::Sleep(200),
                Step::Send(flags(false)),
            ],
        )
        .await;

        assert_eq!(presses, 0);
        assert_eq!(releases, 0);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn flags_changed_ignored_while_disabled() {
        let (presses, releases) = run(
            150,
            vec![
                Step::Send(HotkeyMsg::SetEnabled(false)),
                Step::Send(flags(true)),
                Step::Sleep(200),
                Step::Send(flags(false)),
            ],
        )
        .await;

        assert_eq!(presses, 0);
        assert_eq!(releases, 0);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn re_enable_resumes_normal_operation() {
        let (presses, releases) = run(
            150,
            vec![
                Step::Send(HotkeyMsg::SetEnabled(false)),
                Step::Send(HotkeyMsg::SetEnabled(true)),
                Step::Send(flags(true)),
                Step::Sleep(160),
                Step::Send(flags(false)),
            ],
        )
        .await;

        assert_eq!(presses, 1);
        assert_eq!(releases, 1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn shutdown_message_breaks_loop_cleanly() {
        let Harness {
            msg_tx,
            msg_rx,
            press_tx,
            press_rx: _press_rx,
            release_tx,
            release_rx: _release_rx,
        } = harness();
        let handle = tokio::spawn(run_state_machine(
            msg_rx,
            test_config(),
            press_tx,
            release_tx,
            noop_toggle_shortcut_controller(),
        ));

        msg_tx.send(HotkeyMsg::Shutdown).unwrap();
        // Sender drop is not required; Shutdown must terminate on its own.
        handle.await.unwrap();
    }

    /// Backlog 0003 — hypothesis #1: bursty modifier flag-change events
    /// must never desync the state machine's press/release accounting.
    /// Three failed taps (each cancelled within the debounce window)
    /// followed by a real held press must produce exactly one
    /// press/release pair.
    ///
    /// Plan 030 — the "cancelled within debounce" presses below all
    /// release at 40 ms, which is inside the 80 ms tap window. With
    /// toggle enabled they would each start a toggle session. We
    /// instantiate the harness with `toggle_enabled: false` so this
    /// test still pins the original dead-zone semantics; a parallel
    /// case below exercises the toggle-enabled rapid-tap behaviour.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn rapid_taps_then_real_hold_emits_one_balanced_pair() {
        let mut config = test_config();
        config.toggle_enabled = false;
        let (presses, releases) = run_with_config(
            config,
            vec![
                Step::Send(flags(true)),
                Step::Sleep(40),
                Step::Send(flags(false)),
                Step::Send(flags(true)),
                Step::Sleep(40),
                Step::Send(flags(false)),
                Step::Send(flags(true)),
                Step::Sleep(40),
                Step::Send(flags(false)),
                Step::Send(flags(true)),
                Step::Sleep(200),
                Step::Send(flags(false)),
            ],
        )
        .await;

        assert_eq!(presses.len(), 1, "exactly one PTT press from the held tap");
        assert_eq!(
            releases,
            vec![ReleaseKind::Commit],
            "exactly one release should pair with that press"
        );
    }

    /// Backlog 0003 — repeated balanced press/release cycles must never
    /// drift. Three full cycles must emit exactly three press/release
    /// pairs, no more and no fewer.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn back_to_back_cycles_emit_balanced_pairs() {
        let mut steps = Vec::new();
        for _ in 0..3 {
            steps.push(Step::Send(flags(true)));
            steps.push(Step::Sleep(160));
            steps.push(Step::Send(flags(false)));
            steps.push(Step::Sleep(20));
        }
        let (presses, releases) = run(150, steps).await;
        assert_eq!(presses, 3);
        assert_eq!(releases, 3);
    }

    /// Backlog 0003 — many duplicate `(true)` messages while held (which
    /// macOS emits when other modifiers cycle on top of Control+Option)
    /// must not synthesize a second press, and the eventual release must
    /// still produce exactly one release event.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn dup_held_burst_during_active_press_does_not_inflate_releases() {
        let mut steps = vec![Step::Send(flags(true)), Step::Sleep(160)];
        for _ in 0..20 {
            steps.push(Step::Send(flags(true)));
        }
        steps.push(Step::Sleep(20));
        steps.push(Step::Send(flags(false)));

        let (presses, releases) = run(150, steps).await;
        assert_eq!(presses, 1);
        assert_eq!(releases, 1);
    }

    /// Subscribers added via the public API receive press/release events.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn subscribe_press_and_release_receive_events() {
        let Harness {
            msg_tx,
            msg_rx,
            press_tx,
            mut press_rx,
            release_tx,
            mut release_rx,
        } = harness();
        // A second subscriber proves the broadcast channel fans out.
        let mut press_rx_2 = press_tx.subscribe();

        let handle = tokio::spawn(run_state_machine(
            msg_rx,
            test_config(),
            press_tx,
            release_tx,
            noop_toggle_shortcut_controller(),
        ));

        msg_tx.send(flags(true)).unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(160)).await;
        msg_tx.send(flags(false)).unwrap();

        assert_eq!(drain_presses(&mut press_rx).await.len(), 1);
        assert_eq!(drain_presses(&mut press_rx_2).await.len(), 1);
        assert_eq!(drain_releases(&mut release_rx).await.len(), 1);

        drop(msg_tx);
        let _ = handle.await;
    }

    // -- Plan 030: tap-to-toggle gesture tests -----------------------------

    /// Quick tap (release under the tap threshold) starts a locked
    /// toggle session — press fires with `HotkeyMode::ToggleLocked` and
    /// no release fires yet.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn quick_tap_under_threshold_fires_toggle_press() {
        let (presses, releases) = run_with_config(
            test_config(),
            vec![
                Step::Send(flags(true)),
                Step::Sleep(50),
                Step::Send(flags(false)),
                Step::Sleep(20),
            ],
        )
        .await;

        assert_eq!(
            presses,
            vec![HotkeyMode::ToggleLocked],
            "quick tap must fire a locked-mode press"
        );
        assert!(
            releases.is_empty(),
            "toggle session is still in flight (got {releases:?})"
        );
    }

    /// Tap → re-tap commits the session via a normal Commit release.
    /// The trailing flags(false) is just the user lifting modifiers
    /// after the re-tap and should not emit anything.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn tap_then_retap_fires_commit_release() {
        let (presses, releases) = run_with_config(
            test_config(),
            vec![
                Step::Send(flags(true)),
                Step::Sleep(50),
                Step::Send(flags(false)),
                Step::Sleep(20),
                Step::Send(flags(true)),
                Step::Sleep(5),
                Step::Send(flags(false)),
            ],
        )
        .await;

        assert_eq!(presses, vec![HotkeyMode::ToggleLocked]);
        assert_eq!(
            releases,
            vec![ReleaseKind::Commit],
            "re-tap commits the session"
        );
    }

    /// Esc during a locked session cancels — release fires with
    /// `ReleaseKind::Cancel`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn tap_then_esc_fires_cancel_release() {
        let (presses, releases) = run_with_config(
            test_config(),
            vec![
                Step::Send(flags(true)),
                Step::Sleep(50),
                Step::Send(flags(false)),
                Step::Send(HotkeyMsg::EscPressed),
            ],
        )
        .await;

        assert_eq!(presses, vec![HotkeyMode::ToggleLocked]);
        assert_eq!(
            releases,
            vec![ReleaseKind::Cancel],
            "Esc cancels the toggle session"
        );
    }

    /// Plan 030 follow-up — Enter during a locked session commits the
    /// session through the normal release path, but as `CommitAndSubmit`
    /// (not `Commit`, not `Cancel`) so the session layer also presses
    /// Enter in the focused app after the paste.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn tap_then_enter_fires_commit_and_submit_release() {
        let (presses, releases) = run_with_config(
            test_config(),
            vec![
                Step::Send(flags(true)),
                Step::Sleep(50),
                Step::Send(flags(false)),
                Step::Send(HotkeyMsg::EnterPressed),
            ],
        )
        .await;

        assert_eq!(presses, vec![HotkeyMode::ToggleLocked]);
        assert_eq!(
            releases,
            vec![ReleaseKind::CommitAndSubmit],
            "Enter commits the toggle session and requests submit"
        );
    }

    /// Plan 030 follow-up — Enter arriving when no toggle session is
    /// active must NOT fire anything. Crucial because the boot-
    /// installed KeyDown tap sees every Enter keypress system-wide —
    /// the `toggle_active` guard is the only thing preventing every
    /// stray newline from synthesising a press/release.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn enter_with_no_toggle_session_is_noop() {
        let (presses, releases) = run_with_config(
            test_config(),
            vec![
                Step::Send(HotkeyMsg::EnterPressed),
                Step::Send(HotkeyMsg::EnterPressed),
                Step::Send(HotkeyMsg::EnterPressed),
                Step::Sleep(10),
            ],
        )
        .await;

        assert!(presses.is_empty());
        assert!(releases.is_empty());
    }

    /// Plan 030 follow-up — Enter during an active PTT hold (not a
    /// toggle session) must NOT fire a commit. The PTT release is
    /// what ends a press-and-hold session; Enter is toggle-only.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn enter_during_ptt_hold_is_noop() {
        let (presses, releases) = run_with_config(
            test_config(),
            vec![
                // Hold past debounce to enter PTT.
                Step::Send(flags(true)),
                Step::Sleep(160),
                // Now press Enter while still holding.
                Step::Send(HotkeyMsg::EnterPressed),
                Step::Sleep(10),
                // Release modifiers to actually end the press.
                Step::Send(flags(false)),
            ],
        )
        .await;

        assert_eq!(
            presses,
            vec![HotkeyMode::Ptt],
            "exactly one PTT press, Enter must not synthesise a second"
        );
        assert_eq!(
            releases,
            vec![ReleaseKind::Commit],
            "exactly one release from the modifier lift; Enter must not double-fire"
        );
    }

    /// Silence-watchdog timeout from the session orchestrator commits
    /// the active toggle session via the [`HotkeyMsg::SilenceTimeout`]
    /// path. The state machine handles teardown so the next press
    /// starts clean.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn silence_timeout_during_toggle_fires_commit_release() {
        let (presses, releases) = run_with_config(
            test_config(),
            vec![
                Step::Send(flags(true)),
                Step::Sleep(50),
                Step::Send(flags(false)),
                Step::Sleep(20),
                Step::Send(HotkeyMsg::SilenceTimeout),
            ],
        )
        .await;

        assert_eq!(presses, vec![HotkeyMode::ToggleLocked]);
        assert_eq!(
            releases,
            vec![ReleaseKind::Commit],
            "silence-watchdog timeout commits the session"
        );
    }

    /// Defensive: a stray [`HotkeyMsg::SilenceTimeout`] outside an
    /// active toggle session is a no-op (no press, no release). Guards
    /// against a watchdog that fires late after teardown racing.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn silence_timeout_outside_toggle_is_noop() {
        let (presses, releases) = run_with_config(
            test_config(),
            vec![Step::Send(HotkeyMsg::SilenceTimeout), Step::Sleep(20)],
        )
        .await;

        assert!(
            presses.is_empty(),
            "silence timeout outside a toggle must not synthesise a press (got {presses:?})"
        );
        assert!(
            releases.is_empty(),
            "silence timeout outside a toggle must not fire a release (got {releases:?})"
        );
    }

    /// Medium release (between tap threshold and PTT debounce) is the
    /// load-bearing dead zone: it preserves today's silent-cancel
    /// semantics. Pinning this test guards against silently breaking
    /// the "I changed my mind" gesture.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn medium_release_between_thresholds_still_cancels() {
        let (presses, releases) = run_with_config(
            test_config(),
            vec![
                Step::Send(flags(true)),
                // 100 ms > 80 ms (tap) but < 150 ms (debounce)
                Step::Sleep(100),
                Step::Send(flags(false)),
                Step::Sleep(200),
            ],
        )
        .await;

        assert!(
            presses.is_empty(),
            "dead-zone release must not fire any press (got {presses:?})"
        );
        assert!(releases.is_empty(), "no press → no release");
    }

    /// Hold past the PTT debounce continues to fire a normal PTT press
    /// even with the toggle gesture enabled. The load-bearing existing
    /// path must not regress.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn hold_past_debounce_still_fires_ptt() {
        let (presses, releases) = run_with_config(
            test_config(),
            vec![
                Step::Send(flags(true)),
                Step::Sleep(160),
                Step::Send(flags(false)),
            ],
        )
        .await;

        assert_eq!(presses, vec![HotkeyMode::Ptt]);
        assert_eq!(releases, vec![ReleaseKind::Commit]);
    }

    /// During an active toggle session, a long hold of the modifiers
    /// does NOT start a second PTT press — the session is still locked
    /// until re-tap / Esc / timeout. The first held = true is the
    /// re-tap that commits; the trailing release just lifts the
    /// modifiers.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn tap_then_hold_during_active_toggle_commits_via_retap_edge() {
        let (presses, releases) = run_with_config(
            test_config(),
            vec![
                Step::Send(flags(true)),
                Step::Sleep(50),
                Step::Send(flags(false)),
                Step::Sleep(20),
                // Modifiers held again — this is the re-tap (down edge).
                Step::Send(flags(true)),
                // Hold for a long time, then release. Should NOT fire a
                // PTT press because the toggle owns the gesture; the
                // release should also be a no-op (the commit already
                // fired on the down edge above).
                Step::Sleep(500),
                Step::Send(flags(false)),
            ],
        )
        .await;

        assert_eq!(presses, vec![HotkeyMode::ToggleLocked]);
        assert_eq!(releases, vec![ReleaseKind::Commit]);
    }

    /// SetEnabled(false) during an active toggle session commits the
    /// session — preserves the "if we have audio, ship it" invariant.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn set_enabled_false_during_toggle_commits() {
        let (presses, releases) = run_with_config(
            test_config(),
            vec![
                Step::Send(flags(true)),
                Step::Sleep(50),
                Step::Send(flags(false)),
                Step::Send(HotkeyMsg::SetEnabled(false)),
            ],
        )
        .await;

        assert_eq!(presses, vec![HotkeyMode::ToggleLocked]);
        assert_eq!(releases, vec![ReleaseKind::Commit]);
    }

    /// Shutdown during a toggle session also commits (same "ship it"
    /// rationale as set_enabled).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn shutdown_during_toggle_commits() {
        let Harness {
            msg_tx,
            msg_rx,
            press_tx,
            mut press_rx,
            release_tx,
            mut release_rx,
        } = harness();

        let handle = tokio::spawn(run_state_machine(
            msg_rx,
            test_config(),
            press_tx,
            release_tx,
            noop_toggle_shortcut_controller(),
        ));

        msg_tx.send(flags(true)).unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        msg_tx.send(flags(false)).unwrap();
        tokio::task::yield_now().await;
        msg_tx.send(HotkeyMsg::Shutdown).unwrap();

        handle.await.unwrap();

        let presses = drain_presses(&mut press_rx).await;
        let releases = drain_releases(&mut release_rx).await;
        assert_eq!(presses, vec![HotkeyMode::ToggleLocked]);
        assert_eq!(releases, vec![ReleaseKind::Commit]);
    }

    /// `MUNI_TOGGLE_ENABLED=0` restores exact pre-feature behaviour: a
    /// quick tap is ignored just like today's "I changed my mind"
    /// cancel.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn toggle_disabled_by_env_falls_back_to_ptt_only() {
        let mut config = test_config();
        config.toggle_enabled = false;
        let (presses, releases) = run_with_config(
            config,
            vec![
                Step::Send(flags(true)),
                Step::Sleep(50),
                Step::Send(flags(false)),
                Step::Sleep(200),
            ],
        )
        .await;

        assert!(presses.is_empty());
        assert!(releases.is_empty());
    }

    /// Esc arriving when no toggle session is in flight must NOT fire
    /// anything. Guards against stray Esc keypresses outside a session.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancel_signal_with_no_toggle_session_is_noop() {
        let (presses, releases) = run_with_config(
            test_config(),
            vec![Step::Send(HotkeyMsg::EscPressed), Step::Sleep(10)],
        )
        .await;

        assert!(presses.is_empty());
        assert!(releases.is_empty());
    }

    // -- Plan 030: env-var resolver tests ----------------------------------

    #[test]
    fn parse_toggle_tap_threshold_ms_accepts_positive_values_below_debounce() {
        assert_eq!(parse_toggle_tap_threshold_ms(Some("50".into())), 50);
        assert_eq!(parse_toggle_tap_threshold_ms(Some(" 120 ".into())), 120);
    }

    #[test]
    fn parse_toggle_tap_threshold_ms_rejects_values_at_or_above_debounce() {
        // A tap threshold ≥ debounce collapses the dead zone — must
        // fall back to default.
        assert_eq!(
            parse_toggle_tap_threshold_ms(Some(DEFAULT_DEBOUNCE_MS.to_string())),
            DEFAULT_TOGGLE_TAP_THRESHOLD_MS,
            "threshold == debounce must fall back"
        );
        assert_eq!(
            parse_toggle_tap_threshold_ms(Some("200".into())),
            DEFAULT_TOGGLE_TAP_THRESHOLD_MS,
            "threshold > debounce must fall back"
        );
    }

    #[test]
    fn parse_toggle_tap_threshold_ms_rejects_zero_and_invalid() {
        assert_eq!(
            parse_toggle_tap_threshold_ms(Some("0".into())),
            DEFAULT_TOGGLE_TAP_THRESHOLD_MS
        );
        assert_eq!(
            parse_toggle_tap_threshold_ms(Some("".into())),
            DEFAULT_TOGGLE_TAP_THRESHOLD_MS
        );
        assert_eq!(
            parse_toggle_tap_threshold_ms(Some("abc".into())),
            DEFAULT_TOGGLE_TAP_THRESHOLD_MS
        );
        assert_eq!(
            parse_toggle_tap_threshold_ms(None),
            DEFAULT_TOGGLE_TAP_THRESHOLD_MS
        );
    }

    #[test]
    fn parse_toggle_max_duration_s_rejects_zero_and_excessive() {
        assert_eq!(
            parse_toggle_max_duration_s(Some("0".into())),
            DEFAULT_TOGGLE_MAX_DURATION_S
        );
        assert_eq!(
            parse_toggle_max_duration_s(Some("".into())),
            DEFAULT_TOGGLE_MAX_DURATION_S
        );
        assert_eq!(
            parse_toggle_max_duration_s(Some("abc".into())),
            DEFAULT_TOGGLE_MAX_DURATION_S
        );
        assert_eq!(
            parse_toggle_max_duration_s(Some("99999".into())),
            DEFAULT_TOGGLE_MAX_DURATION_S
        );
        assert_eq!(
            parse_toggle_max_duration_s(None),
            DEFAULT_TOGGLE_MAX_DURATION_S
        );
    }

    #[test]
    fn parse_toggle_max_duration_s_accepts_positive_within_cap() {
        assert_eq!(parse_toggle_max_duration_s(Some("30".into())), 30);
        assert_eq!(
            parse_toggle_max_duration_s(Some(MAX_TOGGLE_DURATION_S.to_string())),
            MAX_TOGGLE_DURATION_S
        );
    }

    #[test]
    fn parse_toggle_enabled_only_zero_disables() {
        assert!(!parse_toggle_enabled(Some("0".into())));
        assert!(!parse_toggle_enabled(Some(" 0 ".into())));
        assert!(parse_toggle_enabled(Some("1".into())));
        assert!(parse_toggle_enabled(Some("true".into())));
        assert!(parse_toggle_enabled(Some("".into())));
        assert!(parse_toggle_enabled(None));
    }

    // -- Feature 037: shared live trigger state ----------------------------

    use crate::hotkey_binding::HotkeyModifier;

    fn modifiers(mods: Vec<HotkeyModifier>) -> DictationBinding {
        DictationBinding::Modifiers {
            mods,
            key: None,
            display_key: None,
        }
    }

    /// Feature 038 — a keyed (modifier+key) binding. Drives the flags-tap
    /// key-gate: whenever `key` is present the tap must stay silent.
    fn keyed(mods: Vec<HotkeyModifier>, key: &str) -> DictationBinding {
        DictationBinding::Modifiers {
            mods,
            key: Some(key.to_string()),
            display_key: None,
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn snapshot_and_apply_reflect_the_modifier_mask() {
        use core_graphics::event::CGEventFlags;
        let ctrl_opt =
            (CGEventFlags::CGEventFlagControl | CGEventFlags::CGEventFlagAlternate).bits();
        let cmd_shift = (CGEventFlags::CGEventFlagCommand | CGEventFlags::CGEventFlagShift).bits();

        let trigger = HotkeyTriggerState::from_binding(&modifiers(vec![
            HotkeyModifier::Control,
            HotkeyModifier::Option,
        ]));
        assert_eq!(trigger.snapshot(), ctrl_opt);

        // Live rebind — no tap reinstall, just the atomic.
        trigger.apply(&modifiers(vec![
            HotkeyModifier::Command,
            HotkeyModifier::Shift,
        ]));
        assert_eq!(trigger.snapshot(), cmd_shift);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn on_flags_debounces_and_uses_subset_match() {
        use core_graphics::event::CGEventFlags;

        let trigger = HotkeyTriggerState::from_binding(&modifiers(vec![
            HotkeyModifier::Control,
            HotkeyModifier::Option,
        ]));
        let required =
            (CGEventFlags::CGEventFlagControl | CGEventFlags::CGEventFlagAlternate).bits();

        // First matching edge forwards `true`; a duplicate is swallowed.
        assert_eq!(trigger.on_flags(required), Some(true));
        assert_eq!(trigger.on_flags(required), None);

        // Transient bits (numeric pad) latched alongside still match (subset).
        let with_transient = required | CGEventFlags::CGEventFlagNumericPad.bits();
        assert_eq!(trigger.on_flags(with_transient), None); // still held, no edge

        // Dropping a required modifier forwards the falling edge exactly once.
        assert_eq!(
            trigger.on_flags(CGEventFlags::CGEventFlagControl.bits()),
            Some(false)
        );
        assert_eq!(
            trigger.on_flags(CGEventFlags::CGEventFlagControl.bits()),
            None
        );
    }

    #[test]
    fn suppressed_trigger_does_not_fire_and_resumes() {
        use core_graphics::event::CGEventFlags;

        let trigger = HotkeyTriggerState::from_binding(&modifiers(vec![
            HotkeyModifier::Control,
            HotkeyModifier::Option,
        ]));
        let required =
            (CGEventFlags::CGEventFlagControl | CGEventFlags::CGEventFlagAlternate).bits();

        // While the recorder is armed, holding the current chord must NOT fire
        // dictation — the webview captures it instead.
        trigger.set_suppressed(true);
        assert_eq!(trigger.on_flags(required), None);
        assert_eq!(trigger.on_flags(0), None); // release during recording: still inert

        // On resume, the very next real press fires cleanly (no spurious edge
        // from the chord that was held through recording).
        trigger.set_suppressed(false);
        assert_eq!(trigger.on_flags(required), Some(true));
    }

    // -- Feature 038: keyed-binding flags-tap gate + edge sender ------------

    /// A keyed binding silences the flags-tap entirely: `on_flags` returns
    /// `None` regardless of the flag bits, because the consuming global
    /// shortcut is the sole edge source for a keyed trigger.
    #[cfg(target_os = "macos")]
    #[test]
    fn keyed_binding_gates_off_the_flags_tap() {
        use core_graphics::event::CGEventFlags;

        let trigger = HotkeyTriggerState::from_binding(&keyed(
            vec![HotkeyModifier::Control, HotkeyModifier::Shift],
            "KeyR",
        ));
        let ctrl_shift = (CGEventFlags::CGEventFlagControl | CGEventFlags::CGEventFlagShift).bits();

        // Neither the matching chord nor a bare release produces an edge —
        // the tap is inert while a key is bound.
        assert_eq!(trigger.on_flags(ctrl_shift), None);
        assert_eq!(trigger.on_flags(ctrl_shift), None);
        assert_eq!(trigger.on_flags(0), None);
    }

    /// `apply` flips the gate live: switching to a keyed binding silences the
    /// tap, and switching back to a modifier-only binding re-enables it.
    #[cfg(target_os = "macos")]
    #[test]
    fn apply_toggles_the_key_gate_live() {
        use core_graphics::event::CGEventFlags;

        let trigger = HotkeyTriggerState::from_binding(&modifiers(vec![
            HotkeyModifier::Control,
            HotkeyModifier::Option,
        ]));
        let ctrl_opt =
            (CGEventFlags::CGEventFlagControl | CGEventFlags::CGEventFlagAlternate).bits();

        // Modifier-only: the tap fires on the matching edge.
        assert_eq!(trigger.on_flags(ctrl_opt), Some(true));

        // Rebind to a keyed combo — the tap goes silent even for its own
        // required mask (the key-gate short-circuits before the mask check).
        trigger.apply(&keyed(
            vec![HotkeyModifier::Control, HotkeyModifier::Shift],
            "KeyR",
        ));
        let ctrl_shift = (CGEventFlags::CGEventFlagControl | CGEventFlags::CGEventFlagShift).bits();
        assert_eq!(trigger.on_flags(ctrl_shift), None);
        assert_eq!(trigger.on_flags(ctrl_opt), None);

        // Rebind back to modifier-only — the tap fires again. `last_sent`
        // still reads `true` from the pre-keyed press (it tracks runtime edge
        // state, not config, so `apply` leaves it alone), so the reopened tap
        // forwards the next real transition: the falling edge on release.
        trigger.apply(&modifiers(vec![
            HotkeyModifier::Control,
            HotkeyModifier::Option,
        ]));
        assert_eq!(trigger.on_flags(0), Some(false));
        assert_eq!(trigger.on_flags(ctrl_opt), Some(true));
    }

    /// `DictationEdgeSender::send_edge` forwards `Pressed`/`Released` edges as
    /// `FlagsChanged { both_held }` into the state-machine channel, keeping the
    /// private `HotkeyMsg` type opaque to callers.
    #[test]
    fn dictation_edge_sender_forwards_flags_changed() {
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
        let sender = DictationEdgeSender(msg_tx);

        sender.send_edge(true);
        sender.send_edge(false);

        assert!(matches!(
            msg_rx.try_recv(),
            Ok(HotkeyMsg::FlagsChanged { both_held: true })
        ));
        assert!(matches!(
            msg_rx.try_recv(),
            Ok(HotkeyMsg::FlagsChanged { both_held: false })
        ));
    }
}
