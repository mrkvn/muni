//! HUD overlay window — Phase 7.
//!
//! Mirrors Swift v1's `UI/HUDPanel.swift`. The HUD is a small, transparent,
//! click-through window that floats above every other surface (including
//! fullscreen apps) while the user holds the dictation hotkey. It renders
//! a Framer Motion waveform reactive to `audio://amplitude`.
//!
//! ## Responsibilities (Rust side)
//!
//! 1. Configure the underlying `NSWindow` so it
//!    - sits at `NSStatusWindowLevel`,
//!    - joins all spaces and stays auxiliary in fullscreen contexts,
//!    - ignores mouse events (click-through),
//!    - never participates in the window cycle.
//!
//!    Tauri's window builder doesn't expose any of those four properties, so
//!    they MUST be applied via objc2 after the `WebviewWindow` is built. We
//!    do this once during `setup` on the main thread.
//!
//! 2. React to [`SessionState`] transitions: position the HUD on the active
//!    monitor and show it on `Listening`; hide it 150 ms after any other
//!    state so React's `AnimatePresence` fade-out can play before the OS
//!    yanks the surface.
//!
//! ## Threading
//!
//! - [`configure_for_overlay`] uses `objc2-app-kit` setters, which assume the
//!   main thread (NSWindow is `MainThreadOnly`). Callers MUST invoke it from
//!   the Tauri `setup` closure.
//! - [`handle_state_change`] calls Tauri's window APIs (`set_position`,
//!   `show`, `hide`) which dispatch internally to the event loop and are
//!   safe from any thread.
//!
//! ## Hide debouncing
//!
//! Rapid press/release/press sequences (the user releasing then immediately
//! re-pressing) would otherwise flicker the HUD. We use a generation counter:
//! every state change bumps it, every scheduled hide captures the value at
//! schedule time and checks it again before hiding. If a newer transition
//! has landed in the meantime, the hide is dropped.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::{
    AppHandle, Emitter, LogicalPosition, Manager, Monitor, PhysicalPosition, Runtime, WebviewWindow,
};

use crate::error::HudTone;
use crate::error_presenter::{HudNoticePayload, EVENT_HUD_NOTICE};
use crate::session::{SessionState, SessionStateTracker};

/// Logical width of the HUD window. Must equal `tauri.conf.json` `hud.width`.
///
/// Wide enough to hold the full notice-chip copy (e.g. "Deepgram key
/// rejected — check Settings") centered above the pill without clipping.
/// The window is transparent + click-through, so the extra width has no
/// visual cost — only the centered pill/chip paint.
pub const HUD_LOGICAL_WIDTH: f64 = 340.0;
/// Logical height of the HUD window. Must equal `tauri.conf.json` `hud.height`.
///
/// Tall enough to stack the notice chip above the pill (chip + gap + pill +
/// bottom margin) with headroom for the chip's entrance. Content is
/// bottom-anchored (see `HudWindow.tsx`) so the pill keeps a fixed position
/// regardless of whether a chip is present; the slack above is transparent.
pub const HUD_LOGICAL_HEIGHT: f64 = 104.0;
/// Pixels between the HUD window's **bottom** edge and the screen's bottom
/// edge. Content is bottom-anchored, so anchoring the window bottom (rather
/// than its top) keeps the pill at a stable spot as the window grows upward
/// to fit a notice chip. Tuned so the pill sits ~10pt above the screen
/// bottom, matching the prior 120×46 placement.
pub const HUD_BOTTOM_OFFSET: f64 = 2.0;
/// Window label as registered in `tauri.conf.json`.
pub const HUD_WINDOW_LABEL: &str = "hud";
/// Grace window between leaving `Listening` and hiding the OS window. Long
/// enough for the React fade-out to finish; short enough that re-engagement
/// inside it is rare.
pub const HUD_HIDE_DELAY_MS: u64 = 150;

/// Minimum time the OS window stays visible after a `Listening` show, even
/// if the state has already transitioned away. Enforces a perceivable flash
/// for short-lived presses (e.g. a hotkey press with a missing API key
/// fails `pool.take()` in microseconds; without this floor, the user sees
/// nothing because the React entrance animation never has time to render).
///
/// Kept slightly larger than the React `useMinDurationTrue` budget so the
/// OS window stays open through the React exit animation that follows the
/// min-duration window.
pub const HUD_MIN_VISIBLE_MS: u64 = 400;

/// How long a HUD notice chip stays on screen before the Rust side
/// schedules the OS window hide (Feature 035). Mirrors the frontend
/// auto-clear timer (`HUD_NOTICE_DWELL_MS` in `useHudNotice.ts`) so the
/// React fade-out and the OS hide stay in lockstep. The actual hide fires
/// `HUD_HIDE_DELAY_MS` later so the chip's exit animation can finish before
/// the surface is yanked.
pub const HUD_NOTICE_DWELL_MS: u64 = 2500;

