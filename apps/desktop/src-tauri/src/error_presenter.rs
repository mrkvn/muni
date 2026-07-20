//! Phase 10 / Feature 035 — surface a [`MuniError`] to the user.
//!
//! Four surfaces, picked by [`MuniError::surface`] from the error's
//! origin/actionability (NOT its [`MuniError::severity`], which Feature 035
//! demoted to the focus/interrupt dial):
//!
//! - **Notification** — a *silent* macOS banner via
//!   `tauri-plugin-notification` (no sound on content, auth options, or
//!   presentation) so an actionable error (denied permission, missing or
//!   rejected API key) is seen even when the Main window is closed — the
//!   steady state for a menu-bar app. If the error's
//!   [`MuniError::requires_user_action_now`] is `true`, the Main window is
//!   also brought forward.
//! - **HudNotice** — a short text chip above the HUD pill (`hud://notice`,
//!   ~2.5s dwell), for non-actionable mid-dictation FYI (connection blips,
//!   words-lost, partial recovered, cleanup-skipped, no-speech). The tray
//!   icon does NOT flash — since the Phase 11 §11 pivot it's a fixed PNG
//!   with macOS template tinting; `set_state` only updates the tooltip on
//!   `SessionState::Error`.
//! - **Toast** — an in-app Sonner toast (`error://quiet`), for
//!   Settings-window interactions (the Main window is already open).
//! - **Silent** — log-only, for internal self-healing fallbacks (Gemini
//!   LID, audio-LID, VAD, Parakeet) the user never needs to see.
//!
//! The Notification and Toast surfaces additionally emit
//! `error://navigate-to-tab` whenever the error has a [`SettingsTab`]
//! association, so a click on the notification (or in the toast UI) can take
//! the user directly to the tab where they can fix the underlying issue.
//!
//! Mirrors Swift v1's `ErrorPresenter` in surface and intent.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::error::{ErrorSeverity, ErrorSurface, HudTone, MuniError, SettingsTab};
use crate::hud;

/// Tauri event for quiet errors. HUD listens and flashes an amber pill.
pub const EVENT_ERROR_QUIET: &str = "error://quiet";

/// Tauri event carrying a HUD notice chip (Feature 035). The HUD window
/// listens and renders a short text chip above the pill for ~2.5s. The
/// single source of truth for the event name + payload shape lives here;
/// [`crate::hud::show_notice`] re-uses both when it emits.
pub const EVENT_HUD_NOTICE: &str = "hud://notice";

/// JSON shape carried by [`EVENT_HUD_NOTICE`]. The HUD frontend renders
/// `text` and switches styling on `tone`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HudNoticePayload {
    pub text: String,
    pub tone: HudTone,
}

/// Tauri event hinting which Settings tab can resolve the error. Main
/// window's router subscribes and navigates on receipt.
pub const EVENT_NAVIGATE_TO_TAB: &str = "error://navigate-to-tab";

/// JSON shape carried by both events. Frontend switches on `kind` and
/// uses `userMessage` directly; `settingsTab` is `null` when the error
/// has no associated tab.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorPayload {
    pub kind: &'static str,
    pub severity: ErrorSeverity,
    pub user_message: String,
    pub settings_tab: Option<SettingsTab>,
}

impl ErrorPayload {
    /// Build the payload from a [`MuniError`]. Pulled out of `present`
    /// so callers (and tests) can construct payloads without touching
    /// an `AppHandle`.
    pub fn from_error(err: &MuniError) -> Self {
        Self {
            kind: kind_of(err),
            severity: err.severity(),
            user_message: err.user_message(),
            settings_tab: err.settings_tab(),
        }
    }
}

