//! Tauri IPC commands surfaced to the React layer.
//!
//! Phase boundaries:
//! - Phase 0: `ping` (smoke test).
//! - Phase 6: `set_tray_state` (tray icon nudge from UI).
//! - Phase 8: `settings_*`, `secrets_*`, `validate_api_key`,
//!   `cleanup_prompt_*` — wire the React Settings routes to the
//!   Tauri Store, OS keyring, and the bundled `CleanupPrompt.md`.
//! - Phase 9: `request_microphone_access`, `microphone_status`,
//!   `prompt_accessibility`, `is_accessibility_trusted`,
//!   `open_system_settings`, `complete_onboarding` — back the
//!   first-run wizard.
//!
//! Naming convention: every command uses
//! `#[tauri::command(rename_all = "camelCase")]` so the JS callsite
//! reads `invoke("settingsGet", { key })` and the Rust signature can
//! stay snake_case (per R10).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, Runtime, State};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_store::StoreExt;

use crate::about_me::AboutMe;
use crate::audio::{self, InputDevice};
use crate::error::MuniError;
use crate::history_store::{DictationRecord, HistoryStore};
use crate::hotkey::HotkeyTriggerState;
use crate::hotkey_binding::{validate_pair, DictationBinding, RepasteBinding};
use crate::my_words::{MyWords, MyWordsSnapshot};
use crate::permissions::{self, InputMonitoringStatus, MicrophoneStatus, SystemSettingsTarget};
use crate::prompt::CleanupPrompt;
use crate::secrets;
use crate::session::{DeepgramPool, MicSilencedFlag, SessionStateTracker};
use crate::settings;
use crate::tray::{set_state, TrayState};
use crate::usage_store::{ModelSummary, MonthlySummary, PriceRow, UsageStore};
use crate::user_prompt::UserPrompt;
use crate::vocabulary::{Vocabulary, VocabularySnapshot};

/// Phase 0 placeholder: a single command so the IPC surface compiles end-to-end.
/// Real commands land per-phase (Phase 1 hotkey, Phase 8 settings/secrets, etc.).
#[tauri::command(rename_all = "camelCase")]
pub fn ping() -> &'static str {
    "pong"
}

/// Update the tray icon to reflect a session state transition.
///
/// Called from the React layer when the orchestrator emits
/// `session://state-changed`. Rust also calls [`set_state`] directly from
/// `session.rs` so the tray stays in sync even if the frontend has gone
/// away (window closed, background app) — this command exists primarily to
/// let the future Pause/Resume UI nudge the tray without rewiring the
/// state machine.
#[tauri::command(rename_all = "camelCase")]
pub fn set_tray_state(app: tauri::AppHandle, state: String) -> Result<(), String> {
    let parsed =
        TrayState::from_wire(&state).ok_or_else(|| format!("unknown tray state: {state}"))?;
    set_state(&app, parsed);
    Ok(())
}

/// Return the orchestrator's current [`crate::session::SessionState`] as
/// its `as_wire()` string.
///
/// Invoked from JS as `invoke<string>("get_session_state")` — the
/// `rename_all = "camelCase"` attribute only renames arguments, not the
/// command name. Used by the HUD's `useSessionState` hook to seed its
/// initial value on mount. The `session://state-changed` event is
/// pub/sub with no replay, so a WebView that mounts AFTER the
/// orchestrator's first transition (the common case on a fresh launch
/// where the HUD webview hasn't run any JS until the OS shows the
/// window) would otherwise default to `idle` and never recover the
/// in-flight press. Reading the live tracker on mount closes that gap.
#[tauri::command(rename_all = "camelCase")]
pub fn get_session_state(tracker: State<'_, Arc<SessionStateTracker>>) -> String {
    tracker.get().as_wire().to_string()
}

// --- Audio devices IPC -----------------------------------------------------

/// Probe cpal for the current set of input devices. Snapshot semantics:
/// callers should treat the result as a per-call probe, not a
/// long-lived list (USB plug/unplug mutates this set). The tray menu
/// reads this once at build time; a future settings panel will re-call
/// it whenever the picker opens.
/// `async` so the CoreAudio device probe runs off the main thread
/// (plan 039 task 43).
#[tauri::command(rename_all = "camelCase")]
pub async fn enumerate_input_devices() -> Vec<InputDevice> {
    audio::enumerate_input_devices()
}

// --- Settings IPC ----------------------------------------------------------

/// Read a settings value, falling back to the Swift v1 default when the
/// store has no entry yet. Returns `Value::Null` for unknown keys (the
/// frontend treats a null response as "use UI default").
#[tauri::command(rename_all = "camelCase")]
pub fn settings_get<R: Runtime>(app: AppHandle<R>, key: String) -> Result<Value, String> {
    if !settings::is_allowed_key(&key) {
        return Err(format!("unknown settings key: {key}"));
    }
    let store = app
        .store(settings::SETTINGS_FILE)
        .map_err(|e| format!("open store: {e}"))?;
    Ok(store
        .get(&key)
        .or_else(|| settings::default_for(&key))
        .unwrap_or(Value::Null))
}

/// Write a settings value. Rejects keys that aren't on the allowlist so
/// a compromised webview can't pollute the store with arbitrary state, then
/// runs the per-key value validation (type, size, sanitizers, and the
/// binding-key deny-list) in [`settings::validate_set_value`] — plan 039
/// task 42. The value actually persisted is the validator's output, which
/// for `my_words.entries` / `vocabulary.entries` is the sanitized form.
#[tauri::command(rename_all = "camelCase")]
pub fn settings_set<R: Runtime>(
    app: AppHandle<R>,
    key: String,
    value: Value,
) -> Result<(), String> {
    if !settings::is_allowed_key(&key) {
        return Err(format!("unknown settings key: {key}"));
    }
    let value = settings::validate_set_value(&key, value)?;
    let store = app
        .store(settings::SETTINGS_FILE)
        .map_err(|e| format!("open store: {e}"))?;
    store.set(&key, value.clone());
    store.save().map_err(|e| format!("save store: {e}"))?;
    // Notify any mounted `useSetting` hook (this window or another) so it
    // reflects the new value without a refetch. Best-effort: a telemetry-grade
    // failure to emit must not fail the write that already persisted.
    let _ = app.emit(
        settings::EVENT_SETTINGS_CHANGED,
        json!({ "key": key, "value": value }),
    );
    // A telemetry consent opt-OUT must take effect immediately, not at next
    // launch — update the live gate the capture paths consult.
    if let Some(enabled) = value.as_bool() {
        crate::telemetry::apply_consent_change(&key, enabled);
    }
    Ok(())
}

// --- Secrets IPC -----------------------------------------------------------