/// Holds the cancellation generation for pending hide tasks AND the wall-clock
/// timestamp of the last show, so a hide that lands inside the min-visible
/// window can extend its delay. One instance is stashed on the Tauri state
/// via [`AppHandle::manage`] so every state-change observer cancels the same
/// pending task.
#[derive(Debug, Default)]
pub struct HudController {
    hide_generation: AtomicU64,
    /// Unix-millis at last `Listening` show. `0` means "never shown".
    last_shown_ms: AtomicU64,
    /// Unix-millis until which an active notice chip must stay visible — a
    /// hide *floor*. A state-change hide (e.g. the `Idle` that lands ~190 ms
    /// after a post-paste FYI fires) won't fire before this, so the chip isn't
    /// clipped to a flash. `0` means "no active notice".
    notice_until_ms: AtomicU64,
}

impl HudController {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Increment and return the new generation. SeqCst so the load inside the
    /// scheduled hide observes this exact value and not a torn read.
    fn bump_generation(&self) -> u64 {
        self.hide_generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    fn current_generation(&self) -> u64 {
        self.hide_generation.load(Ordering::SeqCst)
    }

    fn record_shown(&self) {
        self.last_shown_ms.store(now_unix_ms(), Ordering::SeqCst);
    }

    /// Return how long the HUD has been visible since the last `record_shown`.
    /// Returns `None` if the HUD has never been shown.
    fn elapsed_since_shown(&self) -> Option<Duration> {
        let stored = self.last_shown_ms.load(Ordering::SeqCst);
        if stored == 0 {
            return None;
        }
        let now = now_unix_ms();
        Some(Duration::from_millis(now.saturating_sub(stored)))
    }

    /// Mark a notice chip active for `dwell` from now — establishes the hide
    /// floor (see [`remaining_notice_dwell`](Self::remaining_notice_dwell)).
    fn record_notice(&self, dwell: Duration) {
        let until = now_unix_ms().saturating_add(dwell.as_millis() as u64);
        self.notice_until_ms.store(until, Ordering::SeqCst);
    }

    /// Clear the notice floor — used when a fresh press takes over the HUD so a
    /// stale notice can't extend the new press's hide.
    fn clear_notice(&self) {
        self.notice_until_ms.store(0, Ordering::SeqCst);
    }

    /// How much longer an active notice chip must stay visible (`0` if there's
    /// no active notice or its dwell already elapsed). A state-change hide
    /// floors its delay to this so it can't clip the chip.
    fn remaining_notice_dwell(&self) -> Duration {
        let until = self.notice_until_ms.load(Ordering::SeqCst);
        Duration::from_millis(until.saturating_sub(now_unix_ms()))
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Compute the hide delay so the OS window stays visible for at least
/// [`HUD_MIN_VISIBLE_MS`] from the last show, then waits one more
/// [`HUD_HIDE_DELAY_MS`] for the React fade-out before hiding.
///
/// Pure helper; testable with synthesized elapsed durations.
pub(crate) fn hide_delay_with_min_visible(
    elapsed_since_shown: Option<Duration>,
    min_visible: Duration,
    base_delay: Duration,
) -> Duration {
    let elapsed = match elapsed_since_shown {
        // Never shown: just use the base delay. Hiding a window that
        // wasn't visible is a no-op anyway.
        None => return base_delay,
        Some(d) => d,
    };
    if elapsed >= min_visible {
        return base_delay;
    }
    // Shown for less than the floor → extend so the user perceives the
    // flash before the fade-out plays.
    (min_visible - elapsed) + base_delay
}

/// Apply macOS-only NSWindow flags so the HUD floats above normal windows
/// and over fullscreen apps. No-op on non-macOS targets.
pub fn configure_for_overlay<R: Runtime>(window: &WebviewWindow<R>) -> tauri::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::configure(window)?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
    }
    Ok(())
}

/// Surface the HUD using AppKit's `orderFrontRegardless`, which is the
/// only show primitive that pierces a fullscreen Space's shielding
/// window. Tauri's `WebviewWindow::show` alone is *not* sufficient —
/// see the `macos` module's design note. No-op on non-macOS targets.
pub fn order_front_regardless<R: Runtime>(window: &WebviewWindow<R>) -> tauri::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::order_front_regardless(window)?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
    }
    Ok(())
}

/// Re-apply the HUD's window level + collection behavior right before
/// each show. Tauri's `alwaysOnTop` and `visibleOnAllWorkspaces`
/// config map to lower-level NSWindow flags that can be re-applied at
/// runtime by Tauri's internal show machinery; reasserting the
/// `CGShieldingWindowLevel + 1` overlay level + `Transient |
/// FullScreenAuxiliary | …` behavior on every show keeps us above any
/// app's fullscreen shield. No-op on non-macOS targets.
pub fn reapply_overlay_level<R: Runtime>(window: &WebviewWindow<R>) -> tauri::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::reapply_overlay_level(window)?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
    }
    Ok(())
}