/// Feature 033 — is this a routine, *anticipated* degradation (so it should NOT
/// raise a Sentry non-fatal), or an unexpected handled error (worth a Sentry
/// capture)?
///
/// "Expected" = the orchestrator has a planned response (raw-text fallback,
/// route-to-Whisper, silent idle, a permission/config the user must fix) AND/OR
/// it's already counted by a PostHog health event from `session.rs`
/// (`fallback_fired` / `dictation_failed`). Capturing those to Sentry would be
/// noise and, for the terminal/fallback cases, a double-count.
///
/// "Unexpected" = something that shouldn't normally happen and points at a bug
/// or a packaging/environment fault we'd want a scrubbed stack trace for
/// (invalid responses, an empty-key/unknown-service UI bug, a missing bundled
/// resource).
fn is_expected(err: &MuniError) -> bool {
    match err {
        // Routine transient/degradation paths with a planned fallback, OR
        // already surfaced via a PostHog health event from session.rs. Quiet
        // connection/transport hiccups, the terminal/fallback transcription
        // failures, permission/config prompts the user resolves, and the
        // no-op "nothing to paste".
        MuniError::InputMonitoringDenied
        | MuniError::HotkeyTapInstallFailed
        | MuniError::MicrophoneDenied
        | MuniError::NoInputDevice
        | MuniError::AudioStreamFailed { .. }
        | MuniError::DeepgramMissingApiKey
        | MuniError::DeepgramConnectionFailed { .. }
        | MuniError::DeepgramServerError { .. }
        | MuniError::GroqMissingApiKey
        | MuniError::GroqConnectionFailed { .. }
        | MuniError::GroqServerError { .. }
        | MuniError::GeminiMissingApiKey
        | MuniError::GeminiConnectionFailed { .. }
        | MuniError::GeminiServerError { .. }
        | MuniError::AccessibilityDenied
        | MuniError::NothingToPaste
        | MuniError::GladiaMissingApiKey
        | MuniError::GladiaConnectionFailed { .. }
        | MuniError::GladiaServerError { .. }
        // Already counted by `dictation_failed` / `fallback_fired` in session.rs
        // — don't also raise a Sentry error for these (no double-count).
        | MuniError::GladiaFinalizeTimeoutAfterAudio
        | MuniError::TranscriptionUnavailable
        // Degrade-gracefully model load/inference paths (fall back to a working
        // route).
        | MuniError::AudioLidLoadFailed { .. }
        | MuniError::AudioLidInferenceFailed { .. }
        | MuniError::VadLoadFailed { .. }
        | MuniError::VadInferenceFailed { .. }
        | MuniError::ParakeetSidecarUnavailable { .. }
        | MuniError::ParakeetTranscribeFailed { .. }
        // Slice 10 (039) — a locked/denied keychain is an environment state
        // the user resolves (unlock keychain / restart), not a bug. No Sentry.
        | MuniError::KeychainUnavailable
        // Planned degradation: the partial pasted and `dictation_completed`
        // already counts it (degraded). No Sentry capture.
        | MuniError::DeepgramPartialRecovered
        // Feature 035 — a rejected provider key is a user-config issue (the
        // key expired or was revoked), not a bug. No Sentry capture.
        | MuniError::DeepgramKeyRejected
        | MuniError::GladiaKeyRejected
        | MuniError::GroqKeyRejected => true,

        // Unexpected handled errors → worth a scrubbed Sentry capture. A
        // malformed provider response, a UI-bug-shaped empty key / unknown
        // service, a missing bundled cleanup-prompt resource, or running on an
        // unsupported platform all indicate something is actually wrong.
        MuniError::DeepgramInvalidResponse
        | MuniError::GroqInvalidResponse
        | MuniError::GeminiInvalidResponse
        | MuniError::GladiaInvalidResponse
        | MuniError::CleanupPromptMissing
        | MuniError::CleanupPromptInvalid
        | MuniError::CleanupPromptWriteFailed { .. }
        | MuniError::PlatformUnsupported
        | MuniError::EmptyApiKey
        | MuniError::KeychainWriteFailed { .. }
        // Plan 039 task 37 — a CoreGraphics event-creation failure at paste
        // time shouldn't normally happen; it points at a system-resource fault
        // worth a scrubbed Sentry capture.
        | MuniError::PasteInjectionFailed
        | MuniError::UnknownSecretsService { .. } => false,
    }
}

/// Feature 033 — route a presented error to telemetry. Unexpected handled errors
/// are captured to Sentry as a non-fatal (the active-DSN/throttle/sampling gates
/// in `telemetry::before_send` apply automatically; this is a no-op when Sentry
/// isn't initialised). Expected degradations are left to the PostHog health
/// events already emitted on the session path — capturing them here would be
/// noise and, for the terminal/fallback cases, a double-count.
///
/// Only the message is captured (never the `MuniError` `Debug`, which can carry
/// a provider response body via e.g. `GroqServerError.body`); `user_message`
/// omits content fields by construction, and the Sentry `before_send` path-scrub
/// is the backstop.
fn route_to_telemetry(err: &MuniError) {
    if is_expected(err) {
        return;
    }
    sentry::capture_message(&err.user_message(), sentry::Level::Warning);
}