/// Save a secret to the OS keyring. Returns the typed
/// [`MuniError`] serialised to JSON so the frontend can switch on
/// `kind` (matching the rest of our error pipeline).
///
/// Deepgram setters/deleters additionally invalidate the warm WS
/// pool so the next press resolves the current key — without this,
/// a "Remove saved key" + immediate press would happily stream
/// through the previously-authenticated parked socket.
///
/// Fires [`secrets::EVENT_SECRETS_CHANGED`] on success so any
/// React-side `useSecretProbe` (notably the one in Settings → API
/// Keys, which is mounted-but-hidden during onboarding) refreshes its
/// "Saved / Not set" pill instead of staying stuck on the value it
/// read at app launch.
#[tauri::command(rename_all = "camelCase")]
pub async fn secrets_set_deepgram(
    value: String,
    app: AppHandle,
    pool: State<'_, Arc<DeepgramPool>>,
) -> Result<(), MuniError> {
    secrets::set(secrets::DEEPGRAM_ACCOUNT, &value)?;
    pool.clear_parked().await;
    emit_secrets_changed(&app);
    Ok(())
}

#[tauri::command(rename_all = "camelCase")]
pub fn secrets_set_groq(value: String, app: AppHandle) -> Result<(), MuniError> {
    secrets::set(secrets::GROQ_ACCOUNT, &value)?;
    emit_secrets_changed(&app);
    // Feature 016 — boot warmup is a no-op when no Groq key is
    // present; the user's first save is what makes the warmup
    // resolvable. Fire here so the prompt-prefix cache is hot
    // before their first hotkey press.
    crate::groq_warmup::warmup_from_app(&app, crate::groq_warmup::WarmupTrigger::KeySaved);
    Ok(())
}

/// Feature 003 — persist a Gemini text-LID API key. Mirrors the Groq
/// pattern: write to the OS keychain, then broadcast
/// `secrets://changed` so any mounted `useSecretProbe` re-reads. The
/// Gemini classifier resolves the key on every press, so the change
/// takes effect immediately without restarting.
#[tauri::command(rename_all = "camelCase")]
pub fn secrets_set_gemini(value: String, app: AppHandle) -> Result<(), MuniError> {
    secrets::set(secrets::GEMINI_ACCOUNT, &value)?;
    emit_secrets_changed(&app);
    Ok(())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn secrets_delete_deepgram(
    app: AppHandle,
    pool: State<'_, Arc<DeepgramPool>>,
) -> Result<(), MuniError> {
    secrets::delete(secrets::DEEPGRAM_ACCOUNT)?;
    pool.clear_parked().await;
    emit_secrets_changed(&app);
    Ok(())
}

#[tauri::command(rename_all = "camelCase")]
pub fn secrets_delete_groq(app: AppHandle) -> Result<(), MuniError> {
    secrets::delete(secrets::GROQ_ACCOUNT)?;
    emit_secrets_changed(&app);
    Ok(())
}

#[tauri::command(rename_all = "camelCase")]
pub fn secrets_delete_gemini(app: AppHandle) -> Result<(), MuniError> {
    secrets::delete(secrets::GEMINI_ACCOUNT)?;
    emit_secrets_changed(&app);
    Ok(())
}

/// Feature 010 — persist a Gladia Solaria-1 API key. The Gladia key is
/// consulted by the Whisper-batch fallback path (`session::attempt_gladia_fallback_transcribe`),
/// which opens a fresh `GladiaClient` on demand — no warm pool to
/// invalidate. Writes to the OS keychain then broadcasts `secrets://changed`.
#[tauri::command(rename_all = "camelCase")]
pub fn secrets_set_gladia(value: String, app: AppHandle) -> Result<(), MuniError> {
    secrets::set(secrets::GLADIA_ACCOUNT, &value)?;
    emit_secrets_changed(&app);
    Ok(())
}

#[tauri::command(rename_all = "camelCase")]
pub fn secrets_delete_gladia(app: AppHandle) -> Result<(), MuniError> {
    secrets::delete(secrets::GLADIA_ACCOUNT)?;
    emit_secrets_changed(&app);
    Ok(())
}

/// `async` so the keychain probe runs off the main thread (plan 039 task 43).
#[tauri::command(rename_all = "camelCase")]
pub async fn secrets_is_gladia_set() -> bool {
    secrets::is_set(secrets::GLADIA_ACCOUNT)
}

/// Broadcast `secrets://changed` to every window. Failures are logged
/// at warn but never propagate — emit failure shouldn't surface as a
/// "save failed" error to the user when the keychain write itself
/// succeeded; the worst case is a stale probe pill that re-syncs on
/// the next mount.
fn emit_secrets_changed<R: Runtime>(app: &AppHandle<R>) {
    // Invalidate the hot-path key cache BEFORE notifying the FE so the next
    // release-path read re-resolves the just-changed keychain value (task 17).
    secrets::invalidate_cache();
    if let Err(e) = app.emit(secrets::EVENT_SECRETS_CHANGED, ()) {
        log::warn!(target: "secrets", "emit {} failed: {e}", secrets::EVENT_SECRETS_CHANGED);
    }
}

/// `async` so the keychain probe runs off the main thread (plan 039 task 43).
#[tauri::command(rename_all = "camelCase")]
pub async fn secrets_is_deepgram_set() -> bool {
    secrets::is_set(secrets::DEEPGRAM_ACCOUNT)
}

/// `async` so the keychain probe runs off the main thread (plan 039 task 43).
#[tauri::command(rename_all = "camelCase")]
pub async fn secrets_is_groq_set() -> bool {
    secrets::is_set(secrets::GROQ_ACCOUNT)
}

/// `async` so the keychain probe runs off the main thread (plan 039 task 43).
#[tauri::command(rename_all = "camelCase")]
pub async fn secrets_is_gemini_set() -> bool {
    secrets::is_set(secrets::GEMINI_ACCOUNT)
}

// --- API key validator ----------------------------------------------------

/// Hit the relevant per-provider endpoint with the supplied key and
/// report whether the auth header was accepted. Mirrors Swift v1's
/// `URLSessionAPIKeyValidator`.
///
/// Provider notes:
/// - Deepgram + Groq: cheap GET against a list endpoint with the
///   key in the standard `Authorization` header.
/// - Gemini: no equivalent list endpoint that doesn't burn quota.
///   Instead we run a one-token classification against a hardcoded
///   "Hello." prompt — any successful 2xx response means the key
///   works, regardless of the returned label. Cost per call is
///   ~$0.00004 on the paid tier and free on the free tier.
#[tauri::command(rename_all = "camelCase")]
pub async fn validate_api_key(service: String, key: String) -> Result<bool, String> {
    if key.is_empty() {
        return Err("API key is empty.".to_string());
    }
    match service.as_str() {
        "deepgram" => {
            validate_via_get(
                "https://api.deepgram.com/v1/projects",
                &format!("Token {key}"),
            )
            .await
        }
        "groq" => {
            validate_via_get(
                "https://api.groq.com/openai/v1/models",
                &format!("Bearer {key}"),
            )
            .await
        }
        "gemini" => validate_gemini_key(&key).await,
        "gladia" => validate_gladia_key(&key).await,
        other => Err(format!("unknown service: {other}")),
    }
}

/// Feature 010 — validate a Gladia key by issuing a single
/// `POST /v2/live` with a minimal valid config. 2xx → ok, 401/403 →
/// bad key. Per `docs/research/08-gladia-evaluation.md` §3.2 the
/// billing semantics of a POST that immediately drops the WS are
/// ambiguous and may incur a single billing tick; the operator-run
/// `gladia_tls_probe.rs` measures the exact cost. Acceptable cost for
/// a one-shot key check.
async fn validate_gladia_key(key: &str) -> Result<bool, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let body = serde_json::json!({
        "encoding": "wav/pcm",
        "sample_rate": 16000,
        "bit_depth": 16,
        "channels": 1,
    });
    let resp = client
        .post("https://api.gladia.io/v2/live")
        .header("x-gladia-key", key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("transport: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(true);
    }
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Ok(false);
    }
    Err(format!("Gladia returned HTTP {status}"))
}

