//! Persistent user preferences for the Settings UI (Phase 8).
//!
//! Thin wrapper around `tauri-plugin-store` that mirrors Swift v1's
//! `AppSettings` keys verbatim so a future migration script can map 1:1.
//! The store lives at `app_local_data_dir/settings.json` and is keyed by
//! the dotted strings declared as constants below.
//!
//! Defaults match Swift v1's `@AppStorage` defaults so first-launch
//! behaviour stays identical between v1 and the rewrite.
//!
//! IPC plumbing lives in `commands::settings_get` / `settings_set`; this
//! module owns the keys, defaults, and any pure helpers around them.

use serde_json::{json, Value};

/// Filename of the Tauri Store file under `app_local_data_dir`.
pub const SETTINGS_FILE: &str = "settings.json";

// --- AppSettings keys (mirroring Swift v1 `Settings.swift`) ----------------

pub const KEY_HISTORY_RETENTION_DAYS: &str = "history.retention_days";
pub const KEY_GENERAL_LAUNCH_AT_LOGIN: &str = "general.launch_at_login";
/// Internal one-shot marker: set once the legacy AppleScript login item
/// (pre-SMAppService builds) has been cleaned up. Not user-facing; no
/// default needed — absence reads as "not yet migrated".
pub const KEY_GENERAL_LAUNCH_ITEM_MIGRATED: &str = "general.launch_item_migrated";
pub const KEY_PASTE_DELAY_MS: &str = "paste.delay_ms";
/// Default for [`KEY_PASTE_DELAY_MS`]. Single source of truth for the paste
/// delay across layers: it feeds [`default_for`] and the boot-time injector
/// wiring in `lib::run`. The macOS injector's own `DEFAULT_PASTE_DELAY_MS` must
/// equal this (a drift-guard test pins the two together).
pub const DEFAULT_PASTE_DELAY_MS: u64 = 50;
pub const KEY_CLEANUP_AGGRESSIVENESS: &str = "cleanup.aggressiveness";
pub const KEY_HOTKEY_CUSTOM_ENABLED: &str = "hotkey.custom_enabled";

/// Feature 037 — the main dictation trigger binding. Stores a serialized
/// [`crate::hotkey_binding::DictationBinding`] (a `modifiers` chord). Default is
/// the pre-037 hardcoded `Ctrl+Option` so upgrades keep existing muscle memory.
/// Seeded into the live `HotkeyTriggerState` at boot and re-applied when the
/// setting changes.
pub const KEY_HOTKEY_DICTATION_BINDING: &str = "hotkey.dictation_binding";

/// Feature 037 — the re-paste hotkey binding. Stores a serialized
/// [`crate::hotkey_binding::RepasteBinding`] (`mods` + `key`), or `null` when
/// the user clears it (feature disabled). Default is the enabled `Ctrl+Cmd+V`.
pub const KEY_HOTKEY_REPASTE_BINDING: &str = "hotkey.repaste_binding";

pub const KEY_DID_COMPLETE_ONBOARDING: &str = "app.did_complete_onboarding";

/// Spike-only — selects the per-press ASR backend. `false` (default)
/// routes to Groq Whisper; `true` flips to the Deepgram Nova-3 streaming
/// path ("English Fast Mode"). Persisted so the user's last choice
/// survives restart but the live `EnglishFastModeFlag` is the
/// authoritative read-side state — see `session::EnglishFastModeFlag`.
pub const KEY_ENGLISH_FAST_MODE: &str = "asr.english_fast_mode";

/// Backlog 0012 — user-facing accuracy escape hatch. When `true`,
/// every press skips the LID router and routes to Whisper for
/// bilingual correctness (preserves Tagalog content that Deepgram
/// would silently drop on mid-press code-switches). Takes precedence
/// over `KEY_ENGLISH_FAST_MODE` at routing time. Persisted; live
/// `BilingualModeFlag` is the read-side state — see
/// `session::BilingualModeFlag`.
pub const KEY_BILINGUAL_MODE: &str = "asr.bilingual_mode";

/// Phase 8-only key — user override of the cleanup system prompt. The
/// override path on disk (a `.md` file under `app_local_data_dir`) remains
/// the source of truth for the prompt loader; this entry simply lets the
/// settings UI reflect "saved override exists" without touching the file
/// system on every render. Stored as the literal markdown body or omitted
/// when the user reverts.
pub const KEY_CLEANUP_PROMPT_OVERRIDE: &str = "cleanup.prompt_override";