/// Surface `err` to the user.
///
/// Routes by [`MuniError::surface`] (Feature 035) — *not* severity:
///
/// - `Notification` → a silent macOS banner; if the error
///   [`MuniError::requires_user_action_now`], the Main window is also
///   brought forward.
/// - `Toast` → an in-app `error://quiet` Sonner toast (Settings-window
///   interactions).
/// - `HudNotice` → a short chip above the HUD pill (`hud://notice`).
/// - `Silent` → log + telemetry only.
///
/// The Notification and Toast surfaces additionally emit
/// `error://navigate-to-tab` when the error has a [`SettingsTab`], so any
/// UI surface can dispatch routing without re-deriving the tab.
///
/// Failures inside Tauri's notification or emit machinery are logged at
/// `warn` and swallowed — the orchestrator's pipeline must keep running
/// even if the notification daemon is unavailable or the frontend has
/// gone away.
///
/// Thin non-generic wrapper over [`route`] for callers holding a
/// default-runtime [`AppHandle`].
pub fn present(app: &AppHandle, err: &MuniError) {
    route(app, err);
}

/// Deliver a loud error as a system notification.
///
/// Two macOS paths, picked at runtime by whether we're running inside a
/// real signed app bundle:
///
/// - **Bundled (the shipped, notarised `Muni.app`)** →
///   `UNUserNotificationCenter`. This is Apple's current notification
///   API: the banner renders with Muni's own icon, and a click
///   foregrounds Muni rather than an AppleScript helper. It requires a
///   process with a bundle identifier — `currentNotificationCenter`
///   raises otherwise — which is exactly what distinguishes it from the
///   dev binary.
/// - **Unbundled (the `tauri dev` binary, which has no bundle id and is
///   unsigned)** → `osascript display notification`. The deprecated
///   `NSUserNotification` path (`tauri-plugin-notification` /
///   notify-rust) silently drops on modern macOS for unsigned apps, and
///   `UNUserNotificationCenter` would raise; the system-blessed
///   AppleScript helper renders regardless of signing. Trade-off: it's
///   attributed to the AppleScript helper (Script Editor) icon, and a
///   click opens Script Editor — acceptable for dev only.
#[cfg(target_os = "macos")]
fn deliver_loud(payload: &ErrorPayload) {
    if is_app_bundled() {
        deliver_via_user_notification(payload);
    } else {
        deliver_via_osascript(payload);
    }
}

/// True when the process is running inside an app bundle that carries a
/// bundle identifier (the shipped `Muni.app`). False for the bare
/// `tauri dev` executable, which has no `Info.plist` and thus no id —
/// the same condition under which `UNUserNotificationCenter` raises.
#[cfg(target_os = "macos")]
fn is_app_bundled() -> bool {
    use objc2_foundation::NSBundle;
    // `mainBundle` + `bundleIdentifier` are read-only accessors on the
    // process's main bundle; both are safe and thread-agnostic.
    NSBundle::mainBundle().bundleIdentifier().is_some()
}

/// Post a loud error through `UNUserNotificationCenter` (Muni-owned
/// notification, Muni icon, click foregrounds Muni). Authorization is
/// requested first; macOS shows the one-time permission prompt on the
/// first call and remembers the choice. If the user has denied
/// notifications we intentionally do NOT fall back to `osascript` —
/// respecting that choice is correct, and routing around it would be
/// user-hostile.
#[cfg(target_os = "macos")]
fn deliver_via_user_notification(payload: &ErrorPayload) {
    use block2::RcBlock;
    use objc2_foundation::{NSError, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
        UNUserNotificationCenter,
    };

    // Safe: `currentNotificationCenter` only raises without a bundle id,
    // which `is_app_bundled` has already ruled out before we get here.
    let center = UNUserNotificationCenter::currentNotificationCenter();

    // Request authorization (idempotent — a no-op once the user has
    // answered). Heap-allocated `RcBlock` because the completion handler
    // outlives this stack frame; we only log the outcome.
    let auth_handler = RcBlock::new(|granted: objc2::runtime::Bool, err: *mut NSError| {
        if !err.is_null() {
            // SAFETY: non-null `NSError*` handed to us by the framework.
            let message = unsafe { (*err).localizedDescription() };
            log::warn!(
                target: "error_presenter",
                "notification authorization error: {message}"
            );
        } else {
            log::debug!(
                target: "error_presenter",
                "notification authorization granted={}",
                granted.as_bool()
            );
        }
    });
    // Feature 035 — request only `Alert` (no `Sound`): every Muni error
    // notification is silent, so we never ask for the sound authorization.
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert,
        &auth_handler,
    );

    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str("Muni"));
    content.setBody(&NSString::from_str(&payload.user_message));
    // Feature 035 — no `setSound`: the banner is intentionally silent.

    // Identify the request by error kind so a repeated same-error
    // notification replaces the prior one instead of stacking duplicates.
    let id_str = notification_identifier(payload.kind);
    // Remember the tab so a click on this notification can navigate there.
    if let Some(tab) = payload.settings_tab {
        if let Ok(mut map) = LOUD_TABS.lock() {
            map.insert(id_str.clone(), tab);
        }
    }
    let identifier = NSString::from_str(&id_str);
    let request =
        UNNotificationRequest::requestWithIdentifier_content_trigger(&identifier, &content, None);

    let add_handler = RcBlock::new(|err: *mut NSError| {
        if err.is_null() {
            log::debug!(target: "error_presenter", "user notification posted");
        } else {
            // SAFETY: non-null `NSError*` handed to us by the framework.
            let message = unsafe { (*err).localizedDescription() };
            log::warn!(
                target: "error_presenter",
                "user notification rejected: {message}"
            );
        }
    });
    center.addNotificationRequest_withCompletionHandler(&request, Some(&add_handler));
}