/// React to a [`SessionState`] transition. Show + reposition on `Listening`,
/// schedule a debounced hide otherwise.
///
/// Plan 012 — `Cleaning` and `Recovering` keep the HUD pill visible
/// (the React layer renders distinct visuals per state); only `Idle`
/// and `Error` schedule the hide. Without this, the pill would
/// disappear the moment the user releases the hotkey — including
/// during the Whisper-batch fallback's multi-second branch — leaving
/// the user staring at an unchanged screen and wondering whether the
/// hotkey registered.
pub fn handle_state_change<R: Runtime>(
    controller: &Arc<HudController>,
    app: &AppHandle<R>,
    state: SessionState,
) {
    log::info!(target: "hud", "state-change: {state:?}");
    let Some(window) = app.get_webview_window(HUD_WINDOW_LABEL) else {
        log::warn!(
            target: "hud",
            "state-change {state:?} but the HUD window is missing from the app",
        );
        return;
    };

    // States that keep the HUD pill visible. The React layer also
    // gates visibility on this set (see `PILL_VISIBLE_STATES` in
    // HudWindow.tsx); both sides must agree. Plan 030 — `ListeningLocked`
    // is treated identically to `Listening` for visibility purposes (it
    // IS a listening state; the React layer renders the distinct
    // locked-mode affordance).
    if matches!(
        state,
        SessionState::Listening
            | SessionState::ListeningLocked
            | SessionState::Cleaning
            | SessionState::Recovering
    ) {
        // Treat either `Listening` variant as a "fresh show" trigger —
        // both have reliable hotkey-press timing for the min-visible
        // floor and both need the monitor-pick + overlay-level reapply
        // work. `Cleaning` and `Recovering` are continuations of an
        // already-visible HUD; we just cancel any pending hide and
        // return early.
        if !matches!(
            state,
            SessionState::Listening | SessionState::ListeningLocked
        ) {
            controller.bump_generation();
            return;
        }
    }

    if matches!(
        state,
        SessionState::Listening | SessionState::ListeningLocked
    ) {
        // Bumping cancels any in-flight hide so a fast re-engagement never
        // flickers the window off.
        controller.bump_generation();
        controller.record_shown();
        // A fresh press owns the HUD now — drop any stale notice floor so the
        // previous chip's dwell can't extend this press's hide.
        controller.clear_notice();
        if let Err(err) = position_on_active_screen(&window) {
            log::warn!(target: "hud", "position failed: {err}");
        }
        if let Err(err) = window.show() {
            log::warn!(target: "hud", "show failed: {err}");
        }
        // Re-assert the overlay level + collection behavior (so any
        // Tauri internal reset doesn't leave us at NSFloatingWindowLevel)
        // and `orderFrontRegardless` so the window pierces the
        // fullscreen shield without becoming key. Both calls touch raw
        // NSWindow setters, which are main-thread-only — this state-
        // change handler fires from the session orchestrator's tokio
        // task, so we hop via `run_on_main_thread`. (Manual QA §16.1.)
        let window_for_main = window.clone();
        if let Err(err) = app.run_on_main_thread(move || {
            if let Err(err) = reapply_overlay_level(&window_for_main) {
                log::warn!(target: "hud", "reapply overlay level failed: {err}");
            }
            if let Err(err) = order_front_regardless(&window_for_main) {
                log::warn!(target: "hud", "orderFrontRegardless failed: {err}");
            }
        }) {
            log::warn!(target: "hud", "run_on_main_thread for HUD show failed: {err}");
        }
        return;
    }

    let pending = controller.bump_generation();
    // Floor the hide to any active notice's remaining dwell. Without this, the
    // `Idle` state-change that lands ~190 ms after a post-paste FYI fires would
    // schedule a ~150 ms hide and clip the chip to a flash (dogfood scenario 5,
    // 2026-06-18).
    let delay = hide_delay_with_min_visible(
        controller.elapsed_since_shown(),
        Duration::from_millis(HUD_MIN_VISIBLE_MS),
        Duration::from_millis(HUD_HIDE_DELAY_MS),
    )
    .max(controller.remaining_notice_dwell());
    let controller = Arc::clone(controller);
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(delay).await;
        if controller.current_generation() != pending {
            // A newer transition landed during the grace window — leave the
            // HUD where it is.
            return;
        }
        if let Err(err) = window.hide() {
            log::warn!(target: "hud", "hide failed: {err}");
        }
    });
}

