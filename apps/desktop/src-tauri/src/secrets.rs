//! Secrets storage backed by the OS keyring (Phase 8).
//!
//! Mirrors Swift v1's `Keychain.swift` + `APIKeyProvider.swift` 1:1:
//! - Service identifier `com.muni.app` (suffixable via
//!   `MUNI_KEYCHAIN_SUFFIX` for parallel test installations).
//! - Accounts `deepgram` and `groq`.
//! - On read, the env-var override (`MUNI_DEEPGRAM_KEY` /
//!   `MUNI_GROQ_KEY`) wins so dev workflows that source `.env` keep
//!   working without touching the keychain.
//!
//! The `keyring` crate abstracts macOS Keychain, Windows Credential
//! Manager, and Linux Secret Service behind a single `Entry` API. We
//! deliberately use `Entry::new(service, account)` (no target) so the
//! item shows up under the standard `com.muni.app` service in
//! Keychain.app for users who want to inspect or delete it manually.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use crate::error::MuniError;

/// Default service id — same as Swift v1's `Keychain.defaultService`.
const KEYRING_SERVICE: &str = "com.muni.app";

/// Fired by Rust AFTER a successful set or delete of any secret.
/// React-side `useSecretProbe` listens and refreshes its cached `isSet`
/// state. Without this, the Settings → API Keys probe runs once at
/// MainWindow mount (which is at app launch, before onboarding even
/// shows) and never sees keys saved later through the wizard.
pub const EVENT_SECRETS_CHANGED: &str = "secrets://changed";

/// Account identifiers — match Swift v1's `Keychain.deepgramKeyAccount`
/// / `groqKeyAccount` so a user upgrading from v1 keeps their stored
/// key without re-entering it (same OS keychain item).
pub const DEEPGRAM_ACCOUNT: &str = "deepgram";
pub const GROQ_ACCOUNT: &str = "groq";
/// Feature 003 — Gemini text-LID account. The classifier provider is
/// intentionally swappable (see `text_lid::TextLidClassifier`); when a
/// future provider lands (local Gemma, OpenAI, etc.) it will register
/// its own account constant alongside this one rather than reusing it.
pub const GEMINI_ACCOUNT: &str = "gemini";
/// Gladia Solaria-1 ASR account. Consulted by the Whisper-batch
/// fallback path (`session::attempt_gladia_fallback_transcribe`),
/// which opens a fresh `GladiaClient` on demand when Groq Whisper
/// fails on the auto-detect Whisper-route batch call.
pub const GLADIA_ACCOUNT: &str = "gladia";
/// Env vars that override keychain reads. Identical to Swift v1's
/// `APIKeyProvider` so existing devs' shells keep working.
pub const DEEPGRAM_ENV_VAR: &str = "MUNI_DEEPGRAM_KEY";
pub const GROQ_ENV_VAR: &str = "MUNI_GROQ_KEY";
pub const GEMINI_ENV_VAR: &str = "MUNI_GEMINI_KEY";
pub const GLADIA_ENV_VAR: &str = "MUNI_GLADIA_KEY";
/// Test-only override for the keyring service id. Mirrors Swift v1's
/// `MUNI_KEYCHAIN_SUFFIX`. Lets a parallel run in CI / local tests scribble
/// over its own keychain entries without colliding with a developer's
/// already-saved keys.
const SERVICE_SUFFIX_ENV: &str = "MUNI_KEYCHAIN_SUFFIX";

/// Suffix baked into the binary at build time (`MUNI_BAKED_KEYCHAIN_SUFFIX`,
/// forwarded by `build.rs`). Lets the staging bundle — which inherits no shell
/// env from Finder — isolate its keychain under `com.muni.app.staging` so a
/// fresh-user staging test never reads or clobbers the prod (`com.muni.app`)
/// or dev (`com.muni.app.dev`, runtime-suffixed) entries. Prod bakes nothing,
/// keeping `com.muni.app` for v1-upgrade key continuity.
const BAKED_SERVICE_SUFFIX: Option<&str> = option_env!("MUNI_BAKED_KEYCHAIN_SUFFIX");