/// Holds the app handle the notification-click handler uses to bring Muni
/// forward. Set once at startup by [`install_notification_delegate`].
#[cfg(target_os = "macos")]
static DELEGATE_APP: std::sync::OnceLock<AppHandle> = std::sync::OnceLock::new();

/// Maps a posted loud-error notification identifier → the Settings tab that
/// resolves it, so a click can re-navigate there. Notifications are coalesced
/// per error kind, so this holds at most one entry per kind (bounded).
#[cfg(target_os = "macos")]
static LOUD_TABS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, SettingsTab>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Payload for re-emitting `navigate-to-tab` on a notification click. Matches
/// the `settingsTab` field the frontend's `useErrorEvents` router reads.
#[cfg(target_os = "macos")]
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct NavigatePayload {
    settings_tab: SettingsTab,
}

/// Stable per-kind notification identifier, so a repeated same-error
/// notification replaces the prior one (dedup) and the click delegate can map
/// it back to a tab.
#[cfg(target_os = "macos")]
fn notification_identifier(kind: &str) -> String {
    format!("muni.error.{kind}")
}

/// Look up the tab a clicked notification should navigate to.
#[cfg(target_os = "macos")]
fn loud_tab_for(identifier: &str) -> Option<SettingsTab> {
    LOUD_TABS.lock().ok()?.get(identifier).copied()
}

/// Re-emit `navigate-to-tab` so a notification click lands on the tab that
/// fixes the error (the post-time emit may have been superseded by the user
/// navigating elsewhere in an already-open window).
#[cfg(target_os = "macos")]
fn emit_navigate_to_tab(app: &AppHandle, tab: SettingsTab) {
    if let Err(e) = app.emit(EVENT_NAVIGATE_TO_TAB, NavigatePayload { settings_tab: tab }) {
        log::warn!(
            target: "error_presenter",
            "emit {EVENT_NAVIGATE_TO_TAB} (click) failed: {e}"
        );
    }
}

/// Custom `UNUserNotificationCenterDelegate` so a click on a Muni
/// notification foregrounds Muni (independent of LaunchServices' default
/// action), and so banners still present while Muni is the active app.
#[cfg(target_os = "macos")]
mod notification_delegate {
    use super::{
        emit_navigate_to_tab, focus_main_window, is_app_bundled, loud_tab_for, DELEGATE_APP,
    };
    use objc2::rc::Retained;
    use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{define_class, msg_send, AnyThread};
    use objc2_user_notifications::{
        UNNotification, UNNotificationPresentationOptions, UNNotificationResponse,
        UNUserNotificationCenter, UNUserNotificationCenterDelegate,
    };
    use tauri::AppHandle;

    define_class!(
        // SAFETY: NSObject has no subclassing requirements, and `Delegate`
        // holds no ivars / does not implement `Drop`.
        #[unsafe(super(NSObject))]
        #[name = "MuniNotificationDelegate"]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl UNUserNotificationCenterDelegate for Delegate {
            // Present the banner (and list it in Notification Center) even
            // when Muni is the active app, so an actionable error is never
            // silently dropped. Feature 035 — no `Sound`: the banner shows
            // without playing a sound, matching the silent content + auth.
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &UNUserNotificationCenter,
                _notification: &UNNotification,
                completion: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                let opts = UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List;
                completion.call((opts,));
            }