/// User-selected input device. Stores either the sentinel
/// [`SELECTED_MIC_SYSTEM_DEFAULT`] (follow whatever macOS reports as
/// the current default — the post-install behaviour) or a cpal device
/// name. Resolved on every press: if the named device is missing, the
/// audio host logs a warning and falls back to the system default for
/// that press so dictation never blocks on an unplugged mic.
pub const KEY_SELECTED_MICROPHONE: &str = "audio.selected_microphone";

/// Sentinel value of [`KEY_SELECTED_MICROPHONE`] meaning "ask the OS
/// for the current default every press". Spelt out as a constant so
/// the tray menu, settings command, and audio host can't drift on the
/// magic string.
pub const SELECTED_MIC_SYSTEM_DEFAULT: &str = "system_default";

/// Feature 013 — global on/off switch for the My Words substitution
/// layer. Default `true`: rules-on-disk apply automatically. The user
/// can flip this from Settings → My Words without deleting their list.
pub const KEY_MY_WORDS_ENABLED: &str = "my_words.enabled";

/// Feature 013 — persisted JSON array of `{trigger, replacement}` entries.
/// Default `[]` on first launch.
pub const KEY_MY_WORDS_ENTRIES: &str = "my_words.entries";

/// Feature 014 — free-form "About Me" vocabulary context. Default `""`.
/// When non-empty, prepended to the cleanup system prompt under a
/// labeled `## ABOUT THE USER` block. Capped at
/// [`crate::about_me::MAX_LEN`] chars; empty string is the canonical
/// "off" state (byte-identical prompt to pre-feature).
pub const KEY_CLEANUP_ABOUT_ME: &str = "cleanup.about_me";

/// Feature 015 — global on/off switch for the vocabulary soft-bias
/// list. Default `true`: entries on disk are rendered into the cleanup
/// system prompt automatically. The user can flip this from Settings →
/// Cleanup → Vocabulary without deleting their list.
pub const KEY_VOCABULARY_ENABLED: &str = "vocabulary.enabled";

/// Feature 015 — persisted JSON array of `{term, note}` entries.
/// Default `[]` on first launch. Capped at
/// [`crate::vocabulary::MAX_ENTRIES`] entries; per-entry term/note
/// caps live alongside it in `crate::vocabulary`.
pub const KEY_VOCABULARY_ENTRIES: &str = "vocabulary.entries";

/// User-authored "preferences" prompt that takes precedence over the
/// bundled cleanup system prompt when the two conflict. Default `""`
/// (empty string is the canonical "off" state — byte-identical prompt
/// to pre-feature). Capped at [`crate::user_prompt::MAX_LEN`] chars.
pub const KEY_CLEANUP_USER_PROMPT: &str = "cleanup.user_prompt";

// --- Feature 033: telemetry consent (default-on per brainstorm 010 Q2) -----
//
// Two separable toggles gate the post-launch monitoring subsystem. Both
// default `true`: the first-run consent card presents them pre-checked
// (informed opt-out), and Settings → General mirrors them. They only ever
// gate *operational metadata* — latency, error codes, ASR path, model ids,
// app/OS version, feature-usage counts — never transcript or audio content.

/// Phase 1 — gates Sentry crash/error capture. `true` → crashes are sent
/// (subject to a baked DSN, which dev/staging builds lack, so they stay
/// silent regardless).
pub const KEY_CRASH_REPORTING_ENABLED: &str = "telemetry.crash_reporting";

/// Phase 2 — gates the PostHog operational-health analytics queue.
/// `true` → metadata-only health events are enqueued (again subject to a
/// baked key, absent in dev/staging).
pub const KEY_ANALYTICS_ENABLED: &str = "telemetry.analytics";

/// Emitted on every successful [`crate::commands::settings_set`]. Lets any
/// mounted `useSetting` hook update live — including one in a *different*
/// window than the writer (e.g. the Settings page reflecting a toggle the
/// onboarding window just wrote). Without this, `useSetting` reads once at
/// mount and shows a stale value when the same key is changed elsewhere.
/// Payload: `{ "key": <string>, "value": <json> }`.
pub const EVENT_SETTINGS_CHANGED: &str = "settings://changed";