/// Shared helper for the GET-with-auth-header validators.
async fn validate_via_get(url: &str, auth_header: &str) -> Result<bool, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let resp = client
        .get(url)
        .header(reqwest::header::AUTHORIZATION, auth_header)
        .send()
        .await
        .map_err(|e| format!("transport: {e}"))?;
    Ok(resp.status().is_success())
}

/// Validate a Gemini key by issuing a single LID classification.
/// Returns `Ok(true)` for any successful response (2xx), `Ok(false)`
/// for the typical bad-key signals (`GeminiServerError` 401/403), and
/// `Err(...)` for anything else (transport error, parse failure)
/// so the UI can show a "couldn't reach" message instead of a
/// silently-rejected key.
async fn validate_gemini_key(key: &str) -> Result<bool, String> {
    let client = crate::gemini_lid::GeminiLidClient::new()
        .map_err(|e| format!("build client: {}", e.user_message()))?;
    match client.classify_with_key("Hello.", key).await {
        Ok(_) => Ok(true),
        Err(MuniError::GeminiServerError { status, .. }) if status == 401 || status == 403 => {
            Ok(false)
        }
        Err(MuniError::GeminiMissingApiKey) => Ok(false),
        Err(err) => Err(err.user_message()),
    }
}

// --- Cleanup prompt IPC ----------------------------------------------------

/// Read the active cleanup-prompt body — override file if present, else
/// the bundled fallback. Returns the same string the orchestrator would
/// see on the next press.
#[tauri::command(rename_all = "camelCase")]
pub fn cleanup_prompt_get(prompt: State<'_, Arc<CleanupPrompt>>) -> Result<String, MuniError> {
    prompt.text()
}

/// Persist a user override and bust the orchestrator's cache so the
/// next press picks up the new prompt without restarting.
///
/// Fires the cleanup warm-up on success so Groq's prompt-prefix
/// cache is re-seeded against the new bytes before the user's next
/// real press. The brainstorm flagged this as the "isolated call
/// site easy to remove later when the prompt becomes non-editable"
/// — keep this fire-and-forget; do not push it into
/// `CleanupPrompt::save_override` (would couple prompt module to
/// the warmup helper).
#[tauri::command(rename_all = "camelCase")]
pub fn cleanup_prompt_save(
    app: AppHandle,
    prompt: State<'_, Arc<CleanupPrompt>>,
    text: String,
) -> Result<(), MuniError> {
    prompt.save_override(&text)?;
    crate::groq_warmup::warmup_from_app(&app, crate::groq_warmup::WarmupTrigger::PromptEdit);
    Ok(())
}

/// Drop the user override (returning the bundled prompt) and bust the
/// orchestrator's cache. Idempotent.
#[tauri::command(rename_all = "camelCase")]
pub fn cleanup_prompt_revert(prompt: State<'_, Arc<CleanupPrompt>>) -> Result<(), MuniError> {
    prompt.revert_override()
}

// --- Feature 013: My Words IPC --------------------------------------------

/// Read the current My Words snapshot — the toggle + the full list of
/// `{trigger, replacement}` entries. Drives the Settings → My Words pane
/// on mount.
#[tauri::command(rename_all = "camelCase")]
pub fn my_words_get(state: State<'_, Arc<MyWords>>) -> Result<MyWordsSnapshot, String> {
    Ok(state.snapshot())
}

/// Persist a new My Words snapshot. Atomic from the press path's view —
/// the in-memory snapshot is swapped only after the store write succeeds.
#[tauri::command(rename_all = "camelCase")]
pub fn my_words_save<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, Arc<MyWords>>,
    snapshot: MyWordsSnapshot,
) -> Result<(), String> {
    // Feature 033 (Phase 3, task 23) — feature-usage signal on a successful
    // save: the resulting entry COUNT only (a number), never any word text.
    // Fire-and-forget after the write succeeds; a no-op when analytics is off.
    let entry_count = snapshot.entries.len();
    state.save(&app, snapshot)?;
    crate::telemetry::emit_event(crate::telemetry::events::my_words_changed(entry_count));
    Ok(())
}

// --- Feature 015: Vocabulary soft-bias IPC ---------------------------------

/// Read the current vocabulary snapshot — the toggle + the full list
/// of `{term, note}` entries. Drives the Vocabulary pane on mount.
#[tauri::command(rename_all = "camelCase")]
pub fn vocabulary_get(state: State<'_, Arc<Vocabulary>>) -> Result<VocabularySnapshot, String> {
    Ok(state.snapshot())
}

/// Persist a new vocabulary snapshot. Trims terms/notes, drops empty
/// terms, and enforces the entry/term/note caps. The in-memory snapshot
/// is swapped only after the store write succeeds.
///
/// Fires the cleanup warm-up on success so Groq's prompt-prefix
/// cache is re-seeded against the new vocabulary block before the
/// user's next real press.
#[tauri::command(rename_all = "camelCase")]
pub fn vocabulary_save<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, Arc<Vocabulary>>,
    snapshot: VocabularySnapshot,
) -> Result<(), String> {
    state.save(&app, snapshot)?;
    crate::groq_warmup::warmup_from_app(&app, crate::groq_warmup::WarmupTrigger::Vocabulary);
    Ok(())
}

// --- Feature 014: About Me IPC --------------------------------------------

/// Read the current About Me body — the free-form vocabulary context
/// that gets prepended to the cleanup system prompt when non-empty.
#[tauri::command(rename_all = "camelCase")]
pub fn about_me_get(state: State<'_, Arc<AboutMe>>) -> Result<String, String> {
    Ok(state.text())
}

/// Persist a new About Me body. Trims trailing whitespace and rejects
/// inputs over [`crate::about_me::MAX_LEN`] chars; the disk write
/// happens before the in-memory swap so a failed write leaves the
/// press path on the previous value.
///
/// Fires the cleanup warm-up on success so Groq's prompt-prefix
/// cache is re-seeded against the new About Me block before the
/// user's next real press.
#[tauri::command(rename_all = "camelCase")]
pub fn about_me_save<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, Arc<AboutMe>>,
    text: String,
) -> Result<(), String> {
    state.save(&app, text)?;
    crate::groq_warmup::warmup_from_app(&app, crate::groq_warmup::WarmupTrigger::AboutMe);
    Ok(())
}

