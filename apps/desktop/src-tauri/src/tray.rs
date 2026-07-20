//! Menu-bar tray icon (macOS) / system tray (Windows v2).
//!
//! ## Design notes
//!
//! - **Fixed icon (Phase 11 §11 pivot).** The tray icon is the idle PNG and
//!   never swaps for a session-state change. Wispr Flow ships a single
//!   menu-bar mark for the same reason: the HUD already telegraphs
//!   listening / cleaning / error visually, and a flickering tray icon adds
//!   visual noise without information gain. [`TrayState`] is preserved so
//!   the orchestrator can still drive a tooltip update per state and so a
//!   future per-state asset swap (or a redesigned single icon) can land
//!   with minimal churn.
//! - **Template tinting (`set_icon_as_template(true)`).** macOS auto-inverts
//!   a template image to match the live menu-bar foreground colour, so a
//!   single PNG handles both light and dark menu-bar appearances without
//!   per-theme assets. This is the *only* contract that drives §11's
//!   light/dark check — and because we no longer mutate the icon, it is set
//!   exactly once at [`build`] time.
//! - **Embedded PNGs.** Icons are baked into the binary via `include_bytes!`
//!   so a tooltip update is a memory-only operation (no resource_dir
//!   lookup or disk I/O on the hot path). Per-state PNGs are kept for the
//!   hypothetical future re-introduction of per-state visuals.
//! - The tray ID ([`TRAY_ID`]) is stable so future modules can fetch the
//!   icon via `AppHandle::tray_by_id` without threading an `Arc<TrayIcon>`
//!   through every dependency edge.
//! - Menu item IDs are exposed as constants because both this module and
//!   tests dispatch on them; magic strings would drift the moment a copy
//!   change lands.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, MenuBuilder, MenuEvent, MenuItem, Submenu, SubmenuBuilder};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Emitter, Listener, Manager, Runtime};
use tauri_plugin_store::StoreExt;

use crate::audio;
use crate::audio_devices_watcher;
use crate::session::{BilingualModeFlag, EnglishFastModeFlag};
use crate::settings;

/// Holder for the English-fast-mode menu item. Held so
/// [`handle_english_fast_mode_toggle`] can flip the label between
/// "✓ English Mode" and the unchecked variant on every click.
pub struct EnglishFastModeMenuItem<R: Runtime>(pub MenuItem<R>);

/// Backlog 0012 — sibling of [`EnglishFastModeMenuItem`] for the
/// bilingual-mode toggle. Held so [`handle_bilingual_mode_toggle`]
/// can flip its checked/unchecked label on every click.
///
/// **Dormant since 2026-05-25** — Bilingual Mode is intentionally not
/// exposed in the tray (and the flag is force-OFF at boot in `lib.rs`).
/// The session-layer routing precedence still reads `BilingualModeFlag`,
/// and this struct + the handler + labels stay compiled so re-exposing
/// the feature is a one-line tray-builder change. Do not delete.
#[allow(dead_code)]
pub struct BilingualModeMenuItem<R: Runtime>(pub MenuItem<R>);

/// Handle to the state-driven updater menu item (the same `MENU_ID_CHECK_UPDATES`
/// row). Held so [`set_update_indicator`] can flip its label/enabled state as a
/// background update moves Idle → Downloading → Ready without rebuilding the
/// menu. See `crate::updater` for the phase machine that drives it.
pub struct UpdateMenuItem<R: Runtime>(pub MenuItem<R>);

/// Tray-state for the `Microphone ▸` submenu. Holds the submenu
/// handle, the sentinel "System default" item, and the per-device
/// [`CheckMenuItem`] handles so two operations stay cheap:
///
/// - **Selection flip** (user click) — mutate `is_checked` on the
///   existing items without rebuilding the submenu.
/// - **Hot-plug refresh** (CoreAudio notification) — append newly
///   discovered devices and remove ones that disappeared. The
///   submenu handle and the [`Mutex`] around `devices` are what make
///   this safe to do from the event listener thread.
///
/// The `system_default` item is preserved across refreshes; only its
/// label text changes when the OS default flips.
///
/// `sticky` holds an "<name> (unavailable)" placeholder entry that
/// appears when the persisted selection is a device not present in
/// the current enumeration (e.g., USB mic unplugged, AirPods
/// disconnected). It preserves the user's intent across
/// disconnect/reconnect cycles — the entry is disabled (greyed) and
/// kept checked so the user can see what they picked is still their
/// preference; when the device reappears, the sticky is removed and
/// the real entry takes its place with the check intact.
pub struct MicMenuItems<R: Runtime> {
    pub submenu: Submenu<R>,
    pub system_default: CheckMenuItem<R>,
    pub devices: Mutex<Vec<(String, CheckMenuItem<R>)>>,
    pub sticky: Mutex<Option<(String, CheckMenuItem<R>)>>,
}

// Spike — toggle between Auto-detect (LID router; default) and
// English Mode (manual override that skips LID and routes straight
// to Deepgram). The label is phrased in user-facing terms ("when
// you know you're dictating English") instead of naming the backend,
// so the unchecked state reads as "I'm using the default" rather
// than "something is missing."
const FAST_MODE_LABEL_OFF: &str = "English Mode";
const FAST_MODE_LABEL_ON: &str = "✓ English Mode";

fn fast_mode_label(enabled: bool) -> &'static str {
    if enabled {
        FAST_MODE_LABEL_ON
    } else {
        FAST_MODE_LABEL_OFF
    }
}

// Backlog 0012 — bilingual mode toggle. The label phrases the action as
// "I want this app to handle code-switching" rather than "I'm using
// Whisper" so the user-visible decision is about output quality, not
// the implementation. Off (default) = current hybrid LID routing; on
// = force every press through Whisper for bilingual correctness.
//
// Dormant since 2026-05-25 — see `BilingualModeMenuItem` doc for the
// rationale. Kept compiled so a future re-expose is a one-line change.
#[allow(dead_code)]
const BILINGUAL_MODE_LABEL_OFF: &str = "Bilingual Mode (force Whisper)";
#[allow(dead_code)]
const BILINGUAL_MODE_LABEL_ON: &str = "✓ Bilingual Mode (force Whisper)";

