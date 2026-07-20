//! Crash-loop safe mode — a cross-launch net that lets a bad update self-heal.
//!
//! The release profile is `panic = "abort"` (see `Cargo.toml`), chosen so a
//! panic inside an FFI / audio callback can never unwind into Objective-C
//! (that would be UB). The price is that we cannot `catch_unwind` a bad startup
//! *in-process*: a panic on the launch path aborts before the background
//! updater — which needs ~8 s plus a live event loop — can pull a fix. So the
//! safety net has to live ACROSS launches, in persisted state.
//!
//! ## Mechanism
//!
//! Every *configured* (prod) launch [`record_launch`]s a bump to an on-disk
//! counter BEFORE any risky init runs. Once the process has survived a short
//! healthy window, [`schedule_healthy_reset`] clears the counter back to 0. A
//! launch that aborts early never reaches the reset, so the counter climbs.
//!
//! When it reaches [`SAFE_MODE_THRESHOLD`] consecutive unclean launches, the
//! next launch boots in SAFE MODE: it skips audio/hotkey/tray/ASR and arms
//! ONLY the updater, so a fix can download and apply on the following launch
//! instead of crash-looping forever. Safe mode still schedules the reset, so
//! it is **self-limiting** — a false positive (e.g. the user force-quitting
//! mid-launch a few times) costs at most one mostly-headless launch before a
//! normal boot resumes.
//!
//! Everything here fails OPEN: any I/O error reads as "0 unclean launches", so
//! a missing/unwritable marker can never trap the user in safe mode.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tauri::{AppHandle, Manager, Runtime};

/// Consecutive unclean launches that trip safe mode on the next launch. Three
/// gives genuine crash loops a clear signal while making it unlikely that
/// incidental force-quits during a slow launch reach the threshold.
pub const SAFE_MODE_THRESHOLD: u32 = 3;

/// How long the process must stay alive before a launch is deemed "clean" and
/// the unclean-launch counter is reset. Comfortably longer than the updater's
/// startup-check delay (~8 s) so a launch that gets far enough to self-update
/// also counts as healthy.
const HEALTHY_AFTER: Duration = Duration::from_secs(20);

/// Marker filename under the app-local data dir. Deliberately its own tiny file
/// (not the Tauri settings store) so a corrupt/locked store — itself a possible
/// cause of a crash loop — can't take the recovery net down with it.
const MARKER_FILE: &str = "boot_health";

/// True once the unclean-launch tally warrants a minimal, updater-only boot.
pub fn is_crash_loop(unclean_launches: u32) -> bool {
    unclean_launches >= SAFE_MODE_THRESHOLD
}

// --- Path-based core (no Tauri types, unit-tested directly) ------------------

/// Read the unclean-launch counter, treating any missing/garbage/unreadable
/// marker as 0 (fail-open).
fn read_count(path: &Path) -> u32 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// Persist `count`, creating the parent dir if needed. Best-effort: a write
/// failure is logged and swallowed (fail-open — we never block boot on it).
fn write_count(path: &Path, count: u32) {
    if let Some(parent) = path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            log::warn!(target: "boot_health", "create marker dir failed: {err}");
            return;
        }
    }
    if let Err(err) = fs::write(path, count.to_string()) {
        log::warn!(target: "boot_health", "write marker failed: {err}");
    }
}

/// Increment and persist the counter, returning the new value. Saturating, so a
/// pathological tally can't wrap around to 0 and silently disarm safe mode.
fn bump_count(path: &Path) -> u32 {
    let next = read_count(path).saturating_add(1);
    write_count(path, next);
    next
}

// --- AppHandle wrappers ------------------------------------------------------

/// Absolute path to the marker file, or `None` if the data dir can't be
/// resolved (in which case the caller falls open to a normal boot).
fn marker_path<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    match app.path().app_local_data_dir() {
        Ok(dir) => Some(dir.join(MARKER_FILE)),
        Err(err) => {
            log::warn!(target: "boot_health", "app_local_data_dir unavailable: {err}");
            None
        }
    }
}

/// Record one launch attempt: bump the on-disk counter and return the new
/// consecutive-unclean-launch count. Returns 0 if the marker path can't be
/// resolved, so an I/O problem degrades to a normal boot rather than safe mode.
pub fn record_launch<R: Runtime>(app: &AppHandle<R>) -> u32 {
    match marker_path(app) {
        Some(path) => bump_count(&path),
        None => 0,
    }
}

/// After [`HEALTHY_AFTER`] of staying alive, reset the counter to 0 — marking
/// this launch clean. If the process aborts before then, the task dies with it
/// and the counter stays elevated, which is exactly the signal safe mode keys
/// off. No-ops quietly if the marker path can't be resolved.
pub fn schedule_healthy_reset<R: Runtime>(app: &AppHandle<R>) {
    let Some(path) = marker_path(app) else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(HEALTHY_AFTER).await;
        write_count(&path, 0);
        log::info!(
            target: "boot_health",
            "launch healthy after {}s — unclean-launch counter reset",
            HEALTHY_AFTER.as_secs()
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    // Unique temp path per test (no external tempfile dep, no Date/random).
    fn temp_marker() -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("muni_boot_health_{}_{}", std::process::id(), n))
    }

    #[test]
    fn missing_marker_reads_as_zero() {
        let path = temp_marker();
        let _ = fs::remove_file(&path);
        assert_eq!(read_count(&path), 0);
    }

    #[test]
    fn garbage_marker_reads_as_zero() {
        let path = temp_marker();
        fs::write(&path, "not-a-number\n").unwrap();
        assert_eq!(read_count(&path), 0);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn bump_increments_and_persists() {
        let path = temp_marker();
        let _ = fs::remove_file(&path);
        assert_eq!(bump_count(&path), 1);
        assert_eq!(bump_count(&path), 2);
        assert_eq!(bump_count(&path), 3);
        // A fresh read sees the persisted value, not in-memory state.
        assert_eq!(read_count(&path), 3);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn reset_clears_the_tally() {
        let path = temp_marker();
        bump_count(&path);
        bump_count(&path);
        write_count(&path, 0);
        assert_eq!(read_count(&path), 0);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn bump_creates_missing_parent_dir() {
        let dir = temp_marker(); // use the unique name as a dir
        let path = dir.join("boot_health");
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(bump_count(&path), 1);
        assert_eq!(read_count(&path), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_loop_threshold() {
        assert!(!is_crash_loop(0));
        assert!(!is_crash_loop(SAFE_MODE_THRESHOLD - 1));
        assert!(is_crash_loop(SAFE_MODE_THRESHOLD));
        assert!(is_crash_loop(SAFE_MODE_THRESHOLD + 5));
    }
}