/// Compose the runtime service id. Resolution order: runtime
/// [`SERVICE_SUFFIX_ENV`] override (dev/tests) → build-time
/// [`BAKED_SERVICE_SUFFIX`] (staging bundle) → bare [`KEYRING_SERVICE`] (prod).
fn service_id() -> String {
    if let Ok(suffix) = std::env::var(SERVICE_SUFFIX_ENV) {
        if !suffix.is_empty() {
            return format!("{KEYRING_SERVICE}.{suffix}");
        }
    }
    if let Some(suffix) = BAKED_SERVICE_SUFFIX {
        let suffix = suffix.trim();
        if !suffix.is_empty() {
            return format!("{KEYRING_SERVICE}.{suffix}");
        }
    }
    KEYRING_SERVICE.to_string()
}

/// Map a service identifier (`"deepgram"` / `"groq"`) onto the matching
/// `(account, env_var, missing_error)` triple. Centralised so the
/// per-service helpers below stay tiny and future services slot in by
/// extending one match.
fn resolve_service(service: &str) -> Option<(&'static str, &'static str, MuniError)> {
    match service {
        DEEPGRAM_ACCOUNT => Some((
            DEEPGRAM_ACCOUNT,
            DEEPGRAM_ENV_VAR,
            MuniError::DeepgramMissingApiKey,
        )),
        GROQ_ACCOUNT => Some((GROQ_ACCOUNT, GROQ_ENV_VAR, MuniError::GroqMissingApiKey)),
        GEMINI_ACCOUNT => Some((
            GEMINI_ACCOUNT,
            GEMINI_ENV_VAR,
            MuniError::GeminiMissingApiKey,
        )),
        GLADIA_ACCOUNT => Some((
            GLADIA_ACCOUNT,
            GLADIA_ENV_VAR,
            MuniError::GladiaMissingApiKey,
        )),
        _ => None,
    }
}

/// Outcome of a raw keychain read, keeping the "genuinely absent" case
/// distinct from "the keychain refused to answer".
///
/// Every reader keeps the arms apart (Slice 10 task 29 landed this): `Absent`
/// maps to the provider's missing-key error, `Unavailable` (keychain
/// locked / ACL-denied / entry-init error) maps to
/// [`MuniError::KeychainUnavailable`] — both in the uncached [`get`] and the
/// hot-path [`get_cached`] — so the user is told to unlock their keychain
/// rather than to re-enter a key that is actually stored. Caching also needs
/// them apart: a genuine absence is safe to memoise as a sticky miss, but a
/// transient `Unavailable` must NOT be cached — otherwise a one-off failure
/// pins every later press on a stale miss until the next `secrets://changed` or
/// app restart. [`presence`] surfaces the same three-way split (as
/// [`SecretPresence`]) to its callers.
enum KeychainRead {
    /// A non-empty secret resolved from the keychain.
    Found(String),
    /// The entry is genuinely not present (or present-but-empty) — cacheable.
    Absent,
    /// The keychain could not be consulted (locked/denied/init error) — the
    /// value's presence is unknown, so this outcome must not be cached.
    Unavailable,
}

/// Raw keychain read for one account, with the absence-vs-unavailable
/// distinction preserved. Never consults the env override — callers handle
/// that layer first.
fn read_keychain(account: &str) -> KeychainRead {
    let entry = match keyring::Entry::new(&service_id(), account) {
        Ok(e) => e,
        Err(e) => {
            log::warn!(target: "keychain", "entry init failed for {account}: {e}");
            return KeychainRead::Unavailable;
        }
    };
    match entry.get_password() {
        Ok(value) if !value.is_empty() => KeychainRead::Found(value),
        Ok(_) | Err(keyring::Error::NoEntry) => KeychainRead::Absent,
        Err(e) => {
            log::warn!(target: "keychain", "get_password failed for {account}: {e}");
            KeychainRead::Unavailable
        }
    }
}