// --- User-authored cleanup "preferences" prompt IPC -----------------------

/// Read the current user-prompt body — the free-form preferences block
/// that gets appended after the bundled cleanup body when non-empty and
/// takes precedence over the system prompt where they conflict.
#[tauri::command(rename_all = "camelCase")]
pub fn user_prompt_get(state: State<'_, Arc<UserPrompt>>) -> Result<String, String> {
    Ok(state.text())
}

/// Persist a new user-prompt body. Trims trailing whitespace and rejects
/// inputs over [`crate::user_prompt::MAX_LEN`] chars; the disk write
/// happens before the in-memory swap so a failed write leaves the
/// press path on the previous value.
///
/// Fires the cleanup warm-up on success so Groq's prompt-prefix cache
/// is re-seeded against the new user-prompt block before the user's
/// next real press.
#[tauri::command(rename_all = "camelCase")]
pub fn user_prompt_save<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, Arc<UserPrompt>>,
    text: String,
) -> Result<(), String> {
    state.save(&app, text)?;
    crate::groq_warmup::warmup_from_app(&app, crate::groq_warmup::WarmupTrigger::UserPrompt);
    Ok(())
}

// --- Phase 9: Onboarding permission probes --------------------------------

/// Read the current microphone authorisation status without prompting.
/// Powers the "Granted / Denied / Not asked yet" indicator in the wizard
/// and the post-`requestAccess` re-render.
#[tauri::command(rename_all = "camelCase")]
pub fn microphone_status() -> MicrophoneStatus {
    permissions::microphone_status()
}

/// Trigger the macOS microphone prompt (or report the existing decision
/// when one already exists). Resolves to `true` only when the user
/// granted access this turn or had previously granted it.
#[tauri::command(rename_all = "camelCase")]
pub async fn request_microphone_access() -> bool {
    permissions::request_microphone().await
}

/// Pure status read — does NOT raise the system Accessibility prompt.
/// The wizard polls this every 500ms after firing
/// [`prompt_accessibility`] so we notice when the user grants the
/// permission without us having to wait for an OS callback.
#[tauri::command(rename_all = "camelCase")]
pub fn is_accessibility_trusted() -> bool {
    permissions::is_accessibility_trusted()
}

/// Show the macOS Accessibility prompt (the one with the "Open System
/// Settings" button). Returns whether the process is trusted at the
/// instant of the call — the prompt itself is asynchronous, so callers
/// must subsequently poll [`is_accessibility_trusted`].
#[tauri::command(rename_all = "camelCase")]
pub fn prompt_accessibility() -> bool {
    permissions::prompt_accessibility()
}

/// Read the current Input Monitoring (kIOHIDRequestTypeListenEvent)
/// authorisation without prompting.
#[tauri::command(rename_all = "camelCase")]
pub fn input_monitoring_status() -> InputMonitoringStatus {
    permissions::input_monitoring_status()
}

/// Trigger the macOS "Keystroke Receiving" prompt and return the
/// resulting status.
///
/// Implementation note: `IOHIDRequestAccess(kIOHIDRequestTypeListenEvent)`
/// is documented to prompt for Input Monitoring, but on recent macOS
/// (Sonoma+) it does NOT reliably surface the prompt or even register
/// the app with TCC. The only consistent way to trigger the system
/// dialog is to actually attempt to install a CGEventTap — macOS
/// catches the unauthorised install, registers the app in the
/// Privacy & Security pane, and shows the dialog as a side effect.
///
/// We delegate to `arm_hotkey_pipeline` which is exactly that install.
/// The first attempt typically fails (status is notDetermined or
/// denied), which is the expected path; the bundle is preserved so
/// the wizard's Finish step (or another button click) can retry once
/// the user grants the permission.
#[tauri::command(rename_all = "camelCase")]
pub fn request_input_monitoring(app: AppHandle) -> InputMonitoringStatus {
    crate::arm_hotkey_pipeline(&app);
    permissions::input_monitoring_status()
}

/// Open the relevant macOS System Settings pane. Restricted to a closed
/// enum (`microphone` / `accessibility` / `input-monitoring`) so a
/// compromised webview can't ask the host to launch arbitrary
/// `x-apple.systempreferences:` URLs.
#[tauri::command(rename_all = "camelCase")]
pub fn open_system_settings<R: Runtime>(app: AppHandle<R>, target: String) -> Result<(), String> {
    let parsed = SystemSettingsTarget::from_wire(&target)
        .ok_or_else(|| format!("unknown system settings target: {target}"))?;
    app.opener()
        .open_url(parsed.deep_link(), None::<&str>)
        .map_err(|err| {
            log::warn!(target: "permissions", "open {parsed:?} failed: {err}");
            err.to_string()
        })
}

// --- Phase 10: Launch-at-login --------------------------------------------

/// Register or unregister the app as a macOS Login Item (via
/// `SMAppService`, see `crate::launch_item`) and persist the preference.
/// Wired to the Settings → General switch and the onboarding step.
///
/// Idempotent: `crate::launch_item::set_enabled` no-ops when the OS state
/// already matches. Unlike the old AppleScript path, this **never triggers
/// an Automation prompt** — `SMAppService` registers the app itself, so
/// there is no second app to authorize.
///
/// `async` so the SMAppService registration + store write run off the main
/// thread (plan 039 task 43).
#[tauri::command(rename_all = "camelCase")]
pub async fn set_launch_at_login<R: Runtime>(
    app: AppHandle<R>,
    enabled: bool,
) -> Result<(), String> {
    // Dev-build guard. When the running binary lives under a `target/`
    // directory (Tauri dev runner, or a `make build` output that hasn't
    // been moved into /Applications yet), refuse to enable Launch at
    // Login: the path is unstable across rebuilds and the TCC posture for
    // an unsigned/local-signed binary across a fresh login session is
    // fragile — Manual QA §13 saw four zombie muni processes and a
    // LaunchServices `GetProcessPID` failure on one login cycle.
    // `SMAppService.register` also rejects un-code-signed bundles outright.
    //
    // Disabling is still allowed even from a dev bundle so users can
    // clean up an entry left by a prior installed build.
    if enabled && crate::current_exe_is_dev_bundle() {
        let msg = "Launch at Login is disabled in development builds. \
                   Install Muni.app to /Applications first, then toggle from there."
            .to_string();
        log::info!(target: "autostart", "set_launch_at_login(true) refused: dev bundle");
        return Err(msg);
    }

    crate::launch_item::set_enabled(enabled).map_err(|e| {
        log::warn!(target: "autostart", "set_launch_at_login({enabled}) failed: {e}");
        e
    })?;
    log::info!(target: "autostart", "set_launch_at_login: {enabled}");

    let store = app
        .store(settings::SETTINGS_FILE)
        .map_err(|e| format!("open store: {e}"))?;
    store.set(settings::KEY_GENERAL_LAUNCH_AT_LOGIN, json!(enabled));
    // Reaching this command means the app is now on the SMAppService path,
    // so the boot-time legacy-AppleScript cleanup must not fire for this
    // install (it would otherwise osascript-probe System Events on a fresh
    // user whose `launch_at_login` pref is true, surfacing a needless
    // Automation prompt). Marking it migrated here closes that hole.
    store.set(settings::KEY_GENERAL_LAUNCH_ITEM_MIGRATED, json!(true));
    store.save().map_err(|e| format!("save store: {e}"))?;
    Ok(())
}