#[allow(dead_code)]
fn bilingual_mode_label(enabled: bool) -> &'static str {
    if enabled {
        BILINGUAL_MODE_LABEL_ON
    } else {
        BILINGUAL_MODE_LABEL_OFF
    }
}

// Microphone submenu. The "System default" sentinel is pinned at the
// top, separated from the enumerated devices so the user always knows
// where to return after experimenting with a specific mic.
const MIC_LABEL_SYSTEM_DEFAULT: &str = "System default";
const MIC_SUBMENU_LABEL: &str = "Microphone";

/// Hint suffix appended to the system-default entry when the resolved
/// OS default has a known name. Keeps the user oriented when their OS
/// default differs from what they expected (e.g. a freshly-attached
/// AirPods set swapped the default mic).
fn system_default_label(resolved_name: Option<&str>) -> String {
    match resolved_name {
        Some(name) if !name.is_empty() => {
            format!("{MIC_LABEL_SYSTEM_DEFAULT} ({name})")
        }
        _ => MIC_LABEL_SYSTEM_DEFAULT.to_string(),
    }
}

/// Stable identifier for the muni tray. `AppHandle::tray_by_id(TRAY_ID)`
/// returns the live icon at any point after [`build`] has succeeded.
pub const TRAY_ID: &str = "muni";

/// Menu item IDs.
///
/// They are stable strings so the menu-event dispatcher can route them
/// without depending on insertion order (muda assigns ids by build order if
/// a caller doesn't supply one — fragile and surprising). Do not rename
/// without updating tests that assert on these values.
/// Spike — toggles `EnglishFastModeFlag` and persists
/// `KEY_ENGLISH_FAST_MODE`. Stable string id so the menu-event
/// dispatcher routes regardless of insertion order.
pub const MENU_ID_FAST_MODE_TOGGLE: &str = "muni.menu.english_fast_mode_toggle";
/// Backlog 0012 — toggles `BilingualModeFlag` and persists
/// `KEY_BILINGUAL_MODE`. Stable id mirrors the fast-mode toggle.
pub const MENU_ID_BILINGUAL_MODE_TOGGLE: &str = "muni.menu.bilingual_mode_toggle";
/// Shared prefix for every entry inside the `Microphone ▸` submenu.
/// The full id is `MENU_ID_MIC_PREFIX` + the device id (cpal device
/// name, or [`settings::SELECTED_MIC_SYSTEM_DEFAULT`] for the sentinel
/// entry). Stripping the prefix recovers the value we persist into
/// `KEY_SELECTED_MICROPHONE`, so the dispatcher and the click handler
/// never need to maintain a separate lookup table.
///
/// Note on hot-plug: cpal's input device list is captured at
/// tray-build time only. Plugging a new USB mic mid-session requires
/// the user to click `Restart Muni` from the tray to see it; rebuilding
/// menus on macOS tray icons is fragile under an active click event
/// and the restart path is already one item below.
pub const MENU_ID_MIC_PREFIX: &str = "muni.menu.mic::";
pub const MENU_ID_HISTORY: &str = "muni.menu.history";
pub const MENU_ID_SETTINGS: &str = "muni.menu.settings";
pub const MENU_ID_CHECK_UPDATES: &str = "muni.menu.check_updates";
/// Labels for the state-driven updater item (`MENU_ID_CHECK_UPDATES`).
/// `crate::updater` swaps between them as a background update progresses;
/// kept here so the tray owns its copy and the click dispatcher can reason
/// about the row's meaning per phase.
pub const UPDATE_ITEM_IDLE_LABEL: &str = "Check for Updates…";
pub const UPDATE_ITEM_DOWNLOADING_LABEL: &str = "Downloading update…";
pub const UPDATE_ITEM_READY_LABEL: &str = "Update ready — Restart to apply";
/// Phase 11 — refresh the AVFoundation / IOKit caches that survive the
/// process. The QA-driven motivation is the macOS "Quit & Reopen"
/// flow: the system caches mic permission decisions per-process, so a
/// runtime grant or revoke only takes effect after a relaunch. The
/// menu item is the one-click sibling to manually `cmd+q`-and-reopen.
pub const MENU_ID_RESTART: &str = "muni.menu.restart";
pub const MENU_ID_QUIT: &str = "muni.menu.quit";

/// Tauri events the tray emits in response to menu clicks. The frontend
/// listens for these to navigate routes. Quit doesn't need an event —
/// Rust calls `app.exit(0)` directly.
pub const EVENT_TRAY_OPEN_HISTORY: &str = "tray://open-history";
pub const EVENT_TRAY_OPEN_SETTINGS: &str = "tray://open-settings";
/// Phase 10 — emitted when the user clicks the tray's "Check for updates…"
/// item. The frontend invokes the `check_for_updates` IPC command and
/// renders a toast with the typed result.
pub const EVENT_TRAY_CHECK_UPDATES: &str = "tray://check-updates";

/// Visual states for the tray icon.
///
/// Mirrors the Swift `MenuBarController.IconState` cases so future history
/// audits comparing v1 vs v2 line up. Names are also the wire payload for
/// the `set_tray_state` command (camelCase via serde).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TrayState {
    Idle,
    Listening,
    Cleaning,
    /// Whisper-batch ASR failed and the one-shot Gladia fallback
    /// transcribe is in flight. Mirrors `SessionState::Recovering`; the
    /// only user-facing surface is the hover tooltip (the tray icon is
    /// fixed since the Phase 11 §11 pivot).
    Recovering,
    Error,
    Offline,
}