            // Click → bring Muni forward. The `navigate-to-tab` event already
            // fired when the error was presented, so the window is on the
            // right tab; we only need to surface it.
            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(
                &self,
                _center: &UNUserNotificationCenter,
                response: &UNNotificationResponse,
                completion: &block2::DynBlock<dyn Fn()>,
            ) {
                if let Some(app) = DELEGATE_APP.get() {
                    // Emit the tab change BEFORE showing the window: a hidden
                    // WKWebView throttles its event loop, so queueing the
                    // navigate first lets it apply as the webview wakes, rather
                    // than flashing the old tab and switching ~1s later.
                    let id = response.notification().request().identifier().to_string();
                    if let Some(tab) = loud_tab_for(&id) {
                        emit_navigate_to_tab(app, tab);
                    }
                    focus_main_window(app);
                }
                completion.call(());
            }
        }
    );

    /// Install the delegate on the shared notification center exactly once.
    /// No-op when unbundled (the osascript path has no center, and
    /// `currentNotificationCenter` would raise without a bundle id).
    pub fn install(app: AppHandle) {
        if !is_app_bundled() {
            return;
        }
        let _ = DELEGATE_APP.set(app);
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let delegate: Retained<Delegate> = unsafe { msg_send![Delegate::alloc(), init] };
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        // `UNUserNotificationCenter.delegate` is a WEAK property — leak our
        // strong reference so the delegate lives for the process lifetime.
        core::mem::forget(delegate);
    }
}

/// Wire up the notification-click delegate. Call once at app startup on the
/// main thread. macOS-only; a no-op elsewhere.
#[cfg(target_os = "macos")]
pub fn install_notification_delegate(app: AppHandle) {
    notification_delegate::install(app);
}

#[cfg(not(target_os = "macos"))]
pub fn install_notification_delegate(_app: AppHandle) {}

