//! Phase 10 — SQLite-backed history of completed dictations.
//!
//! Mirrors Swift v1's `HistoryStore` 1:1 in shape: one row per successful
//! paste, with the raw transcript, cleaned transcript, target app bundle id
//! (resolved via `NSWorkspace.frontmostApplication`), creation timestamp,
//! and a denormalised char count for fast sorting/filtering.
//!
//! ## Why a sync API
//!
//! `rusqlite::Connection` is `Send` but not `Sync`, and a single-row insert
//! against an on-disk SQLite file completes in well under a millisecond. The
//! orchestrator wraps each call in `tauri::async_runtime::spawn_blocking` to
//! keep the tokio executor unblocked; exposing an async surface here would
//! pull in extra plumbing for no real win.
//!
//! ## Schema versioning
//!
//! Phase 10 ships a single migration (`user_version = 1`). Future phases
//! that change shape must bump `user_version` and add an idempotent
//! migration step in [`HistoryStore::run_migrations`].

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Subdirectory under `app_local_data_dir` where the history file lives. Kept
/// alongside `settings.json` and the cleanup-prompt override so all of
/// Muni's persisted state sits in one place.
pub const HISTORY_FILENAME: &str = "history.sqlite3";

/// Schema version stamped via `PRAGMA user_version`. Bump when changing
/// shape; never recycle a previously-shipped number.
///
/// v1 → initial Phase 10 schema.
/// v2 → adds `served_by` column (plan 012) tagging which backend pasted
///       the row's transcript.
const CURRENT_SCHEMA_VERSION: i32 = 2;

/// Default value for `served_by` on rows that existed before plan 012.
/// Gladia primary is the only path that wrote rows on the Gladia
/// backend before the recovery layer existed; legacy LID paths also
/// counted as "primary" from the user's POV, so the back-compat default
/// for every pre-v2 row is `gladia-primary`.
pub const SERVED_BY_GLADIA_PRIMARY: &str = "gladia-primary";

/// Plan 012 — Groq Whisper-batch served this row's transcript. Written on ANY
/// Groq-Whisper-served press post-slice-4: both the routine audio-LID Whisper
/// route and the Deepgram-route cross-provider rescue tag this (see
/// `session.rs` `transcribe_via_whisper_or_rescue`). `served_by` alone cannot
/// distinguish the two — the tag records only that Groq Whisper produced the
/// text, not which route reached it.
pub const SERVED_BY_WHISPER_FALLBACK: &str = "whisper-fallback";

/// The local Parakeet sidecar served this row's transcript (English path,
/// `MUNI_ASR_BACKEND=parakeet`). Distinguishes on-device presses from the
/// cloud Deepgram/Whisper paths in history and dogfood inspection.
pub const SERVED_BY_PARAKEET_LOCAL: &str = "parakeet-local";

/// Plan 034 — a Deepgram press whose `finalize()` handshake failed (timeout
/// or disconnect) but for which Deepgram had already streamed `is_final`
/// transcript chunks during the press. Those chunks are recovered and pasted
/// (without auto-submit) so the thought isn't silently lost; the tag marks the
/// row as a degraded, possibly-truncated success.
pub const SERVED_BY_DEEPGRAM_PARTIAL: &str = "deepgram-partial";

/// Plan 039 (slice 4, task 11) — the cross-provider Gladia rescue served this
/// row's transcript. Distinct from [`SERVED_BY_GLADIA_PRIMARY`] (the legacy
/// "Gladia was the primary streaming backend" tag): this marks a press whose
/// primary path (Groq Whisper batch, or a dead Deepgram stream) failed and the
/// independent-infrastructure Gladia one-shot rescued it (see learned/011).
/// Splitting the tag lets the cost/QA dashboard tell "rescue fired" apart from
/// legacy Gladia-primary presses.
pub const SERVED_BY_GLADIA_RESCUE: &str = "gladia-rescue";