impl TrayState {
    /// Parse the wire string used by the `set_tray_state` command. Returns
    /// `None` for unrecognised values so the caller can fail fast rather
    /// than coercing to a default.
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "idle" => Some(Self::Idle),
            "listening" => Some(Self::Listening),
            "cleaning" => Some(Self::Cleaning),
            "recovering" => Some(Self::Recovering),
            "error" => Some(Self::Error),
            "offline" => Some(Self::Offline),
            _ => None,
        }
    }

    /// Tooltip shown when the user hovers over the tray icon. Plain text;
    /// macOS truncates anything over ~250 chars but our copy stays well
    /// inside that.
    pub fn tooltip(self) -> &'static str {
        match self {
            Self::Idle => "Muni — ready",
            Self::Listening => "Muni — listening…",
            Self::Cleaning => "Muni — polishing…",
            Self::Recovering => "Muni — recovering…",
            Self::Error => "Muni — last dictation failed",
            Self::Offline => "Muni — offline",
        }
    }

    /// Raw PNG bytes for this state at 1x. Embedded at compile time so
    /// state swaps don't touch the disk on the hot path.
    fn icon_bytes(self) -> &'static [u8] {
        match self {
            Self::Idle => include_bytes!("../icons/tray/idle.png"),
            Self::Listening => include_bytes!("../icons/tray/listening.png"),
            Self::Cleaning => include_bytes!("../icons/tray/cleaning.png"),
            // Plan 012 — no dedicated icon: the tray icon is fixed
            // post-Phase-11 (only the tooltip changes), so we point at
            // the cleaning asset to keep the constructor honest. This
            // path is unused in production.
            Self::Recovering => include_bytes!("../icons/tray/cleaning.png"),
            Self::Error => include_bytes!("../icons/tray/error.png"),
            Self::Offline => include_bytes!("../icons/tray/offline.png"),
        }
    }

    /// Decode the embedded PNG into a Tauri [`Image`] suitable for the tray.
    pub fn to_image(self) -> tauri::Result<Image<'static>> {
        Image::from_bytes(self.icon_bytes()).map(|img| img.to_owned())
    }
}

/// Returns the single PNG asset used for the menu-bar icon regardless of
/// session state. See the module-level "Fixed icon" note for the
/// rationale; the function exists so the contract has a single source of
/// truth that tests can pin.
pub fn fixed_tray_icon_bytes() -> &'static [u8] {
    TrayState::Idle.icon_bytes()
}

/// Decoded form of [`fixed_tray_icon_bytes`].
pub fn fixed_tray_icon() -> tauri::Result<Image<'static>> {
    TrayState::Idle.to_image()
}

/// The "update ready" tray icon: the idle mark with a mono-dot pip in the
/// top-right corner (variant #9, template-safe). It is the single
/// *deliberate* exception to the Phase 11 §11 fixed-icon rule — the icon
/// still never swaps for a *session* state, only to flag a downloaded update
/// awaiting restart. Because it stays pure-black-plus-alpha it tints with the
/// menu bar exactly like the base icon (see `scripts/gen_tray_update_badge.py`).
fn update_badge_icon_bytes() -> &'static [u8] {
    include_bytes!("../icons/tray/idle_update.png")
}

/// Decoded form of [`update_badge_icon_bytes`].
pub fn update_badge_icon() -> tauri::Result<Image<'static>> {
    Image::from_bytes(update_badge_icon_bytes()).map(|img| img.to_owned())
}

/// Build the tray icon and attach the menu + event handlers to the app.
///
/// Returns the [`TrayIcon`] so callers in tests can drive it directly. In
/// production this is called from the Tauri `setup` closure and the icon is
/// dropped — the registered tray stays alive via the manager's internal
/// resource map and is fetched again later via [`set_state`].
pub fn build<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<TrayIcon<R>> {
    // Spike — English Fast Mode (Deepgram) toggle. Default OFF =
    // Whisper. Reads the live flag value at build time so a returning
    // user sees the persisted state on relaunch.
    let initial_fast_mode = app
        .try_state::<EnglishFastModeFlag>()
        .map(|s| s.is_enabled())
        .unwrap_or(false);
    let fast_mode = MenuItem::with_id(
        app,
        MENU_ID_FAST_MODE_TOGGLE,
        fast_mode_label(initial_fast_mode),
        true,
        None::<&str>,
    )?;
    app.manage(EnglishFastModeMenuItem(fast_mode.clone()));

    // Backlog 0012 — Bilingual Mode toggle is **intentionally not added
    // to the tray menu** as of 2026-05-25. The flag is force-OFF at boot
    // in `lib.rs` and the audio-LID pipeline already handles
    // English/Tagalog code-switching well enough that the manual
    // escape hatch isn't worth the user-confusion surface. The handler,
    // labels, `BilingualModeMenuItem`, and the routing precedence in
    // `session.rs` are all kept compiled — re-exposing the toggle is
    // restoring the build/manage/menu/dispatch lines below + this block.
    // Do not delete the dormant code; it is the rollback path.

    // Microphone submenu — read the persisted selection so the radio
    // check lands on the right entry on relaunch, then enumerate cpal
    // devices once at build time (see `MENU_ID_MIC_PREFIX` for the
    // hot-plug story). When the persisted id no longer matches any
    // present device, the runtime resolver in `audio::build_stream`
    // already falls back to the system default; the submenu mirrors
    // that intent by showing the absent device as unchecked.
    let selected_mic = app
        .store(settings::SETTINGS_FILE)
        .ok()
        .and_then(|s| s.get(settings::KEY_SELECTED_MICROPHONE))
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| settings::SELECTED_MIC_SYSTEM_DEFAULT.to_string());
    let mic_submenu = build_mic_submenu(app, &selected_mic)?;

    let history = MenuItem::with_id(app, MENU_ID_HISTORY, "History", true, None::<&str>)?;
    let settings = MenuItem::with_id(
        app,
        MENU_ID_SETTINGS,
        "Settings…",
        true,
        Some("CmdOrCtrl+,"),
    )?;
    // The updater status item is state-driven: it starts as the manual
    // "Check for Updates…" and `updater::set_phase` flips its label/enabled
    // through Downloading → "Restart to apply" as a background update lands.
    // Held in managed state ([`UpdateMenuItem`]) so the flip is a mutation,
    // not a menu rebuild. `UpdaterState` is managed alongside so the click
    // dispatcher can tell "check" from "restart" without extra plumbing.
    let check_updates = MenuItem::with_id(
        app,
        MENU_ID_CHECK_UPDATES,
        UPDATE_ITEM_IDLE_LABEL,
        true,
        None::<&str>,
    )?;
    app.manage(UpdateMenuItem(check_updates.clone()));
    app.manage(crate::updater::UpdaterState::default());
    // Read the productName from the (possibly --config-merged) Tauri config
    // so the dev shell shows "Restart Muni Dev" / "Quit Muni Dev" while the
    // installed prod app keeps "Restart Muni" / "Quit Muni". Falls back to
    // "Muni" if the config field is somehow absent.
    let product_name = app
        .config()
        .product_name
        .clone()
        .unwrap_or_else(|| "Muni".to_string());
    let restart = MenuItem::with_id(
        app,
        MENU_ID_RESTART,
        format!("Restart {product_name}"),
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(
        app,
        MENU_ID_QUIT,
        format!("Quit {product_name}"),
        true,
        Some("CmdOrCtrl+Q"),
    )?;

    let menu = MenuBuilder::new(app)
        .item(&fast_mode)
        .separator()
        .item(&mic_submenu)
        .separator()
        .item(&history)
        .item(&settings)
        .separator()
        .item(&check_updates)
        .separator()
        .item(&restart)
        .item(&quit)
        .build()?;

    let icon = fixed_tray_icon()?;

    let tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        // `icon_as_template(true)` lets macOS auto-tint the icon to match the
        // live menu-bar foreground colour — this is what drives §11's
        // light/dark-appearance switch. Because the icon itself never
        // mutates after build (Phase 11 fixed-icon pivot), this flag is set
        // exactly once here and is the sole runtime contract for theme
        // adaptation.
        .icon_as_template(true)
        .tooltip(TrayState::Idle.tooltip())
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(handle_menu_event)
        .build(app)?;

    // Keep the `Microphone ▸` submenu live across hot-plug and
    // default-input changes. The CoreAudio watcher in
    // `audio_devices_watcher` is what fires these events; here we
    // just react. The listener is registered once and lives for the
    // process lifetime — there's no scenario where we'd want to stop
    // refreshing the menu while the app is running.
    let listener_handle = app.clone();
    app.listen(audio_devices_watcher::EVENT_DEVICES_CHANGED, move |_| {
        refresh_mic_submenu(&listener_handle);
    });

    Ok(tray)
}