/// Read a secret, honouring the env-var override before touching the keyring.
///
/// Returns the matching missing-API-key error from `MuniError` only for a
/// *genuine* absence. A keychain that refused to answer (locked / ACL-denied)
/// maps to [`MuniError::KeychainUnavailable`] instead (Slice 10 task 29), so
/// the user is told to unlock their keychain rather than to re-enter a key
/// that is actually stored.
pub fn get(service: &str) -> Result<String, MuniError> {
    let (account, env_var, missing) =
        resolve_service(service).ok_or_else(|| missing_for_unknown_service(service))?;
    if let Ok(value) = std::env::var(env_var) {
        if !value.is_empty() {
            return Ok(value);
        }
    }
    keychain_read_to_result(read_keychain(account), missing)
}

/// Map a raw [`KeychainRead`] to the caller-facing [`get`] result, keeping the
/// "genuinely absent" case (→ the provider's missing-key error) distinct from a
/// "keychain refused to answer" case (locked / ACL-denied →
/// [`MuniError::KeychainUnavailable`]). Pure so the absent-vs-unavailable split
/// is unit-testable without a live keychain (a locked keychain can't be forced
/// deterministically in CI).
fn keychain_read_to_result(read: KeychainRead, missing: MuniError) -> Result<String, MuniError> {
    match read {
        KeychainRead::Found(value) => Ok(value),
        KeychainRead::Absent => Err(missing),
        KeychainRead::Unavailable => Err(MuniError::KeychainUnavailable),
    }
}

/// Store a secret in the OS keyring. Empty strings are rejected — the
/// caller should call [`delete`] instead so the entry isn't left in an
/// ambiguous "set but blank" state.
pub fn set(service: &str, value: &str) -> Result<(), MuniError> {
    if value.is_empty() {
        return Err(MuniError::EmptyApiKey);
    }
    let (account, _, _) =
        resolve_service(service).ok_or_else(|| missing_for_unknown_service(service))?;
    let entry = keyring::Entry::new(&service_id(), account).map_err(|e| {
        log::warn!(target: "keychain", "entry init failed for {account}: {e}");
        MuniError::KeychainWriteFailed {
            reason: e.to_string(),
        }
    })?;
    entry
        .set_password(value)
        .map_err(|e| MuniError::KeychainWriteFailed {
            reason: e.to_string(),
        })
}

/// Remove a secret from the OS keyring. Idempotent — deleting an
/// already-absent entry succeeds silently.
pub fn delete(service: &str) -> Result<(), MuniError> {
    let (account, _, _) =
        resolve_service(service).ok_or_else(|| missing_for_unknown_service(service))?;
    let entry = keyring::Entry::new(&service_id(), account).map_err(|e| {
        log::warn!(target: "keychain", "entry init failed for {account}: {e}");
        MuniError::KeychainWriteFailed {
            reason: e.to_string(),
        }
    })?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(MuniError::KeychainWriteFailed {
            reason: e.to_string(),
        }),
    }
}

/// Probe that returns whether a key exists **in the OS keychain** WITHOUT
/// exposing the value. Used by the Settings UI to decide whether to
/// render a "key set" indicator and whether the Remove button should be
/// enabled.
///
/// In **release builds** this deliberately does NOT consult the env-var
/// override — the override is a developer convenience that bypasses the
/// user-facing storage, and surfacing it as "saved" would make the
/// Remove button look like it failed (delete the keychain entry, env
/// var still wins, probe still returns true).
///
/// In **debug builds** the env-var override IS treated as "set". Without
/// this, the Settings-UI probe fires `get_password` on every mount and
/// each `tauri dev` rebuild has a fresh code signature, so macOS prompts
/// for the keychain password every time ("Always Allow" cannot stick to
/// a signature that keeps changing). Devs running with env vars don't
/// care whether keychain has the key — it's not the path actually used.
///
/// Use [`get`] when you actually need the live key for API calls.
///
/// This is a `bool` probe, so a keychain that refused to answer (locked /
/// ACL-denied) necessarily reads as "not set" — the value's presence is
/// genuinely unknown. It is a thin wrapper over [`presence`]; callers that must
/// tell "genuinely absent" apart from "keychain unavailable" use [`presence`]
/// directly, and callers that need the live key go
/// through [`get`], which returns [`MuniError::KeychainUnavailable`] for the
/// locked case.
pub fn is_set(service: &str) -> bool {
    matches!(presence(service), SecretPresence::Set)
}