/// Read the current OS launch-at-login state so the General toggle reflects
/// reality on mount — even when the user flipped Muni off directly in System
/// Settings → Login Items (drift the stored preference wouldn't catch).
///
/// `SMAppService.status` is a pure read that **never prompts**, so unlike the
/// old AppleScript probe there is no onboarding guard to tiptoe around: the
/// hidden Main webview pre-rendering `SettingsGeneral` during the wizard can
/// call this freely.
#[tauri::command(rename_all = "camelCase")]
pub fn is_launch_at_login_enabled() -> bool {
    crate::launch_item::is_enabled()
}

/// Raw read of the persisted `general.launch_at_login` preference, with
/// **no** default-merge (unlike `settings_get`). Returns `None` only when
/// the key has genuinely never been written — the store's default-value
/// merge would otherwise make an explicit opt-out indistinguishable from
/// "never touched" (both resolve to `false` via `settings::default_for`).
///
/// Plan 039 task 39(c): the onboarding wizard's `LaunchAtLoginStep` calls
/// this before running its default-on lock-in, so a user who explicitly
/// opted out, then quit mid-wizard (or the wizard re-runs before
/// `complete_onboarding` fires), never gets that opt-out silently
/// overwritten by a fresh mount's default-on persist.
#[tauri::command(rename_all = "camelCase")]
pub fn get_stored_launch_at_login_pref<R: Runtime>(app: AppHandle<R>) -> Option<bool> {
    let store = app.store(settings::SETTINGS_FILE).ok()?;
    store
        .get(settings::KEY_GENERAL_LAUNCH_AT_LOGIN)
        .and_then(|v| v.as_bool())
}

// --- Feature 037: configurable hotkeys ------------------------------------

/// Effective re-paste binding for cross-binding collision checks: the stored
/// value, or the shipped default when the key was never written. An explicit
/// `null` (user cleared it) reads back as `None` (disabled — can't collide).
fn effective_repaste_binding<R: Runtime>(app: &AppHandle<R>) -> Option<RepasteBinding> {
    // Plan 039 task 49(a): one shared resolver semantic with the boot loader —
    // absent → enabled default; null / malformed / structurally-invalid →
    // disabled. Previously this degraded a malformed value to the *default*
    // (enabled), disagreeing with boot's degrade-to-disabled; unified here.
    let stored = app
        .store(settings::SETTINGS_FILE)
        .ok()
        .and_then(|store| store.get(settings::KEY_HOTKEY_REPASTE_BINDING));
    crate::resolve_repaste_binding(stored)
}

/// Persist and **live-apply** the main dictation trigger.
///
/// Rust is the validation source of truth (the frontend only mirrors it): the
/// binding must be structurally valid *and* must not collide with the current
/// re-paste chord. On success the value is written to the store, `settings://
/// changed` is emitted so any mounted `useSetting` refreshes, and the shared
/// [`HotkeyTriggerState`] atomics are updated in place — the leaked flags-tap
/// picks up the new mask/button on its next event, with **no restart and no
/// tap reinstall**.
#[tauri::command(rename_all = "camelCase")]
pub fn set_dictation_hotkey<R: Runtime>(
    app: AppHandle<R>,
    binding: DictationBinding,
    trigger: State<'_, Arc<HotkeyTriggerState>>,
) -> Result<(), String> {
    binding.validate().map_err(|e| e.to_string())?;
    validate_pair(&binding, effective_repaste_binding(&app).as_ref()).map_err(|e| e.to_string())?;

    let value = serde_json::to_value(&binding).map_err(|e| format!("serialize binding: {e}"))?;
    let store = app
        .store(settings::SETTINGS_FILE)
        .map_err(|e| format!("open store: {e}"))?;
    store.set(settings::KEY_HOTKEY_DICTATION_BINDING, value.clone());
    store.save().map_err(|e| format!("save store: {e}"))?;
    let _ = app.emit(
        settings::EVENT_SETTINGS_CHANGED,
        json!({ "key": settings::KEY_HOTKEY_DICTATION_BINDING, "value": value }),
    );

    trigger.apply(&binding);
    // Feature 038 — a keyed binding is driven by the global-shortcut controller
    // (registered/unregistered here), a modifier-only binding by the flags-tap
    // (gated via `trigger`). Together they keep exactly one edge source live.
    // The controller is only managed after `arm_hotkey_pipeline`, so tolerate
    // its absence (a rebind during onboarding, before the tap is installed).
    if let Some(controller) = app.try_state::<Arc<crate::DictationShortcutController>>() {
        controller.apply(&binding);
    }
    log::info!(
        target: "hotkey",
        "dictation hotkey rebound to {} (mask+button {:?})",
        binding.label(),
        trigger.snapshot()
    );
    Ok(())
}

/// Effective dictation binding for cross-binding collision checks: the stored
/// value, or the shipped default when the key was never written or is corrupt.
/// Mirrors `lib.rs::load_dictation_binding` — kept local so the command layer
/// has no dependency on a lib-private helper.
fn effective_dictation_binding<R: Runtime>(app: &AppHandle<R>) -> DictationBinding {
    let Ok(store) = app.store(settings::SETTINGS_FILE) else {
        return DictationBinding::default_dictation();
    };
    match store.get(settings::KEY_HOTKEY_DICTATION_BINDING) {
        Some(value) => {
            serde_json::from_value(value).unwrap_or_else(|_| DictationBinding::default_dictation())
        }
        None => DictationBinding::default_dictation(),
    }
}