/// Update the tray *tooltip* to reflect a new state. The icon itself is
/// fixed (idle PNG) — see the module-level "Fixed icon" note for the
/// rationale. Hover-tooltip is the only per-state surface remaining on
/// the tray; the HUD carries the visual state.
///
/// No-ops (with a warning) if the tray hasn't been built yet — that path is
/// only hit during early-boot races, never in steady state.
pub fn set_state<R: Runtime>(app: &AppHandle<R>, state: TrayState) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        log::warn!(target: "tray", "set_state({state:?}) before tray built");
        return;
    };
    if let Err(err) = tray.set_tooltip(Some(state.tooltip())) {
        log::warn!(target: "tray", "set_tooltip failed: {err}");
    }
}

/// Reflect an updater phase onto the tray: relabel/enable the updater menu
/// item and (when `badged`) swap the menu-bar icon to the mono-dot variant.
/// Driven exclusively by `crate::updater::set_phase` so the tray, the in-app
/// banner, and the persisted phase never disagree.
///
/// Every step is best-effort: a failed icon swap or label update is logged
/// but never propagated — a stale-but-present indicator beats a crash, and
/// the worst case is the user opens the menu to see the textual state.
pub fn set_update_indicator<R: Runtime>(
    app: &AppHandle<R>,
    label: &str,
    enabled: bool,
    badged: bool,
) {
    if let Some(item) = app.try_state::<UpdateMenuItem<R>>() {
        if let Err(err) = item.0.set_text(label) {
            log::warn!(target: "tray", "update item set_text failed: {err}");
        }
        if let Err(err) = item.0.set_enabled(enabled) {
            log::warn!(target: "tray", "update item set_enabled failed: {err}");
        }
    } else {
        log::warn!(target: "tray", "set_update_indicator before tray built");
    }

    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        log::warn!(target: "tray", "set_update_indicator: tray not built");
        return;
    };
    let icon = if badged {
        update_badge_icon()
    } else {
        fixed_tray_icon()
    };
    match icon {
        Ok(img) => {
            if let Err(err) = tray.set_icon(Some(img)) {
                log::warn!(target: "tray", "update-badge set_icon failed: {err}");
            }
            // Re-assert template mode: `set_icon` resets it, and the badge
            // must keep tinting with the menu-bar foreground like the base.
            if let Err(err) = tray.set_icon_as_template(true) {
                log::warn!(target: "tray", "update-badge set_icon_as_template failed: {err}");
            }
        }
        Err(err) => log::warn!(target: "tray", "decode update-badge icon failed: {err}"),
    }
}

/// Dispatch a menu click to the right action.
///
/// For navigation items (History / Settings / Check-Updates) we ONLY
/// emit the corresponding event — the JS-side handler in
/// `useTrayEvents.ts` calls `flushSync(navigate)` to land the new
/// route synchronously, then invokes `focus_main_window` to bring
/// the window forward. Doing the focus from Rust (even after
/// emitting) lost a race against the React render: heavy routes
/// (Settings → General with multiple `useSetting` invokes on mount)
/// were visible as the previous route for a frame after the window
/// surfaced. Letting JS sequence the commit + focus avoids that race
/// entirely — the route is fully rendered before macOS is asked to
/// animate the window.
///
/// Quit closes the app immediately — matches the Cmd+Q reflex users
/// have for menu-bar apps. Pause flips the shared atomic flag and
/// persists the preference so the next launch starts in the same
/// state.
fn handle_menu_event<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    let id = event.id().as_ref();
    log::info!(target: "tray", "menu click: {id}");
    if let Some(device_id) = id.strip_prefix(MENU_ID_MIC_PREFIX) {
        handle_mic_selection(app, device_id);
        return;
    }
    match id {
        MENU_ID_FAST_MODE_TOGGLE => handle_english_fast_mode_toggle(app),
        // MENU_ID_BILINGUAL_MODE_TOGGLE dispatch intentionally omitted —
        // see the `BilingualModeMenuItem` doc for the dormancy rationale.
        // Restore by adding back: `MENU_ID_BILINGUAL_MODE_TOGGLE => handle_bilingual_mode_toggle(app),`
        MENU_ID_HISTORY => emit_event(app, EVENT_TRAY_OPEN_HISTORY),
        MENU_ID_SETTINGS => emit_event(app, EVENT_TRAY_OPEN_SETTINGS),
        MENU_ID_CHECK_UPDATES => match crate::updater::current_phase(app) {
            // When a background update is already downloaded the row reads
            // "Restart to apply" — clicking it restarts into the swapped
            // bundle. Otherwise it's the manual "Check for Updates…" that
            // the frontend services via the `check_for_updates` command.
            crate::updater::UpdaterPhase::Ready { .. } => {
                log::info!(target: "tray", "restart-to-apply-update via tray menu");
                app.restart();
            }
            _ => emit_event(app, EVENT_TRAY_CHECK_UPDATES),
        },
        MENU_ID_RESTART => {
            log::info!(target: "tray", "restart requested via tray menu");
            app.restart();
        }
        MENU_ID_QUIT => {
            log::info!(target: "tray", "quit requested via tray menu");
            app.exit(0);
        }
        unknown => {
            log::warn!(target: "tray", "unknown menu id {unknown}");
        }
    }
}