/// Show a HUD notice chip above the pill (Feature 035).
///
/// The error presenter routes [`crate::error::ErrorSurface::HudNotice`]
/// kinds here. The chip is a short FYI ("Connection dropped", "Pasted
/// without cleanup", …) that surfaces above the pill, dwells for
/// [`HUD_NOTICE_DWELL_MS`], then fades. The OS window is re-shown if it was
/// hidden (a notice can fire *after* a press, e.g. a cleanup-skipped FYI)
/// so the chip is visible even when no pill is up.
///
/// ## Show/hide choreography
///
/// We reuse [`HudController`]'s generation counter so the notice hide and
/// the press-driven hide share one cancellation source. On a notice:
///   1. `bump_generation()` cancels any pending hide (press-release or a
///      prior notice).
///   2. `record_shown()` resets the min-visible clock.
///   3. The OS window is positioned + shown + raised (same as a `Listening`
///      show) so the chip can render over any fullscreen Space.
///   4. [`EVENT_HUD_NOTICE`] tells the frontend to render the chip.
///   5. A generation-guarded hide is scheduled after the dwell + the fade
///      budget. It is skipped if a newer transition bumped the generation
///      (e.g. a fresh dictation press) **or** if the session is still in a
///      pill-visible state — so a notice that fires mid-`Listening`
///      (a connection blip during capture) never tears down the live pill.
///
/// Missing managed state (early boot / unit tests without the controller or
/// the HUD window) is logged and skipped; this never panics.
pub fn show_notice<R: Runtime>(app: &AppHandle<R>, text: &str, tone: HudTone) {
    log::info!(target: "hud", "show_notice tone={tone:?}");

    let Some(window) = app.get_webview_window(HUD_WINDOW_LABEL) else {
        log::warn!(
            target: "hud",
            "show_notice but the HUD window is missing from the app",
        );
        return;
    };
    let Some(controller) = app.try_state::<Arc<HudController>>() else {
        log::warn!(
            target: "hud",
            "show_notice but the HUD controller is not managed yet; skipping",
        );
        return;
    };
    let controller = controller.inner().clone();

    // Cancel any pending hide and reset the min-visible clock so a notice
    // landing right after a hide schedule wins. Establish the dwell floor so a
    // state-change hide (e.g. the post-paste `Idle`) can't clip the chip.
    controller.bump_generation();
    controller.record_shown();
    controller.record_notice(Duration::from_millis(
        HUD_NOTICE_DWELL_MS + HUD_HIDE_DELAY_MS,
    ));

    if let Err(err) = position_on_active_screen(&window) {
        log::warn!(target: "hud", "position failed: {err}");
    }
    if let Err(err) = window.show() {
        log::warn!(target: "hud", "show failed: {err}");
    }
    // Re-assert overlay level + raise on the main thread (raw NSWindow
    // setters are main-thread-only); same pattern as the `Listening` show.
    let window_for_main = window.clone();
    if let Err(err) = app.run_on_main_thread(move || {
        if let Err(err) = reapply_overlay_level(&window_for_main) {
            log::warn!(target: "hud", "reapply overlay level failed: {err}");
        }
        if let Err(err) = order_front_regardless(&window_for_main) {
            log::warn!(target: "hud", "orderFrontRegardless failed: {err}");
        }
    }) {
        log::warn!(target: "hud", "run_on_main_thread for HUD notice failed: {err}");
    }

    let payload = HudNoticePayload {
        text: text.to_string(),
        tone,
    };
    if let Err(err) = app.emit(EVENT_HUD_NOTICE, &payload) {
        log::warn!(target: "hud", "emit {EVENT_HUD_NOTICE} failed: {err}");
    }

    // Schedule the OS-window hide after the dwell + fade budget. Mirrors the
    // press-release hide at the bottom of `handle_state_change`.
    let pending = controller.bump_generation();
    let delay = Duration::from_millis(HUD_NOTICE_DWELL_MS + HUD_HIDE_DELAY_MS);
    let app_for_hide = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(delay).await;
        if controller.current_generation() != pending {
            // A newer transition (a fresh press or a later notice) landed
            // during the dwell — leave the window where it is.
            return;
        }
        // Don't hide while a dictation is in flight: a notice can fire
        // mid-`Listening` (a connection blip during capture), and the chip
        // auto-clears itself on the frontend after the dwell. The pill must
        // outlive it, so only hide when the session is idle.
        if session_is_pill_visible(&app_for_hide) {
            return;
        }
        if let Err(err) = window.hide() {
            log::warn!(target: "hud", "hide failed: {err}");
        }
    });
}

/// True when the managed [`SessionStateTracker`] reports a state that keeps
/// the pill visible. Used by the notice hide so an in-flight dictation isn't
/// torn down when a notice's dwell expires. Returns `false` (safe to hide)
/// if the tracker isn't managed yet.
fn session_is_pill_visible<R: Runtime>(app: &AppHandle<R>) -> bool {
    let Some(tracker) = app.try_state::<Arc<SessionStateTracker>>() else {
        return false;
    };
    matches!(
        tracker.get(),
        SessionState::Listening
            | SessionState::ListeningLocked
            | SessionState::Cleaning
            | SessionState::Recovering
    )
}