/// Tri-state presence of a secret, distinguishing a key that is genuinely
/// absent from a keychain that refused to answer (locked / ACL-denied /
/// entry-init error). Unlike [`is_set`] — a `bool` that necessarily collapses
/// the unknown case to "not set" — this preserves the three-way split so a
/// caller can surface the locked-keychain state to the user instead of
/// mislabelling it "key missing" (Slice 10 task 29).
///
/// Honours the same debug-build env-var short-circuit as [`is_set`]/[`get`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretPresence {
    /// A non-empty secret is present (keychain or, in debug builds, env override).
    Set,
    /// The secret is genuinely not present (or present-but-empty).
    Absent,
    /// The keychain refused to answer — presence is unknown, not "missing".
    Unknown,
}

/// Probe a secret's presence as a tri-state (see [`SecretPresence`]). Never
/// exposes the value. An unknown service reads as [`SecretPresence::Absent`] —
/// there is no keychain entry to be locked out of.
pub fn presence(service: &str) -> SecretPresence {
    let Some((account, _env_var, _)) = resolve_service(service) else {
        return SecretPresence::Absent;
    };
    #[cfg(debug_assertions)]
    if std::env::var(_env_var).is_ok_and(|v| !v.is_empty()) {
        return SecretPresence::Set;
    }
    match read_keychain(account) {
        KeychainRead::Found(_) => SecretPresence::Set,
        KeychainRead::Absent => SecretPresence::Absent,
        KeychainRead::Unavailable => SecretPresence::Unknown,
    }
}

// ───────────────────────── hot-path read cache (plan 039 task 17) ─────────────────────────

/// Process-wide cache of resolved secrets for the release path.
///
/// Key: account id (`"groq"`, `"gladia"`, …). Value: `Some(secret)` once the
/// key resolved (env override or keychain), `None` when it is known-absent.
/// Populated lazily by [`get_cached`]; wiped wholesale by [`invalidate_cache`]
/// on every `secrets://changed` emit (see `commands::emit_secrets_changed`).
///
/// Exposure note: this holds the plaintext secret in process memory for the
/// lifetime of the entry — the **same** exposure class as the per-call
/// `keyring` reads it replaces (the value already transited this process's heap
/// on every ASR call). Values are NEVER logged.
fn secret_cache() -> &'static RwLock<HashMap<String, Option<String>>> {
    static CACHE: OnceLock<RwLock<HashMap<String, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Cached variant of [`get`] for the hot release path.
///
/// Only the expensive layer — the OS keychain IPC — is cached. The env-var
/// override is still consulted **live** on every call: it is a cheap
/// process-local read (no IPC), and keeping it uncached means a dev/test `.env`
/// change takes effect immediately without a cache flush (and so a cached
/// keychain miss can never shadow an env override). The keychain result is
/// memoised per account and wiped wholesale on `secrets://changed`.
///
/// Only a *definitive* keychain outcome is cached — a resolved key or a genuine
/// absence. A transient failure (keychain locked/denied) returns
/// [`MuniError::KeychainUnavailable`] uncached (see `resolve_cache_outcome`) so
/// the dictation path tells the user to unlock the keychain (never "add your
/// key"), and the next press retries so a one-off failure self-heals instead of
/// pinning every later press on a stale miss.
///
/// Unknown services bypass the cache and error immediately, exactly like
/// [`get`]. A poisoned lock is recovered rather than propagated — a cache read
/// must never panic the release path.
pub fn get_cached(service: &str) -> Result<String, MuniError> {
    let Some((account, env_var, missing)) = resolve_service(service) else {
        return Err(missing_for_unknown_service(service));
    };
    // Live env override (dev/test convenience) — never cached.
    if let Ok(value) = std::env::var(env_var) {
        if !value.is_empty() {
            return Ok(value);
        }
    }
    // Cached keychain layer.
    if let Some(entry) = secret_cache()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .get(account)
    {
        return match entry {
            Some(value) => Ok(value.clone()),
            None => Err(missing),
        };
    }
    // Miss — resolve the keychain once (env already handled above). Only a
    // definitive outcome is memoised: a resolved key or a *genuine* absence.
    // A transient failure (keychain locked/denied) is returned uncached so the
    // next press retries and a one-off failure self-heals — caching it would
    // pin every later press on a stale miss until `secrets://changed`.
    let (result, to_cache) = resolve_cache_outcome(read_keychain(account), missing);
    if let Some(entry) = to_cache {
        secret_cache()
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(account.to_string(), entry);
    }
    result
}