/// Build the `Microphone ▸` submenu and stash the per-device check
/// items into Tauri-managed state so [`handle_mic_selection`] (click)
/// and [`refresh_mic_submenu`] (CoreAudio hot-plug) can mutate them
/// without rebuilding the parent menu.
///
/// Enumeration is synchronous via cpal; the call is fast in practice
/// (one CoreAudio probe on macOS) but is the reason this helper exists
/// as a dedicated function instead of inlining into [`build`].
fn build_mic_submenu<R: Runtime>(app: &AppHandle<R>, selected: &str) -> tauri::Result<Submenu<R>> {
    let devices = audio::enumerate_input_devices();
    let default_name = devices
        .iter()
        .find(|d| d.is_default)
        .map(|d| d.name.clone());

    let is_default_selected = selected == settings::SELECTED_MIC_SYSTEM_DEFAULT;
    let system_default = CheckMenuItem::with_id(
        app,
        format!(
            "{MENU_ID_MIC_PREFIX}{}",
            settings::SELECTED_MIC_SYSTEM_DEFAULT
        ),
        system_default_label(default_name.as_deref()),
        true,
        is_default_selected,
        None::<&str>,
    )?;

    let mut builder = SubmenuBuilder::new(app, MIC_SUBMENU_LABEL).item(&system_default);
    let mut device_items: Vec<(String, CheckMenuItem<R>)> = Vec::with_capacity(devices.len());
    if !devices.is_empty() {
        builder = builder.separator();
    }
    for device in &devices {
        let item_id = format!("{MENU_ID_MIC_PREFIX}{}", device.id);
        let is_selected = !is_default_selected && device.id == selected;
        let item =
            CheckMenuItem::with_id(app, item_id, &device.name, true, is_selected, None::<&str>)?;
        builder = builder.item(&item);
        device_items.push((device.id.clone(), item));
    }

    // Sticky-at-boot: the saved selection points at a device that
    // isn't currently enumerated (e.g., a USB mic that's unplugged,
    // or AirPods that aren't connected yet). Surface it as a
    // disabled+checked placeholder so the user can see their intent
    // is preserved — when the device reappears, `refresh_mic_submenu`
    // removes the sticky and the real entry takes over.
    let needs_sticky = !is_default_selected && !devices.iter().any(|d| d.id == selected);
    let sticky_item: Option<(String, CheckMenuItem<R>)> = if needs_sticky {
        let item_id = format!("{MENU_ID_MIC_PREFIX}{selected}");
        let label = format!("{selected} (unavailable)");
        let item = CheckMenuItem::with_id(app, item_id, label, false, true, None::<&str>)?;
        builder = builder.item(&item);
        Some((selected.to_string(), item))
    } else {
        None
    };

    let submenu = builder.build()?;

    app.manage(MicMenuItems {
        submenu: submenu.clone(),
        system_default,
        devices: Mutex::new(device_items),
        sticky: Mutex::new(sticky_item),
    });
    Ok(submenu)
}