/// Position the HUD bottom-center of the **active** monitor — defined as the
/// screen the cursor currently sits on. Falls back to the HUD's own current
/// monitor and then the primary monitor if the cursor can't be located (some
/// platforms gate `cursor_position()` behind elevated permissions).
///
/// The cursor is the cheapest reliable proxy macOS gives us for "where the
/// user is looking right now" without a separate Accessibility query for the
/// frontmost app's window — `WebviewWindow::current_monitor()` only knows
/// where the HUD was last placed, which is useless before the first show.
pub fn position_on_active_screen<R: Runtime>(window: &WebviewWindow<R>) -> tauri::Result<()> {
    let monitors = window.available_monitors()?;
    if monitors.is_empty() {
        log::warn!(target: "hud", "no monitors available; skipping reposition");
        return Ok(());
    }

    let cursor_opt = window.cursor_position().ok();
    let monitor = pick_active_monitor(&monitors, cursor_opt, || {
        window.current_monitor().ok().flatten()
    });

    let Some(monitor) = monitor else {
        log::warn!(target: "hud", "could not resolve active monitor; skipping reposition");
        return Ok(());
    };

    let (x, y) = bottom_center_logical_position(
        (monitor.position().x, monitor.position().y),
        (monitor.size().width, monitor.size().height),
        monitor.scale_factor(),
        (HUD_LOGICAL_WIDTH, HUD_LOGICAL_HEIGHT),
        HUD_BOTTOM_OFFSET,
    );

    window.set_position(LogicalPosition::new(x, y))
}

/// Pick the monitor the user is currently working on.
///
/// Strategy (first match wins):
///   1. Whichever monitor's physical rect contains the cursor.
///   2. The HUD window's own `current_monitor()` (resolved lazily — only
///      called if the cursor lookup fell through).
///   3. The first monitor in `monitors` (a stable tie-break that mirrors
///      Tauri's primary-monitor ordering on macOS).
fn pick_active_monitor<F>(
    monitors: &[Monitor],
    cursor: Option<PhysicalPosition<f64>>,
    fallback_current: F,
) -> Option<Monitor>
where
    F: FnOnce() -> Option<Monitor>,
{
    if let Some(cursor) = cursor {
        if let Some(idx) = monitor_index_for_cursor(monitors, (cursor.x, cursor.y)) {
            return Some(monitors[idx].clone());
        }
    }
    if let Some(current) = fallback_current() {
        return Some(current);
    }
    monitors.first().cloned()
}

/// Pure helper: index of the first monitor whose physical rect contains the
/// cursor position. Top-left inclusive, bottom-right exclusive — matches
/// Tauri's coordinate convention.
pub(crate) fn monitor_index_for_cursor(monitors: &[Monitor], cursor: (f64, f64)) -> Option<usize> {
    let rects: Vec<(i32, i32, u32, u32)> = monitors
        .iter()
        .map(|m| {
            let p = m.position();
            let s = m.size();
            (p.x, p.y, s.width, s.height)
        })
        .collect();
    rect_index_for_cursor(&rects, cursor)
}

/// Test-friendly variant of [`monitor_index_for_cursor`]. Operates on raw
/// `(x, y, width, height)` tuples so callers don't need a live Tauri runtime
/// to construct `Monitor` instances.
pub(crate) fn rect_index_for_cursor(
    rects: &[(i32, i32, u32, u32)],
    cursor: (f64, f64),
) -> Option<usize> {
    rects.iter().position(|&(x, y, w, h)| {
        let left = x as f64;
        let top = y as f64;
        let right = left + w as f64;
        let bottom = top + h as f64;
        cursor.0 >= left && cursor.0 < right && cursor.1 >= top && cursor.1 < bottom
    })
}

/// Pure helper: project a monitor's physical rect into the logical
/// bottom-center position for an overlay of `hud_logical_size`.
///
/// Split out as a free function so the math is unit-testable without
/// constructing a `WebviewWindow` (which requires a live Tauri runtime).
pub(crate) fn bottom_center_logical_position(
    monitor_position_physical: (i32, i32),
    monitor_size_physical: (u32, u32),
    scale_factor: f64,
    hud_logical_size: (f64, f64),
    bottom_offset: f64,
) -> (f64, f64) {
    // Guard against a 0 or negative scale factor — `Monitor::scale_factor`
    // is documented to be positive but the Tauri shim returns f64 so a
    // misbehaving runtime could leak a zero. Treat it as 1.0 to keep the
    // position math meaningful instead of dividing by zero.
    let scale = if scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };

    let (px, py) = monitor_position_physical;
    let (pw, ph) = monitor_size_physical;
    let (hw, hh) = hud_logical_size;

    let logical_x = px as f64 / scale;
    let logical_y = py as f64 / scale;
    let logical_w = pw as f64 / scale;
    let logical_h = ph as f64 / scale;

    let mid_x = logical_x + logical_w / 2.0;
    let max_y = logical_y + logical_h;

    // Anchor the window's BOTTOM edge `bottom_offset` px above the screen
    // bottom (top-left origin = bottom edge minus the window height). Content
    // is bottom-anchored, so this keeps the pill stable as the window height
    // grows to make room for a notice chip stacked above it.
    (mid_x - hw / 2.0, max_y - bottom_offset - hh)
}