/// Persist and **live-apply** the re-paste hotkey.
///
/// `binding == None` clears it (disabled). Rust is the validation source of
/// truth (the frontend only mirrors it): a supplied binding must be
/// structurally valid *and* must not collide with the current dictation chord.
/// On success the value is written to the store, `settings://changed` is
/// emitted, and the managed [`crate::RepasteController`] unregisters the old
/// accelerator and registers the new one on the main thread — **no restart**.
/// `None` unregisters only.
#[tauri::command(rename_all = "camelCase")]
pub fn set_repaste_hotkey<R: Runtime>(
    app: AppHandle<R>,
    binding: Option<RepasteBinding>,
    controller: State<'_, Arc<crate::RepasteController>>,
) -> Result<(), String> {
    if let Some(candidate) = binding.as_ref() {
        candidate.validate().map_err(|e| e.to_string())?;
    }
    validate_pair(&effective_dictation_binding(&app), binding.as_ref())
        .map_err(|e| e.to_string())?;

    let value = serde_json::to_value(&binding).map_err(|e| format!("serialize binding: {e}"))?;
    let store = app
        .store(settings::SETTINGS_FILE)
        .map_err(|e| format!("open store: {e}"))?;
    store.set(settings::KEY_HOTKEY_REPASTE_BINDING, value.clone());
    store.save().map_err(|e| format!("save store: {e}"))?;
    let _ = app.emit(
        settings::EVENT_SETTINGS_CHANGED,
        json!({ "key": settings::KEY_HOTKEY_REPASTE_BINDING, "value": value }),
    );

    controller.apply(binding.as_ref());
    match binding.as_ref() {
        Some(candidate) => {
            log::info!(target: "hotkey", "re-paste hotkey rebound to {}", candidate.label())
        }
        None => log::info!(target: "hotkey", "re-paste hotkey cleared (disabled)"),
    }
    Ok(())
}

/// Suppress or resume the dictation trigger while the Shortcuts recorder is
/// armed. Without this, holding the current chord to re-record it would also
/// fire dictation (the native flags-tap is below the webview and can't be
/// `preventDefault`-ed). The frontend calls this `true` when recording starts
/// and `false` on every end (commit, cancel, tab switch, window blur).
#[tauri::command(rename_all = "camelCase")]
pub fn hotkey_set_recording<R: Runtime>(
    app: AppHandle<R>,
    trigger: State<'_, Arc<HotkeyTriggerState>>,
    watchdog: State<'_, crate::RecordingWatchdog>,
    recording: bool,
) {
    // Plan 039 tasks 49(b)/51(c): suppress/resume every trigger source (flags-tap
    // + both keyed controllers) and manage the orphan-suppression watchdog. A
    // keyed binding registers a *consuming* global shortcut, so a flag-ignore is
    // insufficient — the controllers unregister for the recording window.
    crate::set_recording_suppression(&app, &trigger, &watchdog, recording);
}

/// Phase 11 — true once the orchestrator has detected a press that
/// produced no audible audio. Surfaced to the Permissions card so the
/// Microphone row can show a "stale, restart Muni" pill that
/// overrides the (lying) AVFoundation cache reading. Reset only by a
/// real process restart, since the cache itself can only be refreshed
/// that way.
#[tauri::command(rename_all = "camelCase")]
pub fn is_mic_likely_silenced(flag: State<'_, MicSilencedFlag>) -> bool {
    flag.is_silenced()
}

/// Phase 11 — restart Muni so the OS refreshes per-process caches that
/// live for the lifetime of the running binary.
///
/// The motivating case is macOS's `AVAuthorizationStatus`: toggling
/// Privacy & Security → Microphone while Muni is running and clicking
/// *Later* on the "Quit & Reopen" sheet leaves the cached decision
/// stuck at whatever it was at process start. The Permissions card
/// surfaces a "Restart Muni" button that calls this; the loud
/// `MicrophoneDenied` notification copy points the user here too.
///
/// `AppHandle::restart` doesn't return — it terminates the current
/// process and re-execs the binary, so the function is annotated `!`
/// in the Tauri API surface. The Result return is a Tauri-IPC
/// requirement; in practice the restart happens before the
/// `Ok(())` ever serialises back to JS.
#[tauri::command(rename_all = "camelCase")]
pub fn restart_app<R: Runtime>(app: AppHandle<R>) {
    log::info!(target: "app", "restart_app: relaunching to refresh OS permission caches");
    app.restart();
}

// --- Phase 10: History IPC ------------------------------------------------

/// Default row count when the JS caller omits `limit`. Matches the
/// `useHistory` hook's own default so the two agree on "a generous page".
const HISTORY_LIST_DEFAULT_LIMIT: i64 = 200;
/// Hard server-side cap on how many rows a single `history_list` call can
/// pull, regardless of what the webview asks for (plan 039 task 43). Bounds
/// the IPC payload so a compromised or buggy caller can't drag the whole
/// table across the bridge.
const HISTORY_LIST_MAX_LIMIT: i64 = 500;

/// Return the most recent records, newest first. `limit` is clamped
/// server-side to `[0, HISTORY_LIST_MAX_LIMIT]`; `None` defaults to
/// [`HISTORY_LIST_DEFAULT_LIMIT`]. `async` so the SQLite read runs off the
/// main thread (plan 039 task 43).
#[tauri::command(rename_all = "camelCase")]
pub async fn history_list(
    history: State<'_, Arc<HistoryStore>>,
    limit: Option<i64>,
) -> Result<Vec<DictationRecord>, String> {
    let clamped = limit
        .unwrap_or(HISTORY_LIST_DEFAULT_LIMIT)
        .clamp(0, HISTORY_LIST_MAX_LIMIT);
    history.list(Some(clamped)).map_err(|e| e.user_message())
}

/// Total record count. Cheap; safe to call on every render. `async` so the
/// SQLite read runs off the main thread (plan 039 task 43).
#[tauri::command(rename_all = "camelCase")]
pub async fn history_count(history: State<'_, Arc<HistoryStore>>) -> Result<i64, String> {
    history.count().map_err(|e| e.user_message())
}

/// Drop a single row by id. `async` so the SQLite write runs off the main
/// thread (plan 039 task 43).
#[tauri::command(rename_all = "camelCase")]
pub async fn history_delete(history: State<'_, Arc<HistoryStore>>, id: i64) -> Result<(), String> {
    history.delete(id).map(|_| ()).map_err(|e| e.user_message())
}

/// Wipe every history row. Mirrors Swift v1's "Wipe history" control.
/// `async` so the SQLite write runs off the main thread (plan 039 task 43).
#[tauri::command(rename_all = "camelCase")]
pub async fn history_wipe(history: State<'_, Arc<HistoryStore>>) -> Result<(), String> {
    history.wipe_all().map(|_| ()).map_err(|e| e.user_message())
}

/// Resolve a macOS bundle id to its user-visible display name (e.g.
/// `com.mitchellh.ghostty` → `Ghostty`). Returns `None` when the app
/// isn't installed or the bundle id is unknown. Cheap Launch Services
/// lookup; the History pane caches results client-side per bundle id.
#[tauri::command(rename_all = "camelCase")]
pub fn app_display_name(bundle_id: String) -> Option<String> {
    crate::history_store::display_name_for_bundle_id(&bundle_id)
}

// --- Phase 10: Window focus -----------------------------------------------

