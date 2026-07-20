//! Anonymous install identifier (Feature 033).
//!
//! Telemetry attaches **one** stable identifier to every event: a random v4
//! UUID generated on first launch and persisted under `app_local_data_dir`.
//! This UUID is the ONLY identifier ever sent off-device — never an email, a
//! license key, a machine name, or anything derivable from the user. It lets
//! Sentry de-duplicate a single install's crashes and PostHog compute
//! retention, without identifying the person behind the install.
//!
//! Everything here fails OPEN: if the file can't be read or written (locked
//! dir, full disk, permissions), we return a fresh ephemeral UUID rather than
//! blocking boot. The cost of a failure is only that a transient install looks
//! like a new one to the dashboards — never a crash and never a leak.

use std::fs;
use std::path::Path;

use uuid::Uuid;

/// Filename under the app-local data dir holding the persisted install UUID.
/// Its own tiny file (not the Tauri settings store) so a corrupt/locked store
/// can never take the identifier — or telemetry init — down with it.
const INSTALL_ID_FILE: &str = "telemetry_install_id";

/// Load the persisted install UUID, or generate-and-persist a new one.
///
/// Resolution: read `telemetry_install_id`; if present and a valid UUID, return
/// it. Otherwise generate a fresh v4 UUID, best-effort persist it, and return
/// it. Any I/O error is logged and swallowed (fail-open to an ephemeral UUID).
pub fn load_or_create(app_local_data_dir: &Path) -> String {
    let path = app_local_data_dir.join(INSTALL_ID_FILE);
    load_or_create_at(&path)
}

/// Path-based core, unit-tested directly without Tauri types.
fn load_or_create_at(path: &Path) -> String {
    if let Some(existing) = read_valid(path) {
        return existing;
    }
    let fresh = Uuid::new_v4().to_string();
    persist(path, &fresh);
    fresh
}

/// Read the file and return its contents only if they parse as a UUID. A
/// missing/unreadable/garbage file reads as `None` so the caller regenerates.
fn read_valid(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    // Validate so a truncated/garbage write doesn't become a permanent,
    // malformed distinct_id. Re-serialise the parsed value to normalise.
    Uuid::parse_str(trimmed)
        .ok()
        .map(|u| u.hyphenated().to_string())
}

/// Best-effort persist; a write failure is logged and swallowed (fail-open —
/// boot is never blocked on the identifier file).
fn persist(path: &Path, id: &str) {
    if let Some(parent) = path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            log::warn!(target: "telemetry", "create install-id dir failed: {err}");
            return;
        }
    }
    if let Err(err) = fs::write(path, id) {
        log::warn!(target: "telemetry", "persist install-id failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    // Unique temp path per test (no external tempfile dep, mirrors boot_health).
    fn temp_path() -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("muni_install_id_{}_{}", std::process::id(), n))
    }

    #[test]
    fn generates_and_persists_a_valid_uuid_when_absent() {
        let path = temp_path();
        let _ = fs::remove_file(&path);

        let id = load_or_create_at(&path);
        assert!(Uuid::parse_str(&id).is_ok(), "returns a valid UUID");
        // It was written to disk.
        let on_disk = fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk.trim(), id);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn returns_the_same_id_on_a_second_load() {
        let path = temp_path();
        let _ = fs::remove_file(&path);

        let first = load_or_create_at(&path);
        let second = load_or_create_at(&path);
        assert_eq!(first, second, "stable across launches");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn regenerates_when_file_holds_garbage() {
        let path = temp_path();
        fs::write(&path, "not-a-uuid\n").unwrap();

        let id = load_or_create_at(&path);
        assert!(Uuid::parse_str(&id).is_ok());
        // The garbage was overwritten with the fresh valid UUID.
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), id);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn never_returns_email_or_license_shaped_value() {
        // Guardrail: the identifier is a bare UUID — no `@`, no key prefix.
        let path = temp_path();
        let _ = fs::remove_file(&path);
        let id = load_or_create_at(&path);
        assert!(!id.contains('@'));
        assert!(!id.contains(' '));
        assert_eq!(id.len(), 36, "hyphenated v4 UUID length");
        let _ = fs::remove_file(&path);
    }
}