#[cfg(target_os = "macos")]
mod macos {
    //! Apply NSWindow flags via objc2. Mirrors HUDPanel.swift's `init` plus
    //! `setLevel(.statusBar)` / `collectionBehavior` configuration.
    //!
    //! ## Why these specific flags
    //!
    //! - **Level = `CGShieldingWindowLevel() + 1`.** When another app
    //!   enters fullscreen mode, macOS creates a "shielding window" at a
    //!   high level (~1000) that obscures every regular window — including
    //!   `NSStatusWindowLevel` (25) which is just above the menu bar.
    //!   `FullScreenAuxiliary` collection behavior is *not* enough on its
    //!   own; the level must also clear the shield. `CGShieldingWindowLevel`
    //!   is a CoreGraphics primitive that returns the live shield level on
    //!   the running OS — `+ 1` puts our HUD one notch above any
    //!   third-party app's fullscreen shield. (This is the same trick
    //!   Tauri's own `tao` window backend uses for its borderless-
    //!   fullscreen overlay path; see `tao::platform_impl::macos::window::set_fullscreen`.)
    //! - **`Transient` not `Stationary`.** `Stationary` excludes the
    //!   window from Mission Control reflows; `Transient` excludes it
    //!   from window cycling and Mission Control entirely. Swift v1's
    //!   HUDPanel uses `Transient` and matches the look we want — the
    //!   HUD shouldn't appear in Cmd+Tab or Mission Control thumbnails.
    //! - **`orderFrontRegardless()` after Tauri `show()`.** Tauri's
    //!   `window.show()` calls `makeKeyAndOrderFront:` under the hood,
    //!   which can be suppressed when the foreground app owns a
    //!   fullscreen Space. `orderFrontRegardless` is the AppKit primitive
    //!   that says "surface this window even if a shield is up" without
    //!   making it key (so it doesn't activate Muni's app and bounce
    //!   the user out of fullscreen).

    use objc2_app_kit::{NSWindow, NSWindowCollectionBehavior, NSWindowLevel};
    use tauri::{Runtime, WebviewWindow};

    // CoreGraphics gives us the live shielding-window level for the
    // running macOS version. Declaring the FFI here keeps the dependency
    // surface zero and matches the pattern tao itself uses internally.
    extern "C" {
        fn CGShieldingWindowLevel() -> i32;
    }

    fn overlay_level() -> NSWindowLevel {
        // SAFETY: free function with no preconditions; returns an i32.
        let shield = unsafe { CGShieldingWindowLevel() };
        (shield as NSWindowLevel).saturating_add(1)
    }

    fn overlay_collection_behavior() -> NSWindowCollectionBehavior {
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Transient
            | NSWindowCollectionBehavior::IgnoresCycle
            | NSWindowCollectionBehavior::FullScreenAuxiliary
    }

    pub(super) fn configure<R: Runtime>(window: &WebviewWindow<R>) -> tauri::Result<()> {
        let Some(ns_window) = ns_window_ref(window)? else {
            return Ok(());
        };

        ns_window.setLevel(overlay_level());
        ns_window.setCollectionBehavior(overlay_collection_behavior());
        ns_window.setIgnoresMouseEvents(true);
        // Match Swift v1's HUDPanel.hidesOnDeactivate = false. Without
        // this, the HUD vanishes whenever Muni isn't the frontmost app
        // — which is *always*, since the user is dictating *into*
        // someone else's window. The default for a regular NSWindow is
        // false but Tauri's WebviewWindow sometimes flips it; setting
        // it explicitly is cheap and removes the variable.
        ns_window.setHidesOnDeactivate(false);
        Ok(())
    }

    /// Re-apply the level + collection-behavior right before showing.
    /// Tauri's `alwaysOnTop: true` config maps to `NSFloatingWindowLevel`
    /// (3) under the hood; if any internal Tauri path resets the level
    /// between configure and show, this re-application keeps the HUD
    /// above the fullscreen shield. The setter is idempotent — cheap
    /// to call every show.
    pub(super) fn reapply_overlay_level<R: Runtime>(
        window: &WebviewWindow<R>,
    ) -> tauri::Result<()> {
        let Some(ns_window) = ns_window_ref(window)? else {
            return Ok(());
        };
        ns_window.setLevel(overlay_level());
        ns_window.setCollectionBehavior(overlay_collection_behavior());
        Ok(())
    }

    pub(super) fn order_front_regardless<R: Runtime>(
        window: &WebviewWindow<R>,
    ) -> tauri::Result<()> {
        let Some(ns_window) = ns_window_ref(window)? else {
            return Ok(());
        };
        // orderOut→orderFrontRegardless forces macOS to re-evaluate
        // which Space the window joins. Without the orderOut, a
        // window that was previously visible on Space A keeps its
        // Space-A membership even after the user switches to Space B
        // (a fullscreen Chrome Space, for example) — and on Space B,
        // calling orderFrontRegardless leaves the window pinned to A.
        // The user's §16 observation matched this: HUD only became
        // visible once Mission Control flattened the Z-order.
        // Detaching with orderOut: nil first sheds the prior Space
        // anchor, and the subsequent orderFrontRegardless attaches to
        // whichever Space is currently active. CanJoinAllSpaces
        // collection behavior (set in `configure`) is what allows
        // that attach to land on a fullscreen Space.
        ns_window.orderOut(None);
        ns_window.orderFrontRegardless();
        Ok(())
    }