/// Re-enumerate cpal input devices and reconcile the live submenu in
/// place: remove items for devices that disappeared, append items for
/// devices that just plugged in, and update the "System default
/// (...)" label to reflect the current OS default name.
///
/// Called from the [`audio_devices_watcher::EVENT_DEVICES_CHANGED`]
/// listener, which itself fires from a CoreAudio property
/// notification. The mutex on `devices` is the synchronisation point
/// between this writer and the click handler reader.
///
/// Failures along the way are logged but never propagated — a partial
/// refresh is still better than a frozen menu, and the user can
/// always click `Restart Muni` to recover.
fn refresh_mic_submenu<R: Runtime>(app: &AppHandle<R>) {
    let Some(items_state) = app.try_state::<MicMenuItems<R>>() else {
        log::debug!(target: "tray", "refresh_mic_submenu: MicMenuItems not yet managed");
        return;
    };

    let selected = app
        .store(settings::SETTINGS_FILE)
        .ok()
        .and_then(|s| s.get(settings::KEY_SELECTED_MICROPHONE))
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| settings::SELECTED_MIC_SYSTEM_DEFAULT.to_string());
    let is_default_selected = selected == settings::SELECTED_MIC_SYSTEM_DEFAULT;

    let new_devices = audio::enumerate_input_devices();
    let new_default_name = new_devices
        .iter()
        .find(|d| d.is_default)
        .map(|d| d.name.clone());

    // Update the sentinel label + checked state first so the OS
    // default flip is visible even if a later step fails.
    if let Err(err) = items_state
        .system_default
        .set_text(system_default_label(new_default_name.as_deref()))
    {
        log::warn!(target: "tray", "set_text on system-default item failed: {err}");
    }
    if let Err(err) = items_state.system_default.set_checked(is_default_selected) {
        log::warn!(target: "tray", "set_checked on system-default item failed: {err}");
    }

    let mut devices_guard = match items_state.devices.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!(target: "tray", "mic devices mutex poisoned; recovering");
            poisoned.into_inner()
        }
    };
    let mut sticky_guard = match items_state.sticky.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!(target: "tray", "mic sticky mutex poisoned; recovering");
            poisoned.into_inner()
        }
    };

    let new_ids: std::collections::HashSet<&str> =
        new_devices.iter().map(|d| d.id.as_str()).collect();
    let old_ids: std::collections::HashSet<String> =
        devices_guard.iter().map(|(id, _)| id.clone()).collect();

    // Remove items whose underlying devices vanished.
    devices_guard.retain(|(id, item)| {
        if new_ids.contains(id.as_str()) {
            true
        } else {
            if let Err(err) = items_state.submenu.remove(item) {
                log::warn!(target: "tray", "remove mic item {id:?} failed: {err}");
            }
            false
        }
    });

    // Append items for newly-discovered devices. The submenu starts
    // with `[system_default, separator, device_0, device_1, ...]`;
    // newcomers join at the end, which matches the order cpal
    // returns them.
    for device in &new_devices {
        if old_ids.contains(&device.id) {
            // Existing device — just refresh its check state.
            if let Some((_, item)) = devices_guard.iter().find(|(id, _)| id == &device.id) {
                let should_check = !is_default_selected && device.id == selected;
                if let Err(err) = item.set_checked(should_check) {
                    log::warn!(target: "tray", "set_checked on mic item {:?} failed: {err}", device.id);
                }
            }
            continue;
        }
        // First time we've seen this device — build a fresh item.
        // If it's the very first entry overall we also need a
        // separator between the sentinel and the device list. The
        // initial build added one when `devices` was non-empty;
        // matching that here keeps the visual layout consistent.
        if devices_guard.is_empty() {
            match tauri::menu::PredefinedMenuItem::separator(app) {
                Ok(sep) => {
                    if let Err(err) = items_state.submenu.append(&sep) {
                        log::warn!(target: "tray", "append separator failed: {err}");
                    }
                }
                Err(err) => {
                    log::warn!(target: "tray", "build separator failed: {err}");
                }
            }
        }
        let item_id = format!("{MENU_ID_MIC_PREFIX}{}", device.id);
        let is_selected = !is_default_selected && device.id == selected;
        let item = match CheckMenuItem::with_id(
            app,
            item_id,
            &device.name,
            true,
            is_selected,
            None::<&str>,
        ) {
            Ok(item) => item,
            Err(err) => {
                log::warn!(target: "tray", "build mic item {:?} failed: {err}", device.id);
                continue;
            }
        };
        if let Err(err) = items_state.submenu.append(&item) {
            log::warn!(target: "tray", "append mic item {:?} failed: {err}", device.id);
            continue;
        }
        devices_guard.push((device.id.clone(), item));
    }

    // Sticky entry reconciliation. The persisted selection points at
    // a device that should be checked. If that device is missing from
    // the current enumeration we keep a disabled+checked
    // "<name> (unavailable)" entry so the user can see their intent
    // is preserved across disconnect/reconnect — strictly better than
    // a submenu with nothing checked. If the device is present (or
    // the user has switched to system default), we tear the sticky
    // down.
    let selected_missing = !is_default_selected && !new_ids.contains(selected.as_str());
    if selected_missing {
        let needs_new = sticky_guard
            .as_ref()
            .map(|(id, _)| id != &selected)
            .unwrap_or(true);
        if needs_new {
            // Remove a sticky for a different device first.
            if let Some((old_id, old_item)) = sticky_guard.take() {
                if let Err(err) = items_state.submenu.remove(&old_item) {
                    log::warn!(target: "tray", "remove old sticky {old_id:?} failed: {err}");
                }
            }
            let item_id = format!("{MENU_ID_MIC_PREFIX}{selected}");
            let label = format!("{selected} (unavailable)");
            match CheckMenuItem::with_id(app, item_id, label, false, true, None::<&str>) {
                Ok(item) => {
                    if let Err(err) = items_state.submenu.append(&item) {
                        log::warn!(target: "tray", "append sticky {selected:?} failed: {err}");
                    } else {
                        log::info!(
                            target: "audio",
                            "mic sticky entry added for absent device {selected:?}"
                        );
                        *sticky_guard = Some((selected.clone(), item));
                    }
                }
                Err(err) => {
                    log::warn!(target: "tray", "build sticky {selected:?} failed: {err}");
                }
            }
        } else if let Some((_, item)) = sticky_guard.as_ref() {
            // Sticky already exists for this device — make sure it
            // stays checked (defence-in-depth; shouldn't drift in
            // practice since clicks elsewhere change the selection).
            if let Err(err) = item.set_checked(true) {
                log::warn!(target: "tray", "set_checked on sticky failed: {err}");
            }
        }
    } else if let Some((old_id, old_item)) = sticky_guard.take() {
        // Either selection moved to system default, the user picked
        // another real device, or the formerly-absent device just
        // reappeared in the enumeration. Either way the sticky is no
        // longer needed.
        if let Err(err) = items_state.submenu.remove(&old_item) {
            log::warn!(target: "tray", "remove sticky {old_id:?} failed: {err}");
        }
        log::info!(
            target: "audio",
            "mic sticky entry removed (was {old_id:?})"
        );
    }

    log::info!(
        target: "audio",
        "mic submenu refreshed: {} device(s), default={:?}, selected={:?}, sticky={}",
        new_devices.len(),
        new_default_name.as_deref().unwrap_or(""),
        selected,
        sticky_guard.is_some()
    );
}

/// Persist the newly-selected mic and delegate the menu reconciliation
/// (check marks, sticky entry teardown/creation) to
/// [`refresh_mic_submenu`].
///
/// We do NOT validate that the id exists in the current device list —
/// `audio::build_stream` already falls back to the system default for
/// a missing device, and rejecting a selection here would prevent the
/// user from "pre-selecting" a mic they're about to plug back in.
fn handle_mic_selection<R: Runtime>(app: &AppHandle<R>, device_id: &str) {
    let new_value = if device_id.is_empty() {
        settings::SELECTED_MIC_SYSTEM_DEFAULT
    } else {
        device_id
    };

    match app.store(settings::SETTINGS_FILE) {
        Ok(store) => {
            store.set(settings::KEY_SELECTED_MICROPHONE, json!(new_value));
            if let Err(err) = store.save() {
                log::warn!(target: "tray", "save selected_microphone failed: {err}");
            }
        }
        Err(err) => {
            log::warn!(target: "tray", "settings store unavailable for mic selection: {err}");
        }
    }

    log::info!(
        target: "audio",
        "selected_microphone → {new_value:?} — next press will use this device"
    );

    // Reconcile through the single source of truth so a click also
    // tears down a sticky-unavailable entry if the user picks
    // somewhere else, and so adding new check-state rules only needs
    // to land in one function.
    refresh_mic_submenu(app);
}