/// Cache policy for a fresh keychain read, factored out of [`get_cached`] so it
/// is unit-testable without a live keychain. Returns the caller-facing result
/// plus the cache mutation to apply: `Some(entry)` memoises `entry`, `None`
/// leaves the cache untouched. `Found`/`Absent` are definitive and cached;
/// `Unavailable` (keychain locked / ACL-denied) maps to
/// [`MuniError::KeychainUnavailable`] — NOT the provider's missing-key error —
/// and is deliberately never cached, so a locked keychain mid-dictation tells
/// the user to unlock it (matching the uncached [`get`] path) and self-heals on
/// the next press.
fn resolve_cache_outcome(
    read: KeychainRead,
    missing: MuniError,
) -> (Result<String, MuniError>, Option<Option<String>>) {
    match read {
        KeychainRead::Found(value) => (Ok(value.clone()), Some(Some(value))),
        KeychainRead::Absent => (Err(missing), Some(None)),
        KeychainRead::Unavailable => (Err(MuniError::KeychainUnavailable), None),
    }
}

/// Drop every cached secret. Called after any keychain mutation
/// (`secrets://changed`) so the next release-path read re-resolves.
pub fn invalidate_cache() {
    secret_cache()
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
}

fn missing_for_unknown_service(service: &str) -> MuniError {
    log::warn!(target: "keychain", "unknown secrets service: {service}");
    MuniError::UnknownSecretsService {
        service: service.to_string(),
    }
}