/// One persisted dictation. `created_at` is unix epoch seconds (UTC); the
/// frontend reconstructs a `Date` from it without having to negotiate a
/// timezone-aware string format with the backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DictationRecord {
    pub id: i64,
    pub created_at: i64,
    pub raw_text: String,
    pub cleaned_text: String,
    /// `None` when the frontmost-app probe couldn't resolve a bundle id
    /// (background app, sandbox-restricted process, or non-macOS build).
    pub target_app_bundle_id: Option<String>,
    pub char_count: i64,
    /// Plan 012 — which backend served this row's transcript. One of
    /// [`SERVED_BY_GLADIA_PRIMARY`], [`SERVED_BY_WHISPER_FALLBACK`],
    /// [`SERVED_BY_PARAKEET_LOCAL`], [`SERVED_BY_DEEPGRAM_PARTIAL`]
    /// (plan 034 — recovered partial), or [`SERVED_BY_GLADIA_RESCUE`]
    /// (plan 039 — cross-provider rescue); kept as a free-form string so a
    /// future backend label can be added without a migration.
    pub served_by: String,
}

/// Insert payload — separate type so callers can't mistakenly set `id` or
/// `created_at` (both are stamped by the store).
#[derive(Debug, Clone)]
pub struct NewDictationRecord {
    pub raw_text: String,
    pub cleaned_text: String,
    pub target_app_bundle_id: Option<String>,
    /// Plan 012 — backend that produced this row's transcript. One of the
    /// `SERVED_BY_*` constants (see [`DictationRecord::served_by`] for the full
    /// list, e.g. [`SERVED_BY_GLADIA_PRIMARY`], [`SERVED_BY_WHISPER_FALLBACK`],
    /// [`SERVED_BY_GLADIA_RESCUE`]).
    pub served_by: String,
}

/// Surface-level errors. Callers don't need rusqlite's full error tree;
/// this enum collapses transport / migration / IO failures into a stable
/// shape the IPC layer can serialise.
#[derive(Debug, Error)]
pub enum HistoryStoreError {
    #[error("Couldn't open history database: {0}")]
    Open(String),
    #[error("Couldn't migrate history database: {0}")]
    Migrate(String),
    #[error("History query failed: {0}")]
    Query(String),
}

impl HistoryStoreError {
    /// Stable wire copy for the IPC layer. The frontend shows this as-is in
    /// any error toast; logged details (rusqlite specifics) are kept on the
    /// Rust side.
    pub fn user_message(&self) -> String {
        self.to_string()
    }
}