/// Spike — flip the ASR routing flag, persist, log so dev/QA can
/// confirm which path the next press will take. The next press reads
/// the new value (atomic load) without restart, so a flip → press
/// sequence shows the new backend in the `[asr]` log line of the very
/// next press.
fn handle_english_fast_mode_toggle<R: Runtime>(app: &AppHandle<R>) {
    let flag = match app.try_state::<EnglishFastModeFlag>() {
        Some(s) => s.inner().clone(),
        None => {
            log::warn!(
                target: "tray",
                "EnglishFastModeFlag not managed; fast-mode toggle is a no-op"
            );
            return;
        }
    };
    let now_enabled = flag.toggle();
    log::info!(
        target: "asr",
        "english_fast_mode toggled → {now_enabled} ({}) — next press will route accordingly",
        if now_enabled { "deepgram (forced)" } else { "auto-detect" }
    );

    if let Some(item_state) = app.try_state::<EnglishFastModeMenuItem<R>>() {
        if let Err(err) = item_state.0.set_text(fast_mode_label(now_enabled)) {
            log::warn!(target: "tray", "set_text on fast-mode item failed: {err}");
        }
    }

    if let Ok(store) = app.store(settings::SETTINGS_FILE) {
        store.set(settings::KEY_ENGLISH_FAST_MODE, json!(now_enabled));
        if let Err(err) = store.save() {
            log::warn!(target: "tray", "save english_fast_mode failed: {err}");
        }
    } else {
        log::warn!(target: "tray", "settings store unavailable for fast-mode toggle");
    }
}

/// Backlog 0012 — flip the bilingual-mode flag, persist, and update the
/// menu label. Mirrors [`handle_english_fast_mode_toggle`] in shape;
/// the difference is which downstream code path the next press takes
/// (LID router skipped → straight to Whisper).
///
/// **Dormant since 2026-05-25.** Not wired into [`handle_menu_event`]
/// because the tray entry was removed; see `BilingualModeMenuItem`
/// doc for the rationale and the one-line restore path.
#[allow(dead_code)]
fn handle_bilingual_mode_toggle<R: Runtime>(app: &AppHandle<R>) {
    let flag = match app.try_state::<BilingualModeFlag>() {
        Some(s) => s.inner().clone(),
        None => {
            log::warn!(
                target: "tray",
                "BilingualModeFlag not managed; bilingual-mode toggle is a no-op"
            );
            return;
        }
    };
    let now_enabled = flag.toggle();
    log::info!(
        target: "asr",
        "bilingual_mode toggled → {now_enabled} ({}) — next press will route accordingly",
        if now_enabled { "whisper (forced)" } else { "auto-detect / hybrid LID" }
    );

    if let Some(item_state) = app.try_state::<BilingualModeMenuItem<R>>() {
        if let Err(err) = item_state.0.set_text(bilingual_mode_label(now_enabled)) {
            log::warn!(target: "tray", "set_text on bilingual-mode item failed: {err}");
        }
    }

    if let Ok(store) = app.store(settings::SETTINGS_FILE) {
        store.set(settings::KEY_BILINGUAL_MODE, json!(now_enabled));
        if let Err(err) = store.save() {
            log::warn!(target: "tray", "save bilingual_mode failed: {err}");
        }
    } else {
        log::warn!(target: "tray", "settings store unavailable for bilingual-mode toggle");
    }
}