/// Fallback notification for the unbundled dev binary — see
/// [`deliver_loud`] for why the AppleScript helper is used here.
#[cfg(target_os = "macos")]
fn deliver_via_osascript(payload: &ErrorPayload) {
    let body = applescript_escape(&payload.user_message);
    let script = format!("display notification \"{body}\" with title \"Muni\"");
    match std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
    {
        Ok(out) if out.status.success() => {
            log::debug!(target: "error_presenter", "osascript notification posted");
        }
        Ok(out) => {
            log::warn!(
                target: "error_presenter",
                "osascript notification rejected (status={}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Err(err) => {
            log::warn!(target: "error_presenter", "osascript invocation failed: {err}");
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn deliver_loud(payload: &ErrorPayload) {
    // Non-macOS platforms aren't shipped yet (Windows/Linux are post-v1
    // per plan §002). Log the would-be notification so the call is at
    // least visible during cross-platform development.
    log::warn!(
        target: "error_presenter",
        "loud notification not implemented on this platform: {}",
        payload.user_message
    );
}

/// Escape a string for safe interpolation into an AppleScript double-
/// quoted string literal. AppleScript escapes `\` and `"` with a
/// leading backslash; nothing else needs escaping inside `"..."`.
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Return the camelCase discriminant the frontend switches on. Kept in
/// lockstep with `MuniError`'s serde tag (`#[serde(tag = "kind")]`); a
/// single match here is easier to audit than scraping `serde_json` at
/// runtime.
pub(crate) fn kind_of(err: &MuniError) -> &'static str {
    match err {
        MuniError::InputMonitoringDenied => "inputMonitoringDenied",
        MuniError::HotkeyTapInstallFailed => "hotkeyTapInstallFailed",
        MuniError::MicrophoneDenied => "microphoneDenied",
        MuniError::NoInputDevice => "noInputDevice",
        MuniError::AudioStreamFailed { .. } => "audioStreamFailed",
        MuniError::DeepgramMissingApiKey => "deepgramMissingApiKey",
        MuniError::DeepgramConnectionFailed { .. } => "deepgramConnectionFailed",
        MuniError::DeepgramInvalidResponse => "deepgramInvalidResponse",
        MuniError::DeepgramServerError { .. } => "deepgramServerError",
        MuniError::GroqMissingApiKey => "groqMissingApiKey",
        MuniError::GroqConnectionFailed { .. } => "groqConnectionFailed",
        MuniError::GroqInvalidResponse => "groqInvalidResponse",
        MuniError::GroqServerError { .. } => "groqServerError",
        MuniError::GeminiMissingApiKey => "geminiMissingApiKey",
        MuniError::GeminiConnectionFailed { .. } => "geminiConnectionFailed",
        MuniError::GeminiInvalidResponse => "geminiInvalidResponse",
        MuniError::GeminiServerError { .. } => "geminiServerError",
        MuniError::CleanupPromptMissing => "cleanupPromptMissing",
        MuniError::AccessibilityDenied => "accessibilityDenied",
        MuniError::NothingToPaste => "nothingToPaste",
        MuniError::PlatformUnsupported => "platformUnsupported",
        MuniError::EmptyApiKey => "emptyApiKey",
        MuniError::KeychainWriteFailed { .. } => "keychainWriteFailed",
        MuniError::KeychainUnavailable => "keychainUnavailable",
        MuniError::UnknownSecretsService { .. } => "unknownSecretsService",
        MuniError::CleanupPromptInvalid => "cleanupPromptInvalid",
        MuniError::CleanupPromptWriteFailed { .. } => "cleanupPromptWriteFailed",
        MuniError::GladiaMissingApiKey => "gladiaMissingApiKey",
        MuniError::GladiaConnectionFailed { .. } => "gladiaConnectionFailed",
        MuniError::GladiaInvalidResponse => "gladiaInvalidResponse",
        MuniError::GladiaServerError { .. } => "gladiaServerError",
        MuniError::GladiaFinalizeTimeoutAfterAudio => "gladiaFinalizeTimeoutAfterAudio",
        MuniError::TranscriptionUnavailable => "transcriptionUnavailable",
        MuniError::AudioLidLoadFailed { .. } => "audioLidLoadFailed",
        MuniError::AudioLidInferenceFailed { .. } => "audioLidInferenceFailed",
        MuniError::VadLoadFailed { .. } => "vadLoadFailed",
        MuniError::VadInferenceFailed { .. } => "vadInferenceFailed",
        MuniError::ParakeetSidecarUnavailable { .. } => "parakeetSidecarUnavailable",
        MuniError::ParakeetTranscribeFailed { .. } => "parakeetTranscribeFailed",
        MuniError::DeepgramPartialRecovered => "deepgramPartialRecovered",
        MuniError::DeepgramKeyRejected => "deepgramKeyRejected",
        MuniError::GladiaKeyRejected => "gladiaKeyRejected",
        MuniError::GroqKeyRejected => "groqKeyRejected",
        MuniError::PasteInjectionFailed => "pasteInjectionFailed",
    }
}

/// Closure type used to inject error presentation into modules that don't
/// hold an `AppHandle` directly. Production wiring builds one from
/// [`present`]; tests pass a recording stub.
pub type PresentError = std::sync::Arc<dyn Fn(&MuniError) + Send + Sync + 'static>;

/// Build a [`PresentError`] that calls [`present`] against the supplied
/// app handle. Cheap to clone (`AppHandle` is `Arc`-internally).
pub fn app_handle_presenter<R: tauri::Runtime + 'static>(handle: AppHandle<R>) -> PresentError {
    std::sync::Arc::new(move |err: &MuniError| {
        // The notification + emit path is wry-runtime only in production.
        // Tests construct a `wry`-typed handle the same way; a non-wry
        // runtime would simply skip the notification — but the typed
        // generic is preserved so future cross-runtime callers compile
        // without touching this helper.
        route(&handle, err);
    })
}

/// Feature 035 — route a [`MuniError`] to its [`ErrorSurface`]. Generic over
/// the runtime so both the production wry handle and the non-generic
/// [`present`] wrapper share one routing body (they were near-duplicates
/// before). `app.try_state`/`emit` work on any runtime, so no specialisation
/// is needed.
fn route<R: tauri::Runtime>(app: &AppHandle<R>, err: &MuniError) {
    let payload = ErrorPayload::from_error(err);
    let surface = err.surface();
    log::info!(
        target: "error_presenter",
        "present kind={} severity={:?} surface={:?} tab={:?}",
        payload.kind,
        payload.severity,
        surface,
        payload.settings_tab,
    );

    route_to_telemetry(err);

    match surface {
        ErrorSurface::Notification => {
            deliver_loud(&payload);
            emit_navigate_for_tab(app, &payload);
            // Feature 035 / dogfood 2026-06-17 — a Notification kind with a
            // non-empty `hud_message` is an actionable error that fires
            // mid-dictation (a provider key missing/rejected). For those we
            // show an immediate HUD chip — in-context feedback where the user
            // is already looking — and DELIBERATELY do NOT steal focus. The
            // old focus-steal yanked the window forward, which (because macOS
            // suppresses the frontmost app's own banner) left the user on a
            // Settings tab with no visible reason for the jump. The navigate
            // event above + the persistent notification still take them to the
            // right tab when they choose to act.
            //
            // Kinds with an empty chip (platform, boot-time setup)
            // keep the focus-steal: they don't fire mid-dictation, so the chip
            // is irrelevant and the focus-steal is the only "now" signal once
            // sound was dropped.
            let chip = err.hud_message();
            if !chip.is_empty() {
                hud::show_notice(app, chip, err.hud_tone());
            } else if err.requires_user_action_now() {
                focus_main_window(app);
            }
        }
        ErrorSurface::Toast => {
            emit_quiet_event(app, &payload);
            emit_navigate_for_tab(app, &payload);
        }
        ErrorSurface::HudNotice => {
            // Non-actionable mid-dictation FYI. No toast, no navigate —
            // the chip is informational and self-dismisses.
            hud::show_notice(app, err.hud_message(), err.hud_tone());
        }
        // Internal self-healing fallback. The `log::info!` above plus the
        // telemetry routing are the entire user-visible footprint.
        ErrorSurface::Silent => {}
    }
}

/// Emit `error://quiet` so the in-app Sonner toast renders. Failures are
/// logged and swallowed — a missing frontend must not stall the pipeline.
fn emit_quiet_event<R: tauri::Runtime>(app: &AppHandle<R>, payload: &ErrorPayload) {
    if let Err(e) = app.emit(EVENT_ERROR_QUIET, payload) {
        log::warn!(
            target: "error_presenter",
            "emit {EVENT_ERROR_QUIET} failed: {e}"
        );
    }
}

/// Emit `error://navigate-to-tab` when the error has an associated
/// [`SettingsTab`], so any UI surface can route there without re-deriving
/// the tab from the kind. No-op when the error has no tab.
fn emit_navigate_for_tab<R: tauri::Runtime>(app: &AppHandle<R>, payload: &ErrorPayload) {
    if payload.settings_tab.is_none() {
        return;
    }
    if let Err(e) = app.emit(EVENT_NAVIGATE_TO_TAB, payload) {
        log::warn!(
            target: "error_presenter",
            "emit {EVENT_NAVIGATE_TO_TAB} failed: {e}"
        );
    }
}

/// Bring the `main` Tauri window forward (show + unminimize + focus).
/// Mirrors `tray::focus_main_window`'s sequencing — kept independent
/// so this module doesn't depend on the tray module's internals.
fn focus_main_window<R: tauri::Runtime>(app: &AppHandle<R>) {
    let Some(window) = app.get_webview_window("main") else {
        log::warn!(target: "error_presenter", "main window not found for auto-focus");
        return;
    };
    if let Err(err) = window.show() {
        log::warn!(target: "error_presenter", "main window show failed: {err}");
    }
    if let Err(err) = window.unminimize() {
        log::debug!(target: "error_presenter", "main window unminimize: {err}");
    }
    if let Err(err) = window.set_focus() {
        log::warn!(target: "error_presenter", "main window focus failed: {err}");
    }
}

/// No-op presenter for tests and early-boot wiring that doesn't need a
/// real notification path.
pub fn noop_presenter() -> PresentError {
    std::sync::Arc::new(|_| {})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_kind_matches_serde_tag_for_every_variant() {
        // Spot-check that `kind_of` agrees with serde's camelCase tag.
        // A divergence would mean the frontend switch on `kind` reads
        // a different value than the one the presenter sends.
        let cases = [
            (MuniError::InputMonitoringDenied, "inputMonitoringDenied"),
            (MuniError::HotkeyTapInstallFailed, "hotkeyTapInstallFailed"),
            (MuniError::MicrophoneDenied, "microphoneDenied"),
            (MuniError::NoInputDevice, "noInputDevice"),
            (MuniError::DeepgramMissingApiKey, "deepgramMissingApiKey"),
            (MuniError::GroqMissingApiKey, "groqMissingApiKey"),
            (MuniError::AccessibilityDenied, "accessibilityDenied"),
            (MuniError::NothingToPaste, "nothingToPaste"),
            (MuniError::PlatformUnsupported, "platformUnsupported"),
            (MuniError::EmptyApiKey, "emptyApiKey"),
            (MuniError::CleanupPromptMissing, "cleanupPromptMissing"),
            (MuniError::CleanupPromptInvalid, "cleanupPromptInvalid"),
            (
                MuniError::DeepgramInvalidResponse,
                "deepgramInvalidResponse",
            ),
            (MuniError::GroqInvalidResponse, "groqInvalidResponse"),
        ];
        for (err, expected_kind) in cases {
            assert_eq!(kind_of(&err), expected_kind, "{err:?}");
            // Crosscheck the serde tag for the unit variants.
            let json = serde_json::to_value(&err).unwrap();
            assert_eq!(
                json.get("kind").and_then(|v| v.as_str()),
                Some(expected_kind),
                "{err:?}"
            );
        }
    }

    #[test]
    fn unexpected_handled_errors_are_not_expected() {
        // These point at a bug / packaging / environment fault — worth a
        // scrubbed Sentry capture, so they must classify as NOT expected.
        let unexpected = [
            MuniError::DeepgramInvalidResponse,
            MuniError::GroqInvalidResponse,
            MuniError::GeminiInvalidResponse,
            MuniError::GladiaInvalidResponse,
            MuniError::CleanupPromptMissing,
            MuniError::CleanupPromptInvalid,
            MuniError::CleanupPromptWriteFailed { reason: "x".into() },
            MuniError::PlatformUnsupported,
            MuniError::EmptyApiKey,
            MuniError::KeychainWriteFailed { reason: "x".into() },
            MuniError::UnknownSecretsService {
                service: "x".into(),
            },
        ];
        for err in unexpected {
            assert!(!is_expected(&err), "{err:?} should route to Sentry");
        }
    }

    #[test]
    fn routine_degradations_are_expected() {
        // Routine transient/degradation paths must NOT raise a Sentry error.
        let expected = [
            MuniError::DeepgramConnectionFailed { reason: "x".into() },
            MuniError::GroqConnectionFailed { reason: "x".into() },
            MuniError::GroqServerError {
                status: 503,
                body: "boom".into(),
            },
            MuniError::NothingToPaste,
            MuniError::GladiaConnectionFailed { reason: "x".into() },
            MuniError::AudioLidLoadFailed { reason: "x".into() },
            MuniError::ParakeetTranscribeFailed { reason: "x".into() },
        ];
        for err in expected {
            assert!(is_expected(&err), "{err:?} should stay off Sentry");
        }
    }

    #[test]
    fn terminal_and_fallback_failures_are_expected_to_avoid_double_count() {
        // These are already counted by `dictation_failed` / `fallback_fired`
        // PostHog events on the session path — they must classify as expected
        // so the presenter doesn't also raise a Sentry error (no double-count).
        assert!(is_expected(&MuniError::TranscriptionUnavailable));
        assert!(is_expected(&MuniError::GladiaFinalizeTimeoutAfterAudio));
    }

    #[test]
    fn applescript_escape_neutralises_quotes_and_backslashes() {
        // The dev-only osascript path interpolates the user message into a
        // double-quoted AppleScript literal. An unescaped `"` would break
        // the script (or allow injecting trailing AppleScript); `\` must be
        // doubled so it can't escape the closing quote. Backslash first,
        // then quote — order matters so the inserted escapes aren't
        // re-escaped.
        assert_eq!(applescript_escape("plain"), "plain");
        assert_eq!(applescript_escape(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(applescript_escape(r"a\b"), r"a\\b");
        assert_eq!(applescript_escape(r#"\""#), r#"\\\""#);
    }

    #[test]
    fn payload_carries_user_message_and_settings_tab() {
        let err = MuniError::DeepgramMissingApiKey;
        let payload = ErrorPayload::from_error(&err);
        assert_eq!(payload.severity, ErrorSeverity::Loud);
        assert_eq!(payload.settings_tab, Some(SettingsTab::ApiKeys));
        assert!(payload.user_message.contains("Deepgram"));
    }

    #[test]
    fn payload_serialises_to_camel_case_with_null_tab() {
        let err = MuniError::DeepgramConnectionFailed {
            reason: "boom".into(),
        };
        let payload = ErrorPayload::from_error(&err);
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["kind"], "deepgramConnectionFailed");
        assert_eq!(json["severity"], "quiet");
        assert!(json["userMessage"].as_str().unwrap().contains("Deepgram"));
        assert!(json["settingsTab"].is_null());
    }

    #[test]
    fn key_rejected_kinds_round_trip_through_kind_of() {
        // Feature 035 — the three new rejected-key kinds must map to the same
        // camelCase tag serde emits, or a frontend switch on `kind` would miss
        // the notification's click-to-route payload.
        let cases = [
            (MuniError::DeepgramKeyRejected, "deepgramKeyRejected"),
            (MuniError::GladiaKeyRejected, "gladiaKeyRejected"),
            (MuniError::GroqKeyRejected, "groqKeyRejected"),
        ];
        for (err, expected_kind) in cases {
            assert_eq!(kind_of(&err), expected_kind, "{err:?}");
            let json = serde_json::to_value(&err).unwrap();
            assert_eq!(
                json.get("kind").and_then(|v| v.as_str()),
                Some(expected_kind),
                "{err:?}",
            );
        }
    }

    #[test]
    fn key_rejected_kinds_are_expected_and_stay_off_sentry() {
        // A rejected provider key is a user-config issue (expired/revoked key),
        // not a bug — so it must NOT raise a Sentry non-fatal.
        for err in [
            MuniError::DeepgramKeyRejected,
            MuniError::GladiaKeyRejected,
            MuniError::GroqKeyRejected,
        ] {
            assert!(is_expected(&err), "{err:?} should stay off Sentry");
        }
    }
}