/// Lookup the Swift v1 default for `key`, returning `None` for unknown keys.
/// Centralised so [`commands::settings_get`] can fall back to the same
/// defaults the Settings UI presents on a brand-new install.
pub fn default_for(key: &str) -> Option<Value> {
    match key {
        KEY_HISTORY_RETENTION_DAYS => Some(json!(30)),
        // Default OFF: the onboarding wizard has its own Launch-at-Login
        // step that lets the user opt in explicitly (and fires the
        // "control System Events" Automation prompt in context). Without
        // this flip, the post-Finish reconcile would re-enable Login
        // Items behind the user's back and re-introduce the cold prompt
        // the wizard step exists to avoid. Returning users can toggle
        // it on at any time from Settings → General.
        KEY_GENERAL_LAUNCH_AT_LOGIN => Some(json!(false)),
        KEY_PASTE_DELAY_MS => Some(json!(DEFAULT_PASTE_DELAY_MS)),
        KEY_CLEANUP_AGGRESSIVENESS => Some(json!(1)),
        KEY_HOTKEY_CUSTOM_ENABLED => Some(json!(false)),
        // Feature 037 — serialize the binding defaults so first-launch reads
        // the enabled Ctrl+Option dictation trigger and the enabled Ctrl+Cmd+V
        // re-paste hotkey (an object, never `null` — `null` means "disabled").
        KEY_HOTKEY_DICTATION_BINDING => Some(
            serde_json::to_value(crate::hotkey_binding::DictationBinding::default_dictation())
                .expect("DictationBinding default serializes"),
        ),
        KEY_HOTKEY_REPASTE_BINDING => Some(
            serde_json::to_value(crate::hotkey_binding::RepasteBinding::default_repaste())
                .expect("RepasteBinding default serializes"),
        ),
        KEY_DID_COMPLETE_ONBOARDING => Some(json!(false)),
        KEY_ENGLISH_FAST_MODE => Some(json!(false)),
        KEY_BILINGUAL_MODE => Some(json!(false)),
        KEY_SELECTED_MICROPHONE => Some(json!(SELECTED_MIC_SYSTEM_DEFAULT)),
        KEY_MY_WORDS_ENABLED => Some(json!(true)),
        KEY_MY_WORDS_ENTRIES => Some(json!([])),
        KEY_CLEANUP_ABOUT_ME => Some(json!("")),
        KEY_VOCABULARY_ENABLED => Some(json!(true)),
        KEY_VOCABULARY_ENTRIES => Some(json!([])),
        KEY_CLEANUP_USER_PROMPT => Some(json!("")),
        KEY_CRASH_REPORTING_ENABLED => Some(json!(true)),
        KEY_ANALYTICS_ENABLED => Some(json!(true)),
        // Cleanup prompt override has no default — absence means "use the
        // bundled prompt" and is meaningful, distinct from "stored empty".
        _ => None,
    }
}

/// Returns `true` when `key` is one Settings is allowed to write to.
///
/// This is the IPC boundary check: without it, a compromised webview could
/// pollute the store with unrelated keys (e.g. impersonating internal
/// pieces of state). Mirrors the closed list of `@AppStorage` keys in
/// Swift v1.
pub fn is_allowed_key(key: &str) -> bool {
    matches!(
        key,
        KEY_HISTORY_RETENTION_DAYS
            | KEY_GENERAL_LAUNCH_AT_LOGIN
            | KEY_PASTE_DELAY_MS
            | KEY_CLEANUP_AGGRESSIVENESS
            | KEY_HOTKEY_CUSTOM_ENABLED
            | KEY_HOTKEY_DICTATION_BINDING
            | KEY_HOTKEY_REPASTE_BINDING
            | KEY_DID_COMPLETE_ONBOARDING
            | KEY_ENGLISH_FAST_MODE
            | KEY_BILINGUAL_MODE
            | KEY_CLEANUP_PROMPT_OVERRIDE
            | KEY_SELECTED_MICROPHONE
            | KEY_MY_WORDS_ENABLED
            | KEY_MY_WORDS_ENTRIES
            | KEY_CLEANUP_ABOUT_ME
            | KEY_VOCABULARY_ENABLED
            | KEY_VOCABULARY_ENTRIES
            | KEY_CLEANUP_USER_PROMPT
            | KEY_CRASH_REPORTING_ENABLED
            | KEY_ANALYTICS_ENABLED
    )
}