/// Process-wide lock serialising every test that mutates the shared,
/// process-global provider env vars (`MUNI_*_KEY`, the service-suffix
/// override, the Gladia POST-endpoint override). These vars are global to
/// the whole process, so tests in *different modules* (`secrets`,
/// `session`) must share ONE lock — a per-module lock lets a `secrets`
/// test clear `MUNI_GLADIA_KEY` mid-flight and fail a `session` rescue
/// test that just set it (this was the slice-4
/// `deepgram_route_rescue_*` flake: it failed ~2/3 of parallel runs, passed
/// in isolation and under `--test-threads=1`). A `tokio::sync::Mutex` so
/// async tests can `.lock().await` and sync tests can `.blocking_lock()`;
/// its guard is `Send`, so multi-thread `#[tokio::test]`s may hold it across
/// `.await` (a `std` guard is `!Send` and would not compile there).
#[cfg(test)]
pub(crate) fn env_var_test_lock() -> &'static tokio::sync::Mutex<()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_service_returns_typed_error() {
        match get("not-a-real-service") {
            Err(MuniError::UnknownSecretsService { service }) => {
                assert_eq!(service, "not-a-real-service");
            }
            other => panic!("expected UnknownSecretsService, got {other:?}"),
        }
    }

    #[test]
    fn set_rejects_empty_value() {
        match set(DEEPGRAM_ACCOUNT, "") {
            Err(MuniError::EmptyApiKey) => {}
            other => panic!("expected EmptyApiKey, got {other:?}"),
        }
    }

    /// Touches the real OS keyring — gated behind `#[ignore]` so CI
    /// without a keychain agent (or sandboxed runners) doesn't fail. Run
    /// locally with `cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml --
    /// --ignored secrets::tests::round_trip`.
    #[test]
    #[ignore]
    fn round_trip_against_real_keyring() {
        // Use a unique suffix so this test doesn't clobber a developer's
        // saved keys when the harness is run interactively.
        std::env::set_var(SERVICE_SUFFIX_ENV, "rust_round_trip_test");
        // Drop any leftover from a prior aborted run before asserting.
        let _ = delete(DEEPGRAM_ACCOUNT);
        // Ensure no env-var override masks the empty keyring read.
        std::env::remove_var(DEEPGRAM_ENV_VAR);
        assert!(!is_set(DEEPGRAM_ACCOUNT));

        set(DEEPGRAM_ACCOUNT, "sk-test-****ABCD").expect("set");
        assert!(is_set(DEEPGRAM_ACCOUNT));
        assert_eq!(get(DEEPGRAM_ACCOUNT).unwrap(), "sk-test-****ABCD");

        delete(DEEPGRAM_ACCOUNT).expect("delete");
        assert!(!is_set(DEEPGRAM_ACCOUNT));
        std::env::remove_var(SERVICE_SUFFIX_ENV);
    }

    // Env-var-mutating tests serialise on the module-level
    // `env_var_test_lock()` — shared with `session`'s Gladia-rescue tests
    // because these vars are process-global (see that fn's doc comment).

    #[test]
    fn env_override_wins_over_keyring() {
        let _guard = env_var_test_lock().blocking_lock();
        // Force a unique suffix so we don't collide with a real saved
        // key, AND set the env var. With the env var present, get()
        // must return its value without ever touching keyring (and so
        // works in sandboxed CI).
        std::env::set_var(SERVICE_SUFFIX_ENV, "rust_env_override_test");
        std::env::set_var(DEEPGRAM_ENV_VAR, "env-override-****WXYZ");
        assert_eq!(get(DEEPGRAM_ACCOUNT).unwrap(), "env-override-****WXYZ");
        std::env::remove_var(DEEPGRAM_ENV_VAR);
        std::env::remove_var(SERVICE_SUFFIX_ENV);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn is_set_treats_env_var_as_set_in_debug_builds() {
        let _guard = env_var_test_lock().blocking_lock();
        // Dev-build short-circuit: a non-empty env-var override means the
        // app has an effective key without a keychain entry, so `is_set`
        // returns true and the Settings UI doesn't fire a keychain probe
        // (which would prompt for the macOS keychain password on every
        // dev rebuild).
        std::env::set_var(SERVICE_SUFFIX_ENV, "rust_is_set_env_only");
        std::env::remove_var(DEEPGRAM_ENV_VAR);
        // No env var, no keychain entry under our unique suffix → false.
        assert!(!is_set(DEEPGRAM_ACCOUNT));
        // Env var alone drives the result to true in debug builds.
        std::env::set_var(DEEPGRAM_ENV_VAR, "env-only-****ABCD");
        assert!(is_set(DEEPGRAM_ACCOUNT));
        // Empty env var is treated as unset — same shape as `get`.
        std::env::set_var(DEEPGRAM_ENV_VAR, "");
        assert!(!is_set(DEEPGRAM_ACCOUNT));
        std::env::remove_var(DEEPGRAM_ENV_VAR);
        std::env::remove_var(SERVICE_SUFFIX_ENV);
    }

    #[test]
    fn gemini_env_override_wins_over_keyring() {
        let _guard = env_var_test_lock().blocking_lock();
        std::env::set_var(SERVICE_SUFFIX_ENV, "rust_gemini_env_override_test");
        std::env::set_var(GEMINI_ENV_VAR, "gem-env-****WXYZ");
        assert_eq!(get(GEMINI_ACCOUNT).unwrap(), "gem-env-****WXYZ");
        std::env::remove_var(GEMINI_ENV_VAR);
        std::env::remove_var(SERVICE_SUFFIX_ENV);
    }

    #[test]
    fn gladia_env_override_wins_over_keyring() {
        let _guard = env_var_test_lock().blocking_lock();
        std::env::set_var(SERVICE_SUFFIX_ENV, "rust_gladia_env_override_test");
        std::env::set_var(GLADIA_ENV_VAR, "gld-env-****WXYZ");
        assert_eq!(get(GLADIA_ACCOUNT).unwrap(), "gld-env-****WXYZ");
        std::env::remove_var(GLADIA_ENV_VAR);
        std::env::remove_var(SERVICE_SUFFIX_ENV);
    }

    #[test]
    fn gladia_missing_returns_typed_error() {
        let _guard = env_var_test_lock().blocking_lock();
        std::env::set_var(SERVICE_SUFFIX_ENV, "rust_gladia_missing_test");
        std::env::remove_var(GLADIA_ENV_VAR);
        match get(GLADIA_ACCOUNT) {
            Err(MuniError::GladiaMissingApiKey) => {}
            other => panic!("expected GladiaMissingApiKey, got {other:?}"),
        }
        std::env::remove_var(SERVICE_SUFFIX_ENV);
    }

    #[test]
    fn gemini_missing_returns_typed_error() {
        let _guard = env_var_test_lock().blocking_lock();
        std::env::set_var(SERVICE_SUFFIX_ENV, "rust_gemini_missing_test");
        std::env::remove_var(GEMINI_ENV_VAR);
        match get(GEMINI_ACCOUNT) {
            Err(MuniError::GeminiMissingApiKey) => {}
            other => panic!("expected GeminiMissingApiKey, got {other:?}"),
        }
        std::env::remove_var(SERVICE_SUFFIX_ENV);
    }

    #[test]
    fn secret_cache_memoises_and_invalidate_clears() {
        // Exercise the keychain-cache layer that `get_cached` sits on directly,
        // without a real keychain: set → cached read → change → invalidated.
        // Hold `env_var_test_lock` — the de-facto serializer for this module's
        // process-global state — so this test never races another test that
        // mutates or asserts on the same `secret_cache()`/GROQ entry.
        let _guard = env_var_test_lock().blocking_lock();
        invalidate_cache();
        secret_cache()
            .write()
            .unwrap()
            .insert(GROQ_ACCOUNT.to_string(), Some("k-****ABCD".to_string()));
        assert_eq!(
            secret_cache()
                .read()
                .unwrap()
                .get(GROQ_ACCOUNT)
                .cloned()
                .flatten()
                .as_deref(),
            Some("k-****ABCD"),
            "value should be cached after insert"
        );
        invalidate_cache();
        assert!(
            secret_cache().read().unwrap().get(GROQ_ACCOUNT).is_none(),
            "invalidate_cache must drop the entry"
        );
    }

    #[test]
    fn get_cached_env_override_is_live_and_never_cached() {
        let _guard = env_var_test_lock().blocking_lock();
        std::env::set_var(SERVICE_SUFFIX_ENV, "rust_get_cached_env_test");
        invalidate_cache();

        std::env::set_var(GROQ_ENV_VAR, "groq-A-****WXYZ");
        assert_eq!(get_cached(GROQ_ACCOUNT).unwrap(), "groq-A-****WXYZ");
        // Changing the env override is seen immediately — the cache only shields
        // the keychain IPC, never the (uncached) env layer.
        std::env::set_var(GROQ_ENV_VAR, "groq-B-****WXYZ");
        assert_eq!(get_cached(GROQ_ACCOUNT).unwrap(), "groq-B-****WXYZ");
        // And an env-served read must not pollute the keychain cache.
        assert!(
            secret_cache().read().unwrap().get(GROQ_ACCOUNT).is_none(),
            "env-override path must not write the keychain cache"
        );

        std::env::remove_var(GROQ_ENV_VAR);
        std::env::remove_var(SERVICE_SUFFIX_ENV);
        invalidate_cache();
    }

    #[test]
    fn cache_policy_memoises_found_and_absent_but_not_unavailable() {
        // Found → return the value AND memoise it.
        let (res, to_cache) = resolve_cache_outcome(
            KeychainRead::Found("k-****ABCD".into()),
            MuniError::GroqMissingApiKey,
        );
        assert_eq!(res.unwrap(), "k-****ABCD");
        assert_eq!(to_cache, Some(Some("k-****ABCD".to_string())));

        // Absent → return missing AND memoise the sticky miss.
        let (res, to_cache) =
            resolve_cache_outcome(KeychainRead::Absent, MuniError::GroqMissingApiKey);
        assert!(matches!(res, Err(MuniError::GroqMissingApiKey)));
        assert_eq!(to_cache, Some(None));

        // Unavailable (locked / ACL-denied keychain) → map to KeychainUnavailable
        // (NOT the provider's missing-key error — the hot dictation path must not
        // tell the user to "add your key" when a key is actually stored) and do
        // NOT cache, so the next press retries and a one-off failure self-heals.
        let (res, to_cache) =
            resolve_cache_outcome(KeychainRead::Unavailable, MuniError::GroqMissingApiKey);
        assert!(
            matches!(res, Err(MuniError::KeychainUnavailable)),
            "locked keychain on the cached path must surface KeychainUnavailable, not missing-key"
        );
        assert_eq!(to_cache, None, "transient failure must not be cached");
    }

    #[test]
    fn get_maps_unavailable_to_keychain_unavailable_not_missing() {
        // Slice 10 task 29 — the absent-vs-unavailable split. A genuine absence
        // stays the provider's missing-key error (user should add a key); a
        // keychain that refused to answer (locked / ACL-denied — the injected
        // `Unavailable` here stands in for a keyring error we can't force
        // deterministically in CI) maps to KeychainUnavailable so the user is
        // told to unlock the keychain, not to re-enter a key.
        match keychain_read_to_result(KeychainRead::Absent, MuniError::GroqMissingApiKey) {
            Err(MuniError::GroqMissingApiKey) => {}
            other => panic!("absent should stay missing, got {other:?}"),
        }
        match keychain_read_to_result(KeychainRead::Unavailable, MuniError::GroqMissingApiKey) {
            Err(MuniError::KeychainUnavailable) => {}
            other => panic!("unavailable should map to KeychainUnavailable, got {other:?}"),
        }
        match keychain_read_to_result(
            KeychainRead::Found("k-****ABCD".into()),
            MuniError::GroqMissingApiKey,
        ) {
            Ok(v) => assert_eq!(v, "k-****ABCD"),
            other => panic!("found should return the value, got {other:?}"),
        }
    }

    #[test]
    fn presence_unknown_service_reads_absent_not_unknown() {
        // An unknown service has no keychain entry to be locked out of, so it is
        // a genuine `Absent`, never `Unknown`.
        assert_eq!(presence("not-a-real-service"), SecretPresence::Absent);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn presence_treats_env_var_as_set_in_debug_builds() {
        // Mirrors `is_set`'s debug short-circuit: a non-empty env override is an
        // effective key, so `presence` reports `Set` without a keychain probe.
        let _guard = env_var_test_lock().blocking_lock();
        std::env::set_var(SERVICE_SUFFIX_ENV, "rust_presence_env_only");
        std::env::remove_var(DEEPGRAM_ENV_VAR);
        // No env var and no keychain entry under our unique suffix → Absent
        // (the keychain answers "no entry", which is a genuine absence).
        assert_eq!(presence(DEEPGRAM_ACCOUNT), SecretPresence::Absent);
        std::env::set_var(DEEPGRAM_ENV_VAR, "env-only-****ABCD");
        assert_eq!(presence(DEEPGRAM_ACCOUNT), SecretPresence::Set);
        // `is_set` stays consistent with `presence` (it is a thin wrapper).
        assert!(is_set(DEEPGRAM_ACCOUNT));
        std::env::remove_var(DEEPGRAM_ENV_VAR);
        std::env::remove_var(SERVICE_SUFFIX_ENV);
    }

    #[test]
    fn is_set_returns_false_for_unknown_service() {
        // Regression guard for the Settings UI bug where the Remove
        // button left the probe stuck on "saved" because `is_set` was
        // implemented as `get(...).is_ok()` and `get` consults the
        // env-var override before the keychain. The new impl probes
        // the keychain directly; an unknown service short-circuits to
        // `false` without touching env vars.
        assert!(!is_set("not-a-real-service"));
    }
}