/// Configure a freshly-opened connection onto `history.sqlite3` (plan 039
/// task 47). Three independent writer connections share this one file —
/// [`HistoryStore`], [`crate::usage_store::UsageStore`], and
/// `crate::telemetry::posthog::TelemetryQueue` — so every one of them calls
/// this immediately after `Connection::open_with_flags`, before running its
/// own migrations.
///
/// - **`journal_mode = WAL`**: the default rollback-journal mode takes an
///   exclusive file lock for the *entire* duration of an expensive
///   operation (`VACUUM` in [`HistoryStore::wipe_all`], or a large
///   `purge_older_than`), starving the other two writers for however long
///   that takes. WAL lets readers and writers proceed concurrently and only
///   briefly serializes at checkpoint/commit — see
///   <https://www.sqlite.org/wal.html>. The mode is stamped into the file
///   header, so this is a one-time switch that every later connection
///   (including future launches) inherits; re-applying it is a harmless
///   no-op.
/// - **`busy_timeout`**: WAL still allows only one writer at a time. Without
///   a timeout (the rusqlite/SQLite default is 0), a connection that loses
///   that race gets an immediate `SQLITE_BUSY` error instead of waiting its
///   turn. Real contention windows here are milliseconds, so a generous
///   timeout turns a hard failure into invisible backpressure.
/// - **`foreign_keys = ON`**: SQLite defaults FK *enforcement* off per
///   connection (independent of whether a schema declares one). Turning it
///   on makes `api_calls.session_id`'s `ON DELETE SET NULL` (declared in
///   [`crate::usage_store`]'s schema) actually fire when a purge deletes the
///   `dictation_records` row it references — previously the declared FK was
///   decorative, and a purged history row left a dangling `session_id`
///   behind in `api_calls`. This is the "pick ONE" resolution for task 47's
///   FK cleanup: enforce it, rather than drop the (otherwise correct)
///   constraint.
///
/// Public (not `pub(crate)`) so integration tests can open a raw connection
/// with the identical production pragma sequence and exercise real
/// cross-connection lock contention against a `history.sqlite3` file.
pub fn configure_shared_writer_connection(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

/// Thread-safe SQLite-backed dictation history.
///
/// Each operation acquires the inner `Mutex<Connection>` for its short
/// duration; with a single writer (the session orchestrator) and the
/// History tab as the only reader, contention is effectively zero. The
/// store is constructed once in `lib.rs::setup` and shared via
/// `Arc<HistoryStore>`.
pub struct HistoryStore {
    conn: Mutex<Connection>,
}

impl HistoryStore {
    /// Open (or create) the history database at `path`, running any
    /// pending migrations. The parent directory must already exist —
    /// `lib.rs::setup` resolves `app_local_data_dir` (which Tauri creates
    /// on first launch) before calling this.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, HistoryStoreError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path.as_ref(), flags)
            .map_err(|e| HistoryStoreError::Open(e.to_string()))?;
        configure_shared_writer_connection(&conn)
            .map_err(|e| HistoryStoreError::Open(e.to_string()))?;
        Self::run_migrations(&conn).map_err(|e| HistoryStoreError::Migrate(e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Convenience: resolve the canonical history path under
    /// `app_local_data_dir`. Centralised so production callers and tests
    /// agree on the filename without hardcoding it twice.
    pub fn default_path(app_local_data_dir: &Path) -> PathBuf {
        app_local_data_dir.join(HISTORY_FILENAME)
    }

    fn run_migrations(conn: &Connection) -> rusqlite::Result<()> {
        // PRAGMA user_version returns 0 on a fresh database, which lets us
        // distinguish "never migrated" from any future version step.
        let current: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if current >= CURRENT_SCHEMA_VERSION {
            return Ok(());
        }

        // v0 → v1: table + index. `IF NOT EXISTS` protects against a
        // partial earlier run (PRAGMA was bumped but the table create
        // failed) repeating the work harmlessly. v2 `CREATE TABLE`
        // below includes `served_by`; the column-add migration is
        // gated on the v1→v2 step so existing v1 DBs gain the column
        // exactly once.
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS dictation_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at INTEGER NOT NULL,
                raw_text TEXT NOT NULL,
                cleaned_text TEXT NOT NULL,
                target_app_bundle_id TEXT,
                char_count INTEGER NOT NULL,
                served_by TEXT NOT NULL DEFAULT 'gladia-primary'
            );
            CREATE INDEX IF NOT EXISTS idx_dictation_records_created_at
                ON dictation_records (created_at DESC);
            ",
        )?;

        // v1 → v2 (plan 012): add `served_by` column. Existing rows
        // default to `gladia-primary` — the only backend that wrote
        // rows on the Gladia path before the recovery layer existed,
        // and the historically-correct "primary path" tag for the
        // legacy LID rows too. SQLite requires NOT NULL columns added
        // via ALTER TABLE to have a DEFAULT — the literal here matches
        // [`SERVED_BY_GLADIA_PRIMARY`]; the CREATE TABLE above embeds
        // the same default so fresh DBs get the column without the
        // migration ever running. The column may already exist if the
        // CREATE TABLE above just created the table (fresh DB on v2);
        // the ALTER is guarded so we only run it on a true v1 → v2
        // upgrade.
        if current < 2 {
            let column_exists: i32 = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('dictation_records') WHERE name = 'served_by'",
                [],
                |row| row.get(0),
            )?;
            if column_exists == 0 {
                conn.execute(
                    "ALTER TABLE dictation_records \
                     ADD COLUMN served_by TEXT NOT NULL DEFAULT 'gladia-primary'",
                    [],
                )?;
            }
        }

        conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)?;
        Ok(())
    }

    /// Insert a new record, stamping `created_at` to the current unix
    /// time and `char_count` to the cleaned text length. Returns the
    /// generated row id.
    pub fn insert(&self, record: NewDictationRecord) -> Result<i64, HistoryStoreError> {
        let now = unix_seconds_now();
        let char_count = record.cleaned_text.chars().count() as i64;
        let conn = self.conn.lock().expect("history conn poisoned");
        conn.execute(
            "INSERT INTO dictation_records (
                created_at, raw_text, cleaned_text, target_app_bundle_id, char_count, served_by
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                now,
                record.raw_text,
                record.cleaned_text,
                record.target_app_bundle_id,
                char_count,
                record.served_by,
            ],
        )
        .map_err(|e| HistoryStoreError::Query(e.to_string()))?;
        Ok(conn.last_insert_rowid())
    }

    /// Return up to `limit` records in reverse-chronological order. A
    /// `limit` of `None` returns everything.
    pub fn list(&self, limit: Option<i64>) -> Result<Vec<DictationRecord>, HistoryStoreError> {
        let conn = self.conn.lock().expect("history conn poisoned");
        let mut sql = String::from(
            "SELECT id, created_at, raw_text, cleaned_text, target_app_bundle_id, char_count, served_by
             FROM dictation_records
             ORDER BY created_at DESC, id DESC",
        );
        if limit.is_some() {
            sql.push_str(" LIMIT ?1");
        }
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| HistoryStoreError::Query(e.to_string()))?;
        let mapper = |row: &rusqlite::Row<'_>| -> rusqlite::Result<DictationRecord> {
            Ok(DictationRecord {
                id: row.get(0)?,
                created_at: row.get(1)?,
                raw_text: row.get(2)?,
                cleaned_text: row.get(3)?,
                target_app_bundle_id: row.get(4)?,
                char_count: row.get(5)?,
                served_by: row.get(6)?,
            })
        };
        let rows: Result<Vec<DictationRecord>, _> = match limit {
            Some(n) => stmt
                .query_map(params![n], mapper)
                .map_err(|e| HistoryStoreError::Query(e.to_string()))?
                .collect(),
            None => stmt
                .query_map([], mapper)
                .map_err(|e| HistoryStoreError::Query(e.to_string()))?
                .collect(),
        };
        rows.map_err(|e| HistoryStoreError::Query(e.to_string()))
    }

    /// The single most-recent record, or `None` when history is empty. Thin
    /// convenience over the newest-first [`Self::list`] — the re-paste hotkey
    /// (Feature 037) reads this to reinject the user's last dictation.
    pub fn latest(&self) -> Result<Option<DictationRecord>, HistoryStoreError> {
        Ok(self.list(Some(1))?.into_iter().next())
    }

    /// Total record count. Cheap (uses the table's automatic rowid count)
    /// — safe to call on every UI render.
    pub fn count(&self) -> Result<i64, HistoryStoreError> {
        let conn = self.conn.lock().expect("history conn poisoned");
        conn.query_row("SELECT COUNT(*) FROM dictation_records", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|e| HistoryStoreError::Query(e.to_string()))
    }

    /// Look up a single record by id. Returns `Ok(None)` if no row matches.
    pub fn get(&self, id: i64) -> Result<Option<DictationRecord>, HistoryStoreError> {
        let conn = self.conn.lock().expect("history conn poisoned");
        conn.query_row(
            "SELECT id, created_at, raw_text, cleaned_text, target_app_bundle_id, char_count, served_by
             FROM dictation_records WHERE id = ?1",
            params![id],
            |row| {
                Ok(DictationRecord {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    raw_text: row.get(2)?,
                    cleaned_text: row.get(3)?,
                    target_app_bundle_id: row.get(4)?,
                    char_count: row.get(5)?,
                    served_by: row.get(6)?,
                })
            },
        )
        .optional()
        .map_err(|e| HistoryStoreError::Query(e.to_string()))
    }

    /// Delete a single record by id. Idempotent — returns the number of
    /// rows actually affected (0 if `id` was already gone).
    pub fn delete(&self, id: i64) -> Result<usize, HistoryStoreError> {
        let conn = self.conn.lock().expect("history conn poisoned");
        conn.execute("DELETE FROM dictation_records WHERE id = ?1", params![id])
            .map_err(|e| HistoryStoreError::Query(e.to_string()))
    }

    /// Drop every record. Returns the number of rows deleted. Used by the
    /// History tab's "Wipe history" control.
    ///
    /// This is the user's explicit "delete my data" action, so we follow the
    /// delete with `VACUUM` to physically reclaim the freed pages. A plain
    /// `DELETE` leaves the old transcript bytes lingering in the database
    /// file's free list, where they stay forensically recoverable until
    /// overwritten; `VACUUM` rewrites the file without them. The cost is
    /// acceptable because this runs only on an explicit, infrequent user
    /// action (unlike `purge_older_than`, which fires on every launch).
    pub fn wipe_all(&self) -> Result<usize, HistoryStoreError> {
        let conn = self.conn.lock().expect("history conn poisoned");
        let deleted = conn
            .execute("DELETE FROM dictation_records", [])
            .map_err(|e| HistoryStoreError::Query(e.to_string()))?;
        // VACUUM cannot run inside a transaction; this connection is in
        // autocommit mode here, so the bare execute is correct.
        conn.execute("VACUUM", [])
            .map_err(|e| HistoryStoreError::Query(e.to_string()))?;
        Ok(deleted)
    }

    /// Drop records older than `days` (relative to "now"). A retention
    /// of 0 wipes everything; the IPC boundary guards against that case
    /// upstream so the user's slider can't accidentally erase history.
    /// Returns the number of rows deleted.
    ///
    /// Because [`configure_shared_writer_connection`] turns on
    /// `foreign_keys` enforcement, deleting a `dictation_records` row here
    /// also fires `api_calls.session_id`'s `ON DELETE SET NULL` for any
    /// cost-tracking row that referenced it — no orphaned session ids.
    pub fn purge_older_than(&self, days: u32) -> Result<usize, HistoryStoreError> {
        let cutoff = unix_seconds_now() - (days as i64 * SECONDS_PER_DAY);
        let conn = self.conn.lock().expect("history conn poisoned");
        conn.execute(
            "DELETE FROM dictation_records WHERE created_at < ?1",
            params![cutoff],
        )
        .map_err(|e| HistoryStoreError::Query(e.to_string()))
    }
}