/// Bring the Main window to the front (show + unminimize + focus).
///
/// Used by the JS-side tray-menu handlers. Splitting the focus action
/// out of the Rust menu callback lets the React Router commit the new
/// route via `flushSync` *before* the window is animated forward —
/// otherwise heavy routes (Settings → General with several
/// `useSetting` invokes on mount) show the previous route for a frame
/// after macOS surfaces the window. The Rust menu handler only emits
/// the navigation event; this command runs immediately after the JS
/// `flushSync(navigate)` returns.
#[tauri::command(rename_all = "camelCase")]
pub fn focus_main_window<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    let Some(window) = app.get_webview_window("main") else {
        return Err("main window not found".into());
    };
    if let Err(err) = window.show() {
        log::warn!(target: "main_window", "show failed: {err}");
    }
    if let Err(err) = window.unminimize() {
        log::debug!(target: "main_window", "unminimize: {err}");
    }
    if let Err(err) = window.set_focus() {
        log::warn!(target: "main_window", "set_focus failed: {err}");
    }
    Ok(())
}

// --- Phase 10: Updater check ----------------------------------------------

/// Trigger an on-demand updater check from the tray's "Check for Updates…"
/// item. Delegates to [`crate::updater::run_manual_check`], which checks the
/// release feed and — when an update exists — kicks off the same silent
/// background download/install used at launch. Returns a typed result the JS
/// layer renders as a toast; the download itself proceeds via the
/// `updater://state` event regardless of the toast outcome.
#[tauri::command(rename_all = "camelCase")]
pub async fn check_for_updates<R: Runtime>(
    app: AppHandle<R>,
) -> crate::updater::UpdaterCheckResult {
    crate::updater::run_manual_check(&app).await
}

/// Apply a downloaded update by restarting into the freshly-swapped bundle.
/// Invoked from the non-blocking "update ready" banner's Restart button (the
/// tray's "Restart to apply" item is the sibling path). Restarting is always
/// user-initiated — the updater never forces it mid-session.
#[tauri::command]
pub fn apply_pending_update<R: Runtime>(app: AppHandle<R>) {
    log::info!(target: "updater", "restart-to-apply-update requested by user (banner)");
    app.restart();
}

/// Final-step wizard handler.
///
/// 1. Persists `app.did_complete_onboarding=true` so the next launch
///    skips straight to Main.
/// 2. Runs the bits of app init deferred during onboarding (tray
///    build + launch-at-login reconciliation) — see
///    `lib.rs::run_post_onboarding_init` for the deferral rationale.
/// 3. Arms the hotkey listener (deferred during onboarding so its
///    Input Monitoring prompt doesn't collide with the wizard's own
///    permission steps).
/// 4. Reveals the Main window.
/// 5. Closes the onboarding window so its webview is freed.
///
/// Dropped the `R: Runtime` generic on this command because
/// `arm_hotkey_pipeline` is monomorphic (the CGEventTap path is
/// macOS+wry only); `tauri::generate_handler!` defaults the
/// `AppHandle` to the wry runtime, which matches.
#[tauri::command(rename_all = "camelCase")]
pub fn complete_onboarding(app: AppHandle) -> Result<(), String> {
    let store = app
        .store(settings::SETTINGS_FILE)
        .map_err(|e| format!("open store: {e}"))?;
    store.set(settings::KEY_DID_COMPLETE_ONBOARDING, json!(true));
    store.save().map_err(|e| format!("save store: {e}"))?;

    // Bring the deferred startup work up to date now that the user
    // has walked through the permission prompts. This is what makes
    // the tray icon appear in the menubar and the launch-at-login
    // preference reconcile against the OS.
    crate::run_post_onboarding_init(&app);

    // Arm before flipping windows so the hotkey is live the moment
    // the user reaches Main. If Input Monitoring is still missing
    // (the wizard's step just had it polled), this no-ops cleanly
    // and the next launch retries.
    crate::arm_hotkey_pipeline(&app);

    if let Some(main) = app.get_webview_window("main") {
        if let Err(err) = main.show() {
            log::warn!(target: "onboarding", "show main failed: {err}");
        }
        if let Err(err) = main.set_focus() {
            log::warn!(target: "onboarding", "focus main failed: {err}");
        }
    }
    if let Some(onboarding) = app.get_webview_window("onboarding") {
        // Closing (vs hiding) frees the webview and matches "wizard is
        // a one-shot" semantics — there's no path back to it without
        // toggling the settings flag from a debug menu.
        if let Err(err) = onboarding.close() {
            log::warn!(target: "onboarding", "close onboarding failed: {err}");
        }
    }
    Ok(())
}

// --- Feature 005: Cost & Usage IPC ----------------------------------------

/// IPC payload for the Cost & Usage panel. Carries every month with
/// recorded spend (newest first) plus the metadata the disclaimer needs
/// (`lastPricedSuccessAt`). The current local month is always the first
/// entry — seeded empty when no calls have landed yet — so the panel's
/// lead header never vanishes at the start of a month.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageHistory {
    pub months: Vec<MonthlySummary>,
    pub last_priced_success_at: Option<i64>,
}

/// Per-provider rollups for every recorded month, newest first, bucketed
/// by the machine's local timezone. Drives the "Cost & Usage" panel: the
/// current month leads, prior months follow. The display months follow
/// wherever the app is used; stored data and the pricing month stay UTC.
#[tauri::command(rename_all = "camelCase")]
pub async fn usage_summary_list_all_months(
    usage: State<'_, Arc<UsageStore>>,
) -> Result<UsageHistory, String> {
    let current_month = usage.current_local_yyyymm().map_err(|e| e.user_message())?;
    let months = usage.summary_by_month().map_err(|e| e.user_message())?;
    let months = paid_months_with_current(months, &current_month);
    let last_priced_success_at = usage
        .last_priced_success_at()
        .map_err(|e| e.user_message())?;
    Ok(UsageHistory {
        months,
        last_priced_success_at,
    })
}

/// Shape the raw per-month rollup for the Cost & Usage panel:
///
/// 1. Drop on-device (free) providers from every month — they carry no
///    cost and would only render "$— (N calls)", diluting the paid
///    totals. (The dev per-model pane keeps them for auditing.)
/// 2. Drop past months left with no paid providers.
/// 3. Always keep the current month — and seed it empty when absent — so
///    the panel's lead header stays put even when only local backends ran
///    (or nothing ran yet this month).
fn paid_months_with_current(
    mut months: Vec<MonthlySummary>,
    current_month: &str,
) -> Vec<MonthlySummary> {
    for month in &mut months {
        month
            .provider_totals
            .retain(|p| !crate::pricing::is_local_provider(&p.provider));
    }
    months.retain(|m| !m.provider_totals.is_empty() || m.year_month == current_month);
    if !months.iter().any(|m| m.year_month == current_month) {
        months.insert(
            0,
            MonthlySummary {
                year_month: current_month.to_string(),
                provider_totals: Vec::new(),
            },
        );
    }
    months
}

/// Per-model rollup for the dev pane only — exposes raw audio
/// seconds / token counts alongside the cost figure.
#[tauri::command(rename_all = "camelCase")]
pub async fn usage_summary_get_per_model_current_month(
    usage: State<'_, Arc<UsageStore>>,
) -> Result<Vec<ModelSummary>, String> {
    // Matches the user-facing rollup's local month so the dev per-model
    // pane lines up with the header above it.
    let yyyymm = usage.current_local_yyyymm().map_err(|e| e.user_message())?;
    usage
        .summary_for_month_per_model(&yyyymm)
        .map_err(|e| e.user_message())
}