fn emit_event<R: Runtime>(app: &AppHandle<R>, event: &str) {
    if let Err(err) = app.emit(event, ()) {
        log::warn!(target: "tray", "emit {event} failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_state_round_trips_through_serde() {
        for state in [
            TrayState::Idle,
            TrayState::Listening,
            TrayState::Cleaning,
            TrayState::Recovering,
            TrayState::Error,
            TrayState::Offline,
        ] {
            let json = serde_json::to_string(&state).expect("serialize");
            let parsed: TrayState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed, state);
        }
    }

    #[test]
    fn tray_state_from_wire_matches_serde_camel_case() {
        let pairs = [
            ("idle", TrayState::Idle),
            ("listening", TrayState::Listening),
            ("cleaning", TrayState::Cleaning),
            ("recovering", TrayState::Recovering),
            ("error", TrayState::Error),
            ("offline", TrayState::Offline),
        ];
        for (wire, expected) in pairs {
            assert_eq!(TrayState::from_wire(wire), Some(expected));
            // Cross-check that the same string is what serde emits.
            let json = serde_json::to_string(&expected).unwrap();
            assert_eq!(json, format!("\"{wire}\""));
        }
    }

    #[test]
    fn tray_state_from_wire_rejects_unknown_values() {
        assert_eq!(TrayState::from_wire(""), None);
        assert_eq!(TrayState::from_wire("unknown"), None);
        // Capitalisation must match exactly — protects the command surface
        // against drift between the frontend and Rust.
        assert_eq!(TrayState::from_wire("Idle"), None);
    }

    #[test]
    fn every_state_decodes_to_a_valid_image() {
        for state in [
            TrayState::Idle,
            TrayState::Listening,
            TrayState::Cleaning,
            TrayState::Recovering,
            TrayState::Error,
            TrayState::Offline,
        ] {
            let img = state.to_image().expect("decode template png");
            // Sanity: 1x assets are 22×22 (see scripts/gen_tray_icons.py).
            // If a future asset overhaul changes this, update the constant.
            assert!(img.width() >= 16, "icon for {state:?} too small");
            assert!(img.height() >= 16, "icon for {state:?} too small");
        }
    }

    #[test]
    fn tray_state_tooltip_is_not_empty() {
        for state in [
            TrayState::Idle,
            TrayState::Listening,
            TrayState::Cleaning,
            TrayState::Recovering,
            TrayState::Error,
            TrayState::Offline,
        ] {
            assert!(!state.tooltip().is_empty());
        }
    }

    #[test]
    fn fixed_tray_icon_resolves_to_idle_asset() {
        // Pins the Phase 11 §11 contract: the menu-bar icon is fixed,
        // sourced from `idle.png`, and never swaps for a session-state
        // change. If a future patch wants per-state icons back, this test
        // is the canary — flipping `fixed_tray_icon_bytes` to anything
        // other than `Idle.icon_bytes()` should be a deliberate, reviewed
        // change.
        let fixed = fixed_tray_icon_bytes();
        let idle = TrayState::Idle.icon_bytes();
        assert_eq!(
            fixed.as_ptr(),
            idle.as_ptr(),
            "fixed tray icon must point at the idle asset (use Idle.icon_bytes())"
        );

        let fixed_image = fixed_tray_icon().expect("fixed icon decodes");
        let idle_image = TrayState::Idle.to_image().expect("idle decodes");
        assert_eq!(fixed_image.width(), idle_image.width());
        assert_eq!(fixed_image.height(), idle_image.height());
    }

    #[test]
    fn fixed_icon_is_template_friendly_for_light_dark_menu_bar() {
        // §11 light/dark switching is done by macOS template tinting
        // (`icon_as_template(true)` in `build()`). For that to work, the
        // icon must have a usable alpha channel — a fully opaque rectangle
        // would render as a black blob in light mode and a white blob in
        // dark mode with no shape. Decode the fixed icon and assert there
        // is BOTH transparent space (alpha < 255) and visible pixels
        // (alpha > 0); that's the structural check that template tinting
        // will produce a recognisable mark in either appearance.
        let img = fixed_tray_icon().expect("fixed icon decodes");
        let rgba = img.rgba();
        assert!(!rgba.is_empty(), "decoded image must have pixel data");
        assert_eq!(
            rgba.len() % 4,
            0,
            "Tauri image rgba is RGBA8 — length must be divisible by 4"
        );

        let mut has_transparent = false;
        let mut has_visible = false;
        for chunk in rgba.chunks_exact(4) {
            let alpha = chunk[3];
            if alpha == 0 {
                has_transparent = true;
            } else if alpha == 255 {
                has_visible = true;
            }
            if has_transparent && has_visible {
                break;
            }
        }
        assert!(
            has_transparent,
            "fixed icon has no transparent pixels — macOS template tinting needs alpha"
        );
        assert!(
            has_visible,
            "fixed icon has no opaque pixels — would render invisible under template tinting"
        );
    }

    #[test]
    fn update_badge_icon_is_template_friendly_and_idle_sized() {
        // The #9 mono-dot "update ready" icon must (a) decode, (b) match the
        // idle icon's dimensions so the menu bar doesn't jump when it swaps,
        // and (c) keep a usable alpha channel so macOS template tinting
        // renders a recognisable mark+pip in both light and dark menu bars.
        let badge = update_badge_icon().expect("update-badge icon decodes");
        let idle = TrayState::Idle.to_image().expect("idle decodes");
        assert_eq!(badge.width(), idle.width(), "badge width must match idle");
        assert_eq!(
            badge.height(),
            idle.height(),
            "badge height must match idle"
        );

        let rgba = badge.rgba();
        assert_eq!(rgba.len() % 4, 0, "rgba must be RGBA8");
        let mut has_transparent = false;
        let mut has_visible = false;
        for chunk in rgba.chunks_exact(4) {
            match chunk[3] {
                0 => has_transparent = true,
                255 => has_visible = true,
                _ => {}
            }
            if has_transparent && has_visible {
                break;
            }
        }
        assert!(has_transparent, "badge needs alpha for template tinting");
        assert!(
            has_visible,
            "badge would render invisible under template tinting"
        );
    }

    #[test]
    fn update_item_labels_are_distinct() {
        // The three phase labels must differ so the row visibly changes as a
        // background update progresses (Idle → Downloading → Ready).
        let labels = [
            UPDATE_ITEM_IDLE_LABEL,
            UPDATE_ITEM_DOWNLOADING_LABEL,
            UPDATE_ITEM_READY_LABEL,
        ];
        let mut sorted = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            labels.len(),
            "updater item labels must be distinct"
        );
    }

    #[test]
    fn menu_event_constants_are_unique() {
        let ids = [
            MENU_ID_FAST_MODE_TOGGLE,
            MENU_ID_BILINGUAL_MODE_TOGGLE,
            MENU_ID_HISTORY,
            MENU_ID_SETTINGS,
            MENU_ID_CHECK_UPDATES,
            MENU_ID_RESTART,
            MENU_ID_QUIT,
        ];
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate menu id");
    }

    #[test]
    fn mic_menu_prefix_does_not_collide_with_other_menu_ids() {
        // The dispatcher routes by `id.strip_prefix(MENU_ID_MIC_PREFIX)`,
        // so no other registered menu id may legitimately start with this
        // prefix — otherwise a click on (say) `muni.menu.mic_pause` would
        // be misrouted into the mic handler. Pin the contract.
        for id in [
            MENU_ID_FAST_MODE_TOGGLE,
            MENU_ID_BILINGUAL_MODE_TOGGLE,
            MENU_ID_HISTORY,
            MENU_ID_SETTINGS,
            MENU_ID_CHECK_UPDATES,
            MENU_ID_RESTART,
            MENU_ID_QUIT,
        ] {
            assert!(
                !id.starts_with(MENU_ID_MIC_PREFIX),
                "menu id {id:?} collides with mic-submenu prefix"
            );
        }
    }

    #[test]
    fn system_default_label_includes_resolved_name_when_present() {
        // Surfacing the OS-resolved default in the menu label keeps the
        // user oriented when the OS default differs from what they
        // expected (eg. AirPods just connected).
        assert_eq!(
            system_default_label(Some("MacBook Pro Microphone")),
            "System default (MacBook Pro Microphone)"
        );
        assert_eq!(system_default_label(Some("")), "System default");
        assert_eq!(system_default_label(None), "System default");
    }

    #[test]
    fn mic_menu_id_round_trips_through_prefix() {
        // The dispatcher recovers the device id by stripping the
        // prefix; assert the inverse so refactors don't silently break
        // the persistence boundary (the stripped value is what lands
        // in `KEY_SELECTED_MICROPHONE`).
        let device = "Shure SM7B";
        let id = format!("{MENU_ID_MIC_PREFIX}{device}");
        assert_eq!(id.strip_prefix(MENU_ID_MIC_PREFIX), Some(device));
        let sentinel_id = format!(
            "{MENU_ID_MIC_PREFIX}{}",
            settings::SELECTED_MIC_SYSTEM_DEFAULT
        );
        assert_eq!(
            sentinel_id.strip_prefix(MENU_ID_MIC_PREFIX),
            Some(settings::SELECTED_MIC_SYSTEM_DEFAULT)
        );
    }
}