const SECONDS_PER_DAY: i64 = 86_400;

fn unix_seconds_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        // Pre-1970 clocks don't exist outside contrived test setups; saturate
        // to 0 instead of panicking so the orchestrator can keep running.
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Frontmost-application probe
// ---------------------------------------------------------------------------

/// Return the bundle id of the frontmost macOS application, or `None` when
/// it can't be resolved (no frontmost app, sandboxed background process,
/// non-macOS build). Mirrors Swift v1's
/// `NSWorkspace.shared.frontmostApplication?.bundleIdentifier`.
///
/// Called once per successful press just before paste so the recorded
/// `target_app_bundle_id` reflects the app the user was looking at when
/// they triggered dictation — not the app focused by some later
/// background change.
#[cfg(target_os = "macos")]
pub fn frontmost_app_bundle_id() -> Option<String> {
    use objc2_app_kit::NSWorkspace;

    // The objc2 typed bindings for `sharedWorkspace`,
    // `frontmostApplication`, and `bundleIdentifier` are safe — they
    // wrap thread-safe singletons / read-only accessors and return
    // `Retained<_>` smart pointers that live as long as the local
    // binding. `to_string` copies the UTF-16 contents into an owned
    // Rust `String` so the result outlives the autorelease pool.
    let workspace = NSWorkspace::sharedWorkspace();
    let app = workspace.frontmostApplication()?;
    let bundle_id = app.bundleIdentifier()?;
    Some(bundle_id.to_string())
}