/// Upper bound on the serialized byte size of a single value written through
/// [`crate::commands::settings_set`]. The store is a small JSON preferences
/// file loaded on every launch; without a cap a compromised webview could
/// wedge a multi-megabyte blob into it and bloat both disk and the boot-time
/// parse. 64 KiB is comfortably above the largest legitimate value (a full
/// 200-entry My Words / Vocabulary list) yet small enough to stay a config
/// file, not a data store.
pub const MAX_SETTING_VALUE_BYTES: usize = 64 * 1024;

/// Keys that are allowlisted for `settings_get` round-trips but must NOT be
/// written through the generic [`crate::commands::settings_set`] path: they
/// have dedicated, validated commands (`set_dictation_hotkey` /
/// `set_repaste_hotkey`) that enforce structural validity, cross-binding
/// collision rules, and live re-registration a raw JSON write would bypass
/// (plan 039 task 42). A generic write here would desync the store from the
/// live `HotkeyTriggerState` / controllers and could persist an unregisterable
/// or self-tripping chord.
pub fn is_settings_set_forbidden_key(key: &str) -> bool {
    matches!(
        key,
        KEY_HOTKEY_DICTATION_BINDING | KEY_HOTKEY_REPASTE_BINDING
    )
}

/// The coarse JSON type of a value, used to reject a `settings_set` write
/// whose shape doesn't match the key's declared default (e.g. a string sent
/// to a boolean toggle). Integers and floats collapse to `"number"` — JSON
/// draws no line between them and neither does the store.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Validate — and where applicable normalize — a value bound for
/// [`crate::commands::settings_set`] (plan 039 task 42). The caller has
/// already checked [`is_allowed_key`]; this layer adds the per-key rules a
/// bare allowlist can't express:
///
/// 1. Reject keys with dedicated validated commands (binding keys).
/// 2. Reject a value whose JSON type doesn't match the key's `default_for`
///    type. Keys with no default (`cleanup.prompt_override`) are managed by
///    dedicated code and carry meaningful absence, so they skip the type gate.
/// 3. Reject an oversized serialized payload ([`MAX_SETTING_VALUE_BYTES`]).
/// 4. Route list-shaped keys (`my_words.entries`, `vocabulary.entries`)
///    through their module sanitizers so a raw write can't bypass the same
///    trimming, whitespace-collapse, and entry caps the dedicated save
///    commands enforce.
///
/// Returns the value to persist (possibly the sanitized form for list keys).
pub fn validate_set_value(key: &str, value: Value) -> Result<Value, String> {
    if is_settings_set_forbidden_key(key) {
        return Err(format!(
            "{key} cannot be set via settings_set; use its dedicated hotkey command"
        ));
    }

    // Size cap first — cheap and independent of type.
    let serialized_len = serde_json::to_string(&value)
        .map(|s| s.len())
        .unwrap_or(usize::MAX);
    if serialized_len > MAX_SETTING_VALUE_BYTES {
        return Err(format!(
            "value for {key} is too large ({serialized_len} bytes > {MAX_SETTING_VALUE_BYTES} cap)"
        ));
    }

    // Type gate against the declared default's JSON kind.
    if let Some(default) = default_for(key) {
        let (expected, got) = (json_kind(&default), json_kind(&value));
        if expected != got {
            return Err(format!(
                "value for {key} has wrong type (expected {expected}, got {got})"
            ));
        }
    }

    // Sanitize list-shaped keys through their owning modules.
    match key {
        KEY_MY_WORDS_ENTRIES => crate::my_words::sanitize_entries_value(&value),
        KEY_VOCABULARY_ENTRIES => crate::vocabulary::sanitize_entries_value(&value),
        _ => Ok(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_swift_v1_app_storage_values() {
        assert_eq!(default_for(KEY_HISTORY_RETENTION_DAYS), Some(json!(30)));
        assert_eq!(default_for(KEY_GENERAL_LAUNCH_AT_LOGIN), Some(json!(false)));
        assert_eq!(default_for(KEY_PASTE_DELAY_MS), Some(json!(50)));
        assert_eq!(default_for(KEY_CLEANUP_AGGRESSIVENESS), Some(json!(1)));
        assert_eq!(default_for(KEY_HOTKEY_CUSTOM_ENABLED), Some(json!(false)));
        assert_eq!(default_for(KEY_DID_COMPLETE_ONBOARDING), Some(json!(false)));
        assert_eq!(default_for(KEY_ENGLISH_FAST_MODE), Some(json!(false)));
        assert_eq!(default_for(KEY_BILINGUAL_MODE), Some(json!(false)));
        assert_eq!(
            default_for(KEY_SELECTED_MICROPHONE),
            Some(json!(SELECTED_MIC_SYSTEM_DEFAULT))
        );
        assert_eq!(default_for(KEY_MY_WORDS_ENABLED), Some(json!(true)));
        assert_eq!(default_for(KEY_MY_WORDS_ENTRIES), Some(json!([])));
        assert_eq!(default_for(KEY_CLEANUP_ABOUT_ME), Some(json!("")));
        assert_eq!(default_for(KEY_VOCABULARY_ENABLED), Some(json!(true)));
        assert_eq!(default_for(KEY_VOCABULARY_ENTRIES), Some(json!([])));
        assert_eq!(default_for(KEY_CLEANUP_USER_PROMPT), Some(json!("")));
        assert_eq!(default_for(KEY_CRASH_REPORTING_ENABLED), Some(json!(true)));
        assert_eq!(default_for(KEY_ANALYTICS_ENABLED), Some(json!(true)));
    }

    #[test]
    fn telemetry_consent_keys_default_on_and_allowlisted() {
        // Feature 033 — default-on (informed opt-out per brainstorm 010 Q2)
        // and writable from both the consent card and Settings → General.
        for key in [KEY_CRASH_REPORTING_ENABLED, KEY_ANALYTICS_ENABLED] {
            assert_eq!(default_for(key), Some(json!(true)), "{key} defaults on");
            assert!(is_allowed_key(key), "{key} should be allowlisted");
        }
    }

    #[test]
    fn hotkey_binding_keys_default_to_enabled_bindings_and_allowlisted() {
        use crate::hotkey_binding::{DictationBinding, RepasteBinding};

        // Dictation default is the Ctrl+Option object (not null).
        let dictation = default_for(KEY_HOTKEY_DICTATION_BINDING).expect("has a default");
        assert_eq!(
            dictation,
            serde_json::to_value(DictationBinding::default_dictation()).unwrap()
        );
        assert!(dictation.is_object(), "dictation default must be an object");

        // Re-paste default is the ENABLED Ctrl+Cmd+V object, never null.
        let repaste = default_for(KEY_HOTKEY_REPASTE_BINDING).expect("has a default");
        assert_eq!(
            repaste,
            serde_json::to_value(RepasteBinding::default_repaste()).unwrap()
        );
        assert!(repaste.is_object(), "re-paste default must be enabled");

        assert!(is_allowed_key(KEY_HOTKEY_DICTATION_BINDING));
        assert!(is_allowed_key(KEY_HOTKEY_REPASTE_BINDING));
    }

    #[test]
    fn unknown_key_has_no_default() {
        assert!(default_for("totally.unknown").is_none());
    }

    #[test]
    fn cleanup_prompt_override_has_no_default() {
        // Absence is meaningful — see KEY_CLEANUP_PROMPT_OVERRIDE doc.
        assert!(default_for(KEY_CLEANUP_PROMPT_OVERRIDE).is_none());
    }

    #[test]
    fn allowlist_matches_known_keys() {
        for key in [
            KEY_HISTORY_RETENTION_DAYS,
            KEY_GENERAL_LAUNCH_AT_LOGIN,
            KEY_PASTE_DELAY_MS,
            KEY_CLEANUP_AGGRESSIVENESS,
            KEY_HOTKEY_CUSTOM_ENABLED,
            KEY_DID_COMPLETE_ONBOARDING,
            KEY_ENGLISH_FAST_MODE,
            KEY_BILINGUAL_MODE,
            KEY_CLEANUP_PROMPT_OVERRIDE,
            KEY_SELECTED_MICROPHONE,
            KEY_MY_WORDS_ENABLED,
            KEY_MY_WORDS_ENTRIES,
            KEY_CLEANUP_ABOUT_ME,
            KEY_VOCABULARY_ENABLED,
            KEY_VOCABULARY_ENTRIES,
            KEY_CLEANUP_USER_PROMPT,
            KEY_CRASH_REPORTING_ENABLED,
            KEY_ANALYTICS_ENABLED,
        ] {
            assert!(is_allowed_key(key), "{key} should be in allowlist");
        }
    }

    #[test]
    fn allowlist_rejects_arbitrary_keys() {
        assert!(!is_allowed_key(""));
        assert!(!is_allowed_key("not.a.real.key"));
        assert!(!is_allowed_key("history.retention_days.extra"));
    }

    // --- Plan 039 task 42: settings_set value validation -------------------

    #[test]
    fn validate_set_value_rejects_wrong_type_for_boolean_key() {
        // A non-bool sent to a telemetry consent toggle must be rejected so a
        // compromised webview can't store a shape the gate can't read.
        let err = validate_set_value(KEY_ANALYTICS_ENABLED, json!("yes"))
            .expect_err("string to a boolean key must be rejected");
        assert!(err.contains("wrong type"), "{err}");
        assert!(err.contains(KEY_ANALYTICS_ENABLED), "{err}");
        // The correct type still passes through unchanged.
        assert_eq!(
            validate_set_value(KEY_ANALYTICS_ENABLED, json!(false)).unwrap(),
            json!(false)
        );
    }

    #[test]
    fn validate_set_value_rejects_wrong_type_for_number_key() {
        assert!(validate_set_value(KEY_HISTORY_RETENTION_DAYS, json!("30")).is_err());
        assert_eq!(
            validate_set_value(KEY_HISTORY_RETENTION_DAYS, json!(45)).unwrap(),
            json!(45)
        );
    }

    #[test]
    fn validate_set_value_rejects_oversized_payload() {
        // A string past the 64 KiB cap on any key is refused before it can
        // bloat the store file. Use an allowlisted string-typed key.
        let huge = "x".repeat(MAX_SETTING_VALUE_BYTES + 1);
        let err = validate_set_value(KEY_CLEANUP_ABOUT_ME, json!(huge))
            .expect_err("oversized value must be rejected");
        assert!(err.contains("too large"), "{err}");
    }

    #[test]
    fn validate_set_value_rejects_binding_keys() {
        // Binding keys have dedicated validated commands; the generic path
        // must refuse them even though they remain allowlisted for reads.
        for key in [KEY_HOTKEY_DICTATION_BINDING, KEY_HOTKEY_REPASTE_BINDING] {
            assert!(is_allowed_key(key), "{key} still readable via settings_get");
            let err = validate_set_value(key, json!({ "mods": [], "key": "KeyA" }))
                .expect_err("binding key must be rejected by settings_set");
            assert!(err.contains("dedicated"), "{err}");
        }
    }

    #[test]
    fn validate_set_value_routes_my_words_entries_through_sanitizer() {
        // A trigger with padding + internal double-space is normalized, and an
        // empty-trigger entry is dropped — proving the sanitizer ran.
        let raw = json!([
            { "trigger": "  hello   world  ", "replacement": "hi" },
            { "trigger": "   ", "replacement": "dropped" }
        ]);
        let out = validate_set_value(KEY_MY_WORDS_ENTRIES, raw).unwrap();
        assert_eq!(
            out,
            json!([{ "trigger": "hello world", "replacement": "hi" }])
        );
    }

    #[test]
    fn validate_set_value_routes_vocabulary_entries_through_sanitizer() {
        // A note with an embedded newline collapses to a single line so it
        // can't inject a free-standing line into the rendered prompt block.
        let raw = json!([{ "term": "Zernio", "note": "my\nSaaS" }]);
        let out = validate_set_value(KEY_VOCABULARY_ENTRIES, raw).unwrap();
        assert_eq!(out, json!([{ "term": "Zernio", "note": "my SaaS" }]));
    }

    #[test]
    fn validate_set_value_rejects_malformed_list_entries() {
        // A my_words.entries value that's the right JSON kind (array) but the
        // wrong element shape is rejected by the sanitizer, not silently stored.
        let raw = json!([{ "not": "an entry" }]);
        assert!(validate_set_value(KEY_MY_WORDS_ENTRIES, raw).is_err());
    }

    #[test]
    fn validate_set_value_skips_type_gate_for_keys_without_default() {
        // cleanup.prompt_override has no default (absence is meaningful); a
        // string body is accepted as-is.
        assert_eq!(
            validate_set_value(KEY_CLEANUP_PROMPT_OVERRIDE, json!("custom prompt")).unwrap(),
            json!("custom prompt")
        );
    }
}