/// All `price_history` rows for the current UTC month — wired into
/// the dev "Open prices" viewer so `cargo run --bin muni` can verify
/// the bootstrap seed and the refresher's latest fetch.
#[tauri::command(rename_all = "camelCase")]
pub fn usage_prices_list_current_month(
    usage: State<'_, Arc<UsageStore>>,
) -> Result<Vec<PriceRow>, String> {
    let yyyymm = current_utc_yyyymm();
    usage
        .list_prices_for_month(&yyyymm)
        .map_err(|e| e.user_message())
}

fn current_utc_yyyymm() -> String {
    crate::current_utc_yyyymm()
}

/// DEV-ONLY (Feature 033, task 13): fire a deliberate Sentry message + captured
/// error through the ACTIVE DSN to validate capture + PII scrub + symbolication.
/// Both events are tagged `test:true` so they're trivial to resolve/ignore.
///
/// `#[cfg(debug_assertions)]` keeps this command out of the release binary
/// entirely (it isn't compiled, registered, or invocable in prod), so users
/// can't trigger it. In a dev build it only does anything when Sentry is
/// initialised — point the build at the dev project first:
/// `export MUNI_SENTRY_DSN="$MUNI_SENTRY_DSN_DEV"` (value in `.env`).
#[cfg(debug_assertions)]
#[tauri::command(rename_all = "camelCase")]
pub fn telemetry_fire_test_event() {
    crate::telemetry::fire_test_event();
}

/// DEV-ONLY: fire a real loud error through the SAME presenter the session
/// orchestrator uses in production (`app_handle_presenter` → `route`),
/// so the native `UNUserNotificationCenter` delivery (Muni icon), the
/// navigate-to-tab event, and the auto-focus-main-window behaviour can all be
/// exercised on demand — without blanking a real API key or simulating the
/// hotkey.
///
/// `kind` selects which loud error to fire (default `DeepgramMissingApiKey`):
/// - `"deepgram"` — blocking (`requires_user_action_now`): auto-focuses the
///   window; macOS suppresses the frontmost app's own banner, so it lands in
///   Notification Center, not as a banner.
/// - `"groq"` — non-blocking (raw-text fallback already pasted): does NOT steal
///   focus, so Muni stays in the background and the notification shows as a
///   banner. Use this to see the banner path.
///
/// `#[cfg(debug_assertions)]` keeps this command out of the release binary
/// entirely (not compiled, registered, or invocable in prod).
#[cfg(debug_assertions)]
#[tauri::command(rename_all = "camelCase")]
pub fn debug_fire_loud_notification(app: tauri::AppHandle, kind: Option<String>) {
    use crate::error::MuniError;
    let err = match kind.as_deref() {
        Some("groq") => MuniError::GroqMissingApiKey,
        _ => MuniError::DeepgramMissingApiKey,
    };
    let present = crate::error_presenter::app_handle_presenter(app);
    present(&err);
}

/// Feature 033 (Phase 3, task 21) — route a frontend activation-funnel event
/// into the SAME durable PostHog queue the backend uses, so the funnel shares
/// one transport, one distinct_id (the install UUID), and one offline buffer.
/// This is why the frontend does NOT pull in `posthog-js`.
///
/// `event` is a small fixed vocabulary, NOT a free-form string — only the
/// activation-funnel events the wizard/permissions hooks legitimately emit are
/// accepted; anything else is dropped (so a renderer typo or hostile caller
/// can't widen event/property cardinality or smuggle a property in). The typed
/// `events.rs` constructors build the actual payload, so no transcript content
/// can reach the wire by construction. Fire-and-forget: enqueue + return; gating
/// on the analytics toggle lives in `emit_event` (a no-op when analytics is off).
#[tauri::command(rename_all = "camelCase")]
pub fn telemetry_track(event: String, permission: Option<String>) {
    use crate::telemetry::events;

    let built = match event.as_str() {
        "onboarding_started" => events::onboarding_started(),
        "permission_granted" => {
            // The permission name is a fixed vocabulary; an unknown value is
            // dropped rather than passed through, keeping the property bounded.
            let Some(p) = permission
                .as_deref()
                .filter(|p| events::is_known_permission(p))
            else {
                log::debug!(target: "telemetry", "telemetry_track: ignoring permission_granted with invalid permission");
                return;
            };
            events::permission_granted(p)
        }
        other => {
            log::debug!(target: "telemetry", "telemetry_track: ignoring unknown event {other:?}");
            return;
        }
    };
    crate::telemetry::emit_event(built);
}

#[cfg(test)]
mod usage_summary_tests {
    use super::*;
    use crate::usage_store::ProviderSummary;

    fn provider(name: &str) -> ProviderSummary {
        ProviderSummary {
            provider: name.to_string(),
            total_usd: Some(0.10),
            call_count: 3,
        }
    }

    fn month(year_month: &str, providers: &[&str]) -> MonthlySummary {
        MonthlySummary {
            year_month: year_month.to_string(),
            provider_totals: providers.iter().map(|p| provider(p)).collect(),
        }
    }

    #[test]
    fn drops_local_providers_but_keeps_paid_ones() {
        let input = vec![month("2026-06", &["deepgram", "groq", "whisper_local"])];
        let out = paid_months_with_current(input, "2026-06");
        assert_eq!(out.len(), 1);
        let names: Vec<&str> = out[0]
            .provider_totals
            .iter()
            .map(|p| p.provider.as_str())
            .collect();
        assert_eq!(names, vec!["deepgram", "groq"]);
    }

    #[test]
    fn drops_past_month_that_had_only_local_providers() {
        let input = vec![
            month("2026-06", &["deepgram"]),
            month("2026-05", &["whisper_local"]),
        ];
        let out = paid_months_with_current(input, "2026-06");
        assert_eq!(
            out.iter()
                .map(|m| m.year_month.as_str())
                .collect::<Vec<_>>(),
            vec!["2026-06"],
            "a past month left with no paid providers is dropped",
        );
    }

    #[test]
    fn keeps_current_month_even_when_only_local_ran() {
        let input = vec![month("2026-06", &["whisper_local"])];
        let out = paid_months_with_current(input, "2026-06");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].year_month, "2026-06");
        assert!(
            out[0].provider_totals.is_empty(),
            "local provider stripped, but the current-month header is retained",
        );
    }

    #[test]
    fn seeds_current_month_at_front_when_absent() {
        let input = vec![month("2026-05", &["groq"])];
        let out = paid_months_with_current(input, "2026-06");
        assert_eq!(out[0].year_month, "2026-06");
        assert!(out[0].provider_totals.is_empty());
        assert_eq!(out[1].year_month, "2026-05");
    }
}