#[cfg(not(target_os = "macos"))]
pub fn frontmost_app_bundle_id() -> Option<String> {
    None
}

/// Resolve a macOS bundle identifier (e.g. `com.mitchellh.ghostty`) to
/// the user-visible application name (e.g. `Ghostty`). Returns `None`
/// when the app isn't installed or the bundle id is unknown.
///
/// Uses `NSWorkspace.URLForApplicationWithBundleIdentifier:` and strips
/// the `.app` suffix from the resolved URL's last path component. This
/// is cheap (Launch Services lookup) — the History pane can call it
/// once per unique bundle id and cache the result.
#[cfg(target_os = "macos")]
pub fn display_name_for_bundle_id(bundle_id: &str) -> Option<String> {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::NSString;

    if bundle_id.is_empty() {
        return None;
    }
    let ns_id = NSString::from_str(bundle_id);
    let workspace = NSWorkspace::sharedWorkspace();
    let url = workspace.URLForApplicationWithBundleIdentifier(&ns_id)?;
    let last = url.lastPathComponent()?;
    let raw = last.to_string();
    let trimmed = raw.strip_suffix(".app").unwrap_or(raw.as_str());
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(not(target_os = "macos"))]
pub fn display_name_for_bundle_id(_bundle_id: &str) -> Option<String> {
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fresh_store() -> (HistoryStore, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let path = HistoryStore::default_path(dir.path());
        let store = HistoryStore::open(&path).expect("open store");
        (store, dir)
    }

    fn sample(text: &str) -> NewDictationRecord {
        NewDictationRecord {
            raw_text: format!("raw {text}"),
            cleaned_text: text.to_string(),
            target_app_bundle_id: Some("com.example.editor".into()),
            served_by: SERVED_BY_GLADIA_PRIMARY.into(),
        }
    }

    #[test]
    fn fresh_db_has_zero_records_and_current_schema_version() {
        let (store, _dir) = fresh_store();
        assert_eq!(store.count().unwrap(), 0);
        // Re-opening must be a no-op (idempotent migration).
        let conn = store.conn.lock().unwrap();
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn insert_then_list_round_trips_record() {
        let (store, _dir) = fresh_store();
        let id = store.insert(sample("hello world")).unwrap();
        assert!(id > 0);

        let rows = store.list(None).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.id, id);
        assert_eq!(row.cleaned_text, "hello world");
        assert_eq!(row.raw_text, "raw hello world");
        assert_eq!(
            row.target_app_bundle_id.as_deref(),
            Some("com.example.editor")
        );
        assert_eq!(row.char_count, "hello world".chars().count() as i64);
        assert!(row.created_at > 0);
    }

    #[test]
    fn list_orders_newest_first() {
        let (store, _dir) = fresh_store();
        let id_a = store.insert(sample("first")).unwrap();
        // Tiny sleep so created_at differs across the two inserts; without
        // it both rows can land in the same second and the secondary sort
        // (id DESC) is what guarantees order — exercise both paths by
        // making ids and timestamps disagree.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let id_b = store.insert(sample("second")).unwrap();

        let rows = store.list(None).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, id_b, "newest record first; got {rows:?}");
        assert_eq!(rows[1].id, id_a);
    }

    #[test]
    fn list_with_limit_caps_returned_rows() {
        let (store, _dir) = fresh_store();
        for i in 0..5 {
            store.insert(sample(&format!("entry-{i}"))).unwrap();
        }
        let rows = store.list(Some(3)).unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn delete_removes_only_the_targeted_row() {
        let (store, _dir) = fresh_store();
        let id_a = store.insert(sample("a")).unwrap();
        let id_b = store.insert(sample("b")).unwrap();

        let n = store.delete(id_a).unwrap();
        assert_eq!(n, 1);
        let n = store.delete(id_a).unwrap();
        assert_eq!(n, 0, "delete of vanished row is idempotent");

        let rows = store.list(None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id_b);
    }

    #[test]
    fn wipe_all_clears_every_row() {
        let (store, _dir) = fresh_store();
        for i in 0..4 {
            store.insert(sample(&format!("row-{i}"))).unwrap();
        }
        let n = store.wipe_all().unwrap();
        assert_eq!(n, 4);
        assert_eq!(store.count().unwrap(), 0);
    }

    #[test]
    fn wipe_all_vacuums_freed_pages() {
        // Regression guard for the "delete my data" forensic-residue fix:
        // a bare DELETE leaves the old transcript bytes in the database's
        // free list. wipe_all() must VACUUM so no freed pages remain, which
        // is the observable proxy for the stale bytes having been rewritten
        // out of the file. Insert enough rows to force the DB to grow past
        // its initial page so the free list would be non-empty without the
        // VACUUM.
        let (store, _dir) = fresh_store();
        for i in 0..200 {
            store
                .insert(sample(&format!("a fairly long transcript body number {i}")))
                .unwrap();
        }
        store.wipe_all().unwrap();
        let conn = store.conn.lock().unwrap();
        let freelist: i64 = conn
            .query_row("PRAGMA freelist_count", [], |row| row.get(0))
            .unwrap();
        assert_eq!(freelist, 0, "VACUUM should leave no freed pages behind");
    }

    #[test]
    fn purge_older_than_keeps_recent_drops_old() {
        let (store, _dir) = fresh_store();

        // Recent record: insert via the public API so created_at = now.
        let recent_id = store.insert(sample("recent")).unwrap();
        // Stale record: backdate by writing through the connection
        // directly so we can simulate a row from 60 days ago without
        // waiting two months in test wall-clock time.
        let cutoff = unix_seconds_now() - 60 * SECONDS_PER_DAY;
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO dictation_records (
                    created_at, raw_text, cleaned_text, target_app_bundle_id, char_count, served_by
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    cutoff,
                    "raw stale",
                    "stale",
                    "com.example.bg",
                    5,
                    SERVED_BY_GLADIA_PRIMARY,
                ],
            )
            .unwrap();
        }
        assert_eq!(store.count().unwrap(), 2);

        let removed = store.purge_older_than(30).unwrap();
        assert_eq!(removed, 1);
        let rows = store.list(None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, recent_id);
    }

    #[test]
    fn latest_returns_none_on_empty_and_newest_after_inserts() {
        let (store, _dir) = fresh_store();
        // Empty history → None (the re-paste hotkey no-ops on this).
        assert!(store.latest().unwrap().is_none());

        store.insert(sample("older")).unwrap();
        // Force a distinct created_at so ordering is unambiguous even when the
        // two inserts would otherwise share a wall-clock second.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let newer_id = store.insert(sample("newer")).unwrap();

        let latest = store.latest().unwrap().expect("history is non-empty");
        assert_eq!(latest.id, newer_id);
        assert_eq!(latest.cleaned_text, "newer");
    }

    #[test]
    fn get_returns_optional_record_for_unknown_id() {
        let (store, _dir) = fresh_store();
        assert!(store.get(9999).unwrap().is_none());
        let id = store.insert(sample("alpha")).unwrap();
        assert_eq!(store.get(id).unwrap().unwrap().cleaned_text, "alpha");
    }

    #[test]
    fn record_serialises_to_camel_case_for_ipc() {
        let record = DictationRecord {
            id: 7,
            created_at: 1_700_000_000,
            raw_text: "raw".into(),
            cleaned_text: "clean".into(),
            target_app_bundle_id: Some("com.example.app".into()),
            char_count: 5,
            served_by: SERVED_BY_GLADIA_PRIMARY.into(),
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"createdAt\":1700000000"));
        assert!(json.contains("\"targetAppBundleId\":\"com.example.app\""));
        assert!(json.contains("\"charCount\":5"));
        assert!(json.contains("\"servedBy\":\"gladia-primary\""));
    }

    #[test]
    fn store_reopens_existing_database_without_loss() {
        let dir = tempdir().expect("tempdir");
        let path = HistoryStore::default_path(dir.path());

        let store = HistoryStore::open(&path).unwrap();
        store.insert(sample("persisted")).unwrap();
        drop(store);

        let store2 = HistoryStore::open(&path).unwrap();
        let rows = store2.list(None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cleaned_text, "persisted");
    }

    #[test]
    fn null_bundle_id_round_trips_as_none() {
        let (store, _dir) = fresh_store();
        let id = store
            .insert(NewDictationRecord {
                raw_text: "raw".into(),
                cleaned_text: "clean".into(),
                target_app_bundle_id: None,
                served_by: SERVED_BY_GLADIA_PRIMARY.into(),
            })
            .unwrap();
        let row = store.get(id).unwrap().unwrap();
        assert!(row.target_app_bundle_id.is_none());
    }

    #[test]
    fn whisper_fallback_tag_round_trips() {
        // Plan 012 — a recovered press is tagged so the History UI can
        // surface a "served by Whisper" indicator and the dev cost
        // pane can correlate rows with the api_calls fallback entries.
        let (store, _dir) = fresh_store();
        let id = store
            .insert(NewDictationRecord {
                raw_text: "raw".into(),
                cleaned_text: "clean".into(),
                target_app_bundle_id: None,
                served_by: SERVED_BY_WHISPER_FALLBACK.into(),
            })
            .unwrap();
        let row = store.get(id).unwrap().unwrap();
        assert_eq!(row.served_by, SERVED_BY_WHISPER_FALLBACK);

        // list() must also surface the column.
        let rows = store.list(None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].served_by, SERVED_BY_WHISPER_FALLBACK);
    }

    #[test]
    fn v1_database_migrates_to_v2_with_default_served_by() {
        // Build a v1-shaped DB by hand (no served_by column, PRAGMA
        // user_version = 1), then re-open through HistoryStore and
        // assert the migration backfills `served_by = 'gladia-primary'`
        // on existing rows.
        let dir = tempdir().expect("tempdir");
        let path = HistoryStore::default_path(dir.path());

        {
            let conn = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .expect("open v1 db");
            conn.execute_batch(
                "CREATE TABLE dictation_records (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    created_at INTEGER NOT NULL,
                    raw_text TEXT NOT NULL,
                    cleaned_text TEXT NOT NULL,
                    target_app_bundle_id TEXT,
                    char_count INTEGER NOT NULL
                );",
            )
            .expect("create v1 table");
            conn.execute(
                "INSERT INTO dictation_records (
                    created_at, raw_text, cleaned_text, target_app_bundle_id, char_count
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    1_700_000_000_i64,
                    "raw legacy",
                    "legacy",
                    "com.example.x",
                    6
                ],
            )
            .expect("insert legacy row");
            conn.pragma_update(None, "user_version", 1_i32)
                .expect("stamp v1");
        }

        let store = HistoryStore::open(&path).expect("migrate v1 → v2");
        let rows = store.list(None).expect("list after migrate");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].served_by, SERVED_BY_GLADIA_PRIMARY);

        // Schema-version stamp advanced.
        let conn = store.conn.lock().unwrap();
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION);
    }
}