    /// Borrow the live NSWindow for the duration of the caller's setter
    /// calls. Returns `None` if Tauri reports a null pointer (window
    /// already disposed); the caller should log+skip rather than panic.
    fn ns_window_ref<R: Runtime>(window: &WebviewWindow<R>) -> tauri::Result<Option<&NSWindow>> {
        let ptr = window.ns_window()?;
        if ptr.is_null() {
            log::warn!(target: "hud", "ns_window() returned null; skipping NSWindow setter");
            return Ok(None);
        }
        // SAFETY: Tauri owns the NSWindow for the lifetime of the
        // WebviewWindow. We borrow it only for the duration of the
        // caller's setter calls and never retain it. NSWindow setters
        // are main-thread-only and we are called from main-thread paths
        // (the Tauri `setup` closure or the state notifier wired
        // through `handle_state_change` via `run_on_main_thread`).
        Ok(Some(unsafe { &*ptr.cast::<NSWindow>() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HUD_SIZE: (f64, f64) = (HUD_LOGICAL_WIDTH, HUD_LOGICAL_HEIGHT);

    #[test]
    fn bottom_center_centers_on_primary_retina_screen() {
        // 14" MBP-shaped retina: 2880×1800 physical, scale 2 → 1440×900 logical.
        let (x, y) =
            bottom_center_logical_position((0, 0), (2880, 1800), 2.0, HUD_SIZE, HUD_BOTTOM_OFFSET);
        // mid_x_logical = 720, HUD half-width = 170 → x = 550
        assert!((x - 550.0).abs() < 1e-6, "x = {x}");
        // max_y_logical = 900, window bottom 2 above it, minus height 104 → y = 794
        assert!((y - 794.0).abs() < 1e-6, "y = {y}");
    }

    #[test]
    fn bottom_center_respects_secondary_monitor_offset() {
        // Secondary 1440×900 sitting to the right of a primary 1440-wide screen.
        let (x, y) = bottom_center_logical_position(
            (1440, 0),
            (1440, 900),
            1.0,
            HUD_SIZE,
            HUD_BOTTOM_OFFSET,
        );
        assert!((x - (1440.0 + 720.0 - 170.0)).abs() < 1e-6, "x = {x}");
        assert!((y - (900.0 - 2.0 - 104.0)).abs() < 1e-6, "y = {y}");
    }

    #[test]
    fn bottom_center_uses_unit_scale_when_factor_is_zero() {
        let (x, y) = bottom_center_logical_position((0, 0), (1280, 720), 0.0, (100.0, 50.0), 10.0);
        assert!((x - (640.0 - 50.0)).abs() < 1e-6, "x = {x}");
        // window bottom 10 above screen bottom, minus height 50 → y = 660
        assert!((y - (720.0 - 10.0 - 50.0)).abs() < 1e-6, "y = {y}");
    }

    #[test]
    fn bottom_center_handles_negative_origin_for_left_secondary() {
        // External display sitting to the left of the primary at -1440x.
        let (x, y) = bottom_center_logical_position(
            (-1920, 0),
            (1920, 1080),
            1.0,
            HUD_SIZE,
            HUD_BOTTOM_OFFSET,
        );
        assert!((x - (-1920.0 + 960.0 - 170.0)).abs() < 1e-6, "x = {x}");
        assert!((y - (1080.0 - 2.0 - 104.0)).abs() < 1e-6, "y = {y}");
    }

    #[test]
    fn hide_delay_uses_base_when_min_visible_already_reached() {
        // Press lasted longer than the min-visible floor → no extension.
        let delay = hide_delay_with_min_visible(
            Some(Duration::from_millis(2000)),
            Duration::from_millis(400),
            Duration::from_millis(150),
        );
        assert_eq!(delay, Duration::from_millis(150));
    }

    #[test]
    fn hide_delay_uses_base_when_never_shown() {
        // Hide before the HUD ever showed (defensive path; in practice the
        // notifier always sees Listening first). Should not extend.
        let delay = hide_delay_with_min_visible(
            None,
            Duration::from_millis(400),
            Duration::from_millis(150),
        );
        assert_eq!(delay, Duration::from_millis(150));
    }

    #[test]
    fn hide_delay_extends_to_floor_for_short_press() {
        // Shown only 5 ms ago — extend so total visibility reaches 400 ms,
        // then add the 150 ms fade-out budget on top.
        let delay = hide_delay_with_min_visible(
            Some(Duration::from_millis(5)),
            Duration::from_millis(400),
            Duration::from_millis(150),
        );
        // (400 - 5) + 150 = 545
        assert_eq!(delay, Duration::from_millis(545));
    }

    #[test]
    fn hide_delay_extends_for_press_exactly_at_floor() {
        // Boundary: elapsed == min_visible → no extension.
        let delay = hide_delay_with_min_visible(
            Some(Duration::from_millis(400)),
            Duration::from_millis(400),
            Duration::from_millis(150),
        );
        assert_eq!(delay, Duration::from_millis(150));
    }

    #[test]
    fn hide_delay_extends_for_press_just_under_floor() {
        let delay = hide_delay_with_min_visible(
            Some(Duration::from_millis(399)),
            Duration::from_millis(400),
            Duration::from_millis(150),
        );
        assert_eq!(delay, Duration::from_millis(151));
    }

    #[test]
    fn hud_controller_generation_is_strictly_monotonic() {
        let c = HudController::default();
        let g1 = c.bump_generation();
        let g2 = c.bump_generation();
        let g3 = c.bump_generation();
        assert!(g2 > g1, "g2 = {g2}, g1 = {g1}");
        assert!(g3 > g2, "g3 = {g3}, g2 = {g2}");
    }

    #[test]
    fn notice_floor_extends_until_cleared() {
        let c = HudController::default();
        // No notice → no floor (a state-change hide uses its base delay).
        assert_eq!(c.remaining_notice_dwell(), Duration::ZERO);

        // Recording a notice establishes a floor close to its dwell — this is
        // what stops the post-paste `Idle` hide from clipping the chip.
        c.record_notice(Duration::from_millis(2500));
        let remaining = c.remaining_notice_dwell();
        assert!(
            remaining > Duration::from_millis(2000) && remaining <= Duration::from_millis(2500),
            "remaining = {remaining:?}",
        );

        // A fresh press clears the floor so a stale notice can't extend it.
        c.clear_notice();
        assert_eq!(c.remaining_notice_dwell(), Duration::ZERO);
    }

    #[test]
    fn rect_index_for_cursor_picks_containing_rect() {
        // Two side-by-side 1440×900 monitors, no scale tracking needed.
        let rects = vec![(0, 0, 1440, 900), (1440, 0, 1440, 900)];
        assert_eq!(rect_index_for_cursor(&rects, (200.0, 100.0)), Some(0));
        assert_eq!(rect_index_for_cursor(&rects, (1500.0, 100.0)), Some(1));
    }

    #[test]
    fn rect_index_for_cursor_respects_negative_origin_for_left_secondary() {
        let rects = vec![(-1920, 0, 1920, 1080), (0, 0, 2560, 1440)];
        assert_eq!(rect_index_for_cursor(&rects, (-100.0, 50.0)), Some(0));
        assert_eq!(rect_index_for_cursor(&rects, (10.0, 50.0)), Some(1));
    }

    #[test]
    fn rect_index_for_cursor_treats_right_and_bottom_edges_as_exclusive() {
        // Touching the right edge of monitor 0 lands the cursor on monitor 1
        // — matches Tauri's half-open rect convention so cursors at the
        // shared border don't double-count.
        let rects = vec![(0, 0, 1440, 900), (1440, 0, 1440, 900)];
        assert_eq!(rect_index_for_cursor(&rects, (1440.0, 50.0)), Some(1));
        assert_eq!(rect_index_for_cursor(&rects, (1439.999, 50.0)), Some(0));
    }

    #[test]
    fn rect_index_for_cursor_returns_none_when_cursor_outside_any_rect() {
        // A cursor reading from a stale OS API (or a virtual desktop above
        // all monitors) might land outside every known rect.
        let rects = vec![(0, 0, 1440, 900)];
        assert_eq!(rect_index_for_cursor(&rects, (-1.0, 0.0)), None);
        assert_eq!(rect_index_for_cursor(&rects, (0.0, 9999.0)), None);
    }

    #[test]
    fn rect_index_for_cursor_returns_none_for_empty_monitor_list() {
        assert_eq!(rect_index_for_cursor(&[], (0.0, 0.0)), None);
    }

    #[test]
    fn notice_hide_delay_is_dwell_plus_fade_budget() {
        // The notice OS-window hide fires after the chip's on-screen dwell
        // plus the fade-out budget, so the React exit animation can finish
        // before the surface is yanked. Guards the constant from drifting
        // away from the frontend `HUD_NOTICE_DWELL_MS`.
        let delay = Duration::from_millis(HUD_NOTICE_DWELL_MS + HUD_HIDE_DELAY_MS);
        assert_eq!(delay, Duration::from_millis(2650));
    }

    #[test]
    fn hud_controller_current_matches_last_bump() {
        let c = HudController::default();
        let g = c.bump_generation();
        assert_eq!(c.current_generation(), g);
        let g2 = c.bump_generation();
        assert_eq!(c.current_generation(), g2);
        assert_ne!(g, g2);
    }
}
