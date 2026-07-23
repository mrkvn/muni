//! SQLite-backed per-call usage + price-history store (feature 005).
//!
//! Sibling of [`crate::history_store::HistoryStore`] — both stores live
//! inside the same `history.sqlite3` file so the user has a single
//! "Muni state" file to reason about. The history store owns the
//! `dictation_records` table and `PRAGMA user_version`; this store
//! owns:
//!
//! - `api_calls` — one row per provider call, with `cost_usd` frozen
//!   at insert time.
//! - `price_history` — one row per `(effective_month, provider,
//!   model)`, written by [`crate::prices_refresher`].
//! - `app_metadata` — string key/value pairs scoped to the cost
//!   tracking subsystem (`usage.schema_version`,
//!   `usage.last_priced_success_at`, `usage.last_priced_attempt_at`).
//!
//! ## Why a separate schema-version key
//!
//! `PRAGMA user_version` is owned by `HistoryStore`. Bumping it from
//! two stores would lose information across reopens. We instead stamp
//! `app_metadata.value WHERE key = 'usage.schema_version'` so each
//! subsystem migrates independently against the same SQLite file.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::history_store::configure_shared_writer_connection;
use crate::pricing::PriceKind;

/// Retention window for `api_calls` rows (plan 039 task 47). Unlike
/// dictation history, cost-tracking has no user-facing retention control —
/// 180 days comfortably covers the "Cost & Usage" panel's rolling
/// month-over-month view while keeping the table from growing unbounded on
/// a long-lived install. Purged by [`UsageStore::purge_api_calls_older_than`]
/// at launch and on `lib.rs`'s periodic retention-purge task, mirroring
/// [`crate::history_store::HistoryStore::purge_older_than`].
pub const API_CALLS_RETENTION_DAYS: u32 = 180;

const SECONDS_PER_DAY: i64 = 86_400;

fn unix_seconds_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        // Pre-1970 clocks don't exist outside contrived test setups; saturate
        // to 0 instead of panicking so purge/insert callers keep running.
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Schema version stamped under `app_metadata.usage.schema_version`.
/// Bump when changing shape; never recycle a previously-shipped number.
///
/// - v1: `api_calls`, `price_history`, `app_metadata`.
/// - v2: `press_timings` (plan 041 — per-press phase-timing ledger).
const CURRENT_SCHEMA_VERSION: i32 = 2;

/// `app_metadata` key holding the cost-tracking schema version.
const SCHEMA_VERSION_KEY: &str = "usage.schema_version";

/// `app_metadata` key holding the unix timestamp (UTC seconds) of the
/// most recent successful prices fetch. Used by
/// [`crate::prices_refresher`] to decide whether the calendar month
/// has rolled over since the last refresh.
pub const LAST_PRICED_SUCCESS_KEY: &str = "usage.last_priced_success_at";

/// `app_metadata` key holding the unix timestamp of the most recent
/// fetch *attempt*, regardless of outcome. Surfaced for diagnostics.
pub const LAST_PRICED_ATTEMPT_KEY: &str = "usage.last_priced_attempt_at";

/// Surface-level errors. Same shape as `HistoryStoreError` so the IPC
/// layer can collapse to a stable string without leaking rusqlite
/// internals.
#[derive(Debug, Error)]
pub enum UsageStoreError {
    #[error("Couldn't open usage database: {0}")]
    Open(String),
    #[error("Couldn't migrate usage database: {0}")]
    Migrate(String),
    #[error("Usage query failed: {0}")]
    Query(String),
}

impl UsageStoreError {
    pub fn user_message(&self) -> String {
        self.to_string()
    }
}

/// Insert payload for `api_calls`. The store stamps `created_at` to
/// the unix-seconds time of the actual API call (passed in by the
/// caller, who owns the wall-clock at the moment the response landed)
/// and computes `cost_usd` against the price for that month — both
/// inside the writer task.
#[derive(Debug, Clone)]
pub struct NewApiCall {
    pub created_at: i64,
    pub provider: String,
    pub model: String,
    pub call_kind: String,
    pub audio_seconds: Option<f64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub latency_ms: Option<i64>,
    pub status: String,
    pub request_id: Option<String>,
    pub session_id: Option<i64>,
}

/// Insert payload for `press_timings` (plan 041). One row per completed
/// press; phase fields are `None` when the route skipped that phase (see
/// the `press_timings` DDL note). `created_at` is the unix-seconds time
/// of the press, `session_id` the `dictation_records` id when history
/// persisted, else `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct NewPressTiming {
    pub created_at: i64,
    pub session_id: Option<i64>,
    pub route: String,
    pub audio_ms: Option<i64>,
    pub asr_ms: Option<i64>,
    pub lid_wait_ms: Option<i64>,
    pub cleanup_ms: Option<i64>,
    pub inject_ms: Option<i64>,
    pub total_ms: i64,
}

/// One row of `price_history` — also the IPC shape returned by the
/// dev-only `usage_prices_list_current_month` command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PriceRow {
    /// `YYYY-MM` UTC.
    pub effective_month: String,
    pub provider: String,
    pub model: String,
    pub kind: String,
    pub usd_per_second: Option<f64>,
    pub usd_per_input_token: Option<f64>,
    pub usd_per_output_token: Option<f64>,
    pub source_url: Option<String>,
    pub fetched_at: i64,
}

/// Per-provider summary returned by [`UsageStore::summary_for_month`].
/// `total_usd` is `None` only when **every** matching row had a NULL
/// `cost_usd` (price unknown for that month).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSummary {
    pub provider: String,
    pub total_usd: Option<f64>,
    pub call_count: i64,
}

/// One calendar month's per-provider rollup, returned by
/// [`UsageStore::summary_by_month`]. Bundles the same [`ProviderSummary`]
/// rows [`UsageStore::summary_for_month`] yields for a single month, so the
/// Cost & Usage panel can render its full month history from one query.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MonthlySummary {
    /// `YYYY-MM` in the machine's local timezone.
    pub year_month: String,
    pub provider_totals: Vec<ProviderSummary>,
}

/// Per-model summary used by the dev pane. Same shape as
/// [`ProviderSummary`] with model-level granularity + raw usage units.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelSummary {
    pub provider: String,
    pub model: String,
    pub total_usd: Option<f64>,
    pub call_count: i64,
    pub audio_seconds: Option<f64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
}

/// SQLite datetime modifier that maps the UTC `created_at` column into
/// the calendar month the user perceives on the machine running the app.
/// `'localtime'` applies the OS timezone — DST-aware and resolved
/// per-timestamp — so the Cost & Usage rollup follows wherever the app
/// is used, with no configuration. The stored timestamp stays UTC
/// unix-seconds; the offset is applied only at read time.
const LOCAL_MONTH_MODIFIER: &str = "localtime";

/// Thread-safe SQLite-backed cost-tracking store.
///
/// Holds its own `Mutex<Connection>` even though `HistoryStore` already
/// owns one against the same file — SQLite's file-locking handles
/// cross-connection writes, and keeping the two stores' connections
/// separate keeps lock-hold times short (a long history `purge_older_than`
/// won't block a usage insert).
pub struct UsageStore {
    conn: Mutex<Connection>,
}

impl UsageStore {
    /// Open (or create) the usage tables alongside the history tables
    /// at `path`. Designed to be invoked AFTER `HistoryStore::open` so
    /// `user_version` is already locked to whatever value the history
    /// store set.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, UsageStoreError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path.as_ref(), flags)
            .map_err(|e| UsageStoreError::Open(e.to_string()))?;
        configure_shared_writer_connection(&conn)
            .map_err(|e| UsageStoreError::Open(e.to_string()))?;
        Self::run_migrations(&conn).map_err(|e| UsageStoreError::Migrate(e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Convenience: SQLite path matches `HistoryStore::default_path`
    /// (deliberately — both stores share the same file).
    pub fn default_path(app_local_data_dir: &Path) -> PathBuf {
        crate::history_store::HistoryStore::default_path(app_local_data_dir)
    }

    fn run_migrations(conn: &Connection) -> rusqlite::Result<()> {
        // `app_metadata` first — every other migration step reads it.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_metadata (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;

        let current = read_schema_version(conn)?;
        if current >= CURRENT_SCHEMA_VERSION {
            return Ok(());
        }

        // Each `current < N` block is a forward-only migration step,
        // idempotent via `IF NOT EXISTS` so a partial earlier run that
        // crashed between `execute_batch` and the version stamp can
        // repeat safely. Steps run in order against both fresh DBs
        // (`current == 0`) and any older shipped version.

        if current < 1 {
            // Schema v1 — three tables + two indexes.
            conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS api_calls (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    created_at INTEGER NOT NULL,
                    provider TEXT NOT NULL,
                    model TEXT NOT NULL,
                    call_kind TEXT NOT NULL,
                    audio_seconds REAL,
                    input_tokens INTEGER,
                    output_tokens INTEGER,
                    cost_usd REAL,
                    latency_ms INTEGER,
                    status TEXT NOT NULL,
                    request_id TEXT,
                    session_id INTEGER REFERENCES dictation_records(id) ON DELETE SET NULL
                );
                CREATE INDEX IF NOT EXISTS idx_api_calls_created_at
                    ON api_calls (created_at);
                CREATE INDEX IF NOT EXISTS idx_api_calls_provider_month
                    ON api_calls (provider, created_at);

                CREATE TABLE IF NOT EXISTS price_history (
                    effective_month       TEXT NOT NULL,
                    provider              TEXT NOT NULL,
                    model                 TEXT NOT NULL,
                    kind                  TEXT NOT NULL,
                    usd_per_second        REAL,
                    usd_per_input_token   REAL,
                    usd_per_output_token  REAL,
                    source_url            TEXT,
                    fetched_at            INTEGER NOT NULL,
                    PRIMARY KEY (effective_month, provider, model)
                );
                ",
            )?;
        }

        if current < 2 {
            // Schema v2 (plan 041) — per-press phase-timing ledger.
            // Phase columns are nullable on purpose: a NULL means "that
            // phase did not occur on this route" (e.g. `lid_wait_ms` on
            // english-fast presses), never zero. `route` and `total_ms`
            // are the only NOT NULL data columns — every press has both.
            conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS press_timings (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    created_at INTEGER NOT NULL,
                    session_id INTEGER REFERENCES dictation_records(id) ON DELETE SET NULL,
                    route TEXT NOT NULL,
                    audio_ms INTEGER,
                    asr_ms INTEGER,
                    lid_wait_ms INTEGER,
                    cleanup_ms INTEGER,
                    inject_ms INTEGER,
                    total_ms INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_press_timings_created_at
                    ON press_timings (created_at);
                ",
            )?;
        }

        write_schema_version(conn, CURRENT_SCHEMA_VERSION)?;
        Ok(())
    }

    /// Insert a single `api_calls` row. Returns the generated row id.
    pub fn insert_call(&self, call: NewApiCall) -> Result<i64, UsageStoreError> {
        let conn = self.conn.lock().expect("usage conn poisoned");
        conn.execute(
            "INSERT INTO api_calls (
                created_at, provider, model, call_kind,
                audio_seconds, input_tokens, output_tokens, cost_usd,
                latency_ms, status, request_id, session_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                call.created_at,
                call.provider,
                call.model,
                call.call_kind,
                call.audio_seconds,
                call.input_tokens,
                call.output_tokens,
                call.cost_usd,
                call.latency_ms,
                call.status,
                call.request_id,
                call.session_id,
            ],
        )
        .map_err(|e| UsageStoreError::Query(e.to_string()))?;
        Ok(conn.last_insert_rowid())
    }

    /// Drop `api_calls` rows older than `days` (relative to "now"). Mirrors
    /// [`crate::history_store::HistoryStore::purge_older_than`]; run at
    /// launch and on `lib.rs`'s periodic retention-purge task so a
    /// long-lived install's cost-tracking table doesn't grow unbounded.
    /// Returns the number of rows deleted.
    pub fn purge_api_calls_older_than(&self, days: u32) -> Result<usize, UsageStoreError> {
        let cutoff = unix_seconds_now() - (days as i64 * SECONDS_PER_DAY);
        let conn = self.conn.lock().expect("usage conn poisoned");
        conn.execute(
            "DELETE FROM api_calls WHERE created_at < ?1",
            params![cutoff],
        )
        .map_err(|e| UsageStoreError::Query(e.to_string()))
    }

    /// Insert a single `press_timings` row (plan 041). Returns the
    /// generated row id. Called from inside the `persist_history`
    /// `spawn_blocking` closure (after paste), never on the hot path.
    pub fn insert_press_timing(&self, t: NewPressTiming) -> Result<i64, UsageStoreError> {
        let conn = self.conn.lock().expect("usage conn poisoned");
        conn.execute(
            "INSERT INTO press_timings (
                created_at, session_id, route,
                audio_ms, asr_ms, lid_wait_ms,
                cleanup_ms, inject_ms, total_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                t.created_at,
                t.session_id,
                t.route,
                t.audio_ms,
                t.asr_ms,
                t.lid_wait_ms,
                t.cleanup_ms,
                t.inject_ms,
                t.total_ms,
            ],
        )
        .map_err(|e| UsageStoreError::Query(e.to_string()))?;
        Ok(conn.last_insert_rowid())
    }

    /// Drop `press_timings` rows older than `days` (relative to "now").
    /// Mirrors [`purge_api_calls_older_than`](Self::purge_api_calls_older_than)
    /// and reuses [`API_CALLS_RETENTION_DAYS`]; run at launch and on
    /// `lib.rs`'s periodic retention-purge task. Returns the number of
    /// rows deleted.
    pub fn purge_press_timings_older_than(&self, days: u32) -> Result<usize, UsageStoreError> {
        let cutoff = unix_seconds_now() - (days as i64 * SECONDS_PER_DAY);
        let conn = self.conn.lock().expect("usage conn poisoned");
        conn.execute(
            "DELETE FROM press_timings WHERE created_at < ?1",
            params![cutoff],
        )
        .map_err(|e| UsageStoreError::Query(e.to_string()))
    }

    /// Insert-or-replace a `price_history` row keyed by
    /// `(effective_month, provider, model)`.
    pub fn upsert_price(&self, row: &PriceRow) -> Result<(), UsageStoreError> {
        let conn = self.conn.lock().expect("usage conn poisoned");
        conn.execute(
            "INSERT INTO price_history (
                effective_month, provider, model, kind,
                usd_per_second, usd_per_input_token, usd_per_output_token,
                source_url, fetched_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(effective_month, provider, model) DO UPDATE SET
                kind = excluded.kind,
                usd_per_second = excluded.usd_per_second,
                usd_per_input_token = excluded.usd_per_input_token,
                usd_per_output_token = excluded.usd_per_output_token,
                source_url = excluded.source_url,
                fetched_at = excluded.fetched_at",
            params![
                row.effective_month,
                row.provider,
                row.model,
                row.kind,
                row.usd_per_second,
                row.usd_per_input_token,
                row.usd_per_output_token,
                row.source_url,
                row.fetched_at,
            ],
        )
        .map_err(|e| UsageStoreError::Query(e.to_string()))?;
        Ok(())
    }

    /// Look up the price row for `(effective_month, provider, model)`,
    /// falling back to the most recent row *at or before* that month.
    ///
    /// The fallback closes a month-rollover gap: the refresher only
    /// inserts the new month's `price_history` row after its first
    /// successful fetch (launch burst + hourly), so calls made in the
    /// first hours of a month would otherwise find no exact-month row
    /// and freeze `cost_usd = NULL` forever (no retroactive repricing).
    /// `effective_month` is `YYYY-MM`, which sorts lexicographically, so
    /// `<= ?1 ORDER BY effective_month DESC LIMIT 1` yields last month's
    /// rate until a fresh one is posted — still a concrete historical
    /// rate frozen at insert time, never a future-month price.
    pub fn lookup_price(
        &self,
        effective_month: &str,
        provider: &str,
        model: &str,
    ) -> Result<Option<PriceRow>, UsageStoreError> {
        let conn = self.conn.lock().expect("usage conn poisoned");
        conn.query_row(
            "SELECT effective_month, provider, model, kind,
                    usd_per_second, usd_per_input_token, usd_per_output_token,
                    source_url, fetched_at
             FROM price_history
             WHERE effective_month <= ?1 AND provider = ?2 AND model = ?3
             ORDER BY effective_month DESC
             LIMIT 1",
            params![effective_month, provider, model],
            map_price_row,
        )
        .optional()
        .map_err(|e| UsageStoreError::Query(e.to_string()))
    }

    /// Aggregate cost + call count by provider for the given local
    /// `YYYY-MM`. Rows are bucketed by the machine's local timezone
    /// ([`LOCAL_MONTH_MODIFIER`]); `created_at` stays UTC unix-seconds,
    /// so no stored data changes.
    pub fn summary_for_month(&self, yyyymm: &str) -> Result<Vec<ProviderSummary>, UsageStoreError> {
        self.summary_for_month_bucketed(yyyymm, LOCAL_MONTH_MODIFIER)
    }

    /// Bucketing core of [`summary_for_month`]. `tz_modifier` is a SQLite
    /// datetime modifier bound as a parameter (not interpolated), so it
    /// is injection-safe; production passes `'localtime'`, tests pass a
    /// fixed offset to make the month boundary deterministic.
    fn summary_for_month_bucketed(
        &self,
        yyyymm: &str,
        tz_modifier: &str,
    ) -> Result<Vec<ProviderSummary>, UsageStoreError> {
        let conn = self.conn.lock().expect("usage conn poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT provider,
                        SUM(cost_usd) AS total_usd,
                        COUNT(*) AS call_count
                 FROM api_calls
                 WHERE strftime('%Y-%m', created_at, 'unixepoch', ?2) = ?1
                 GROUP BY provider
                 ORDER BY provider",
            )
            .map_err(|e| UsageStoreError::Query(e.to_string()))?;
        let rows = stmt
            .query_map(params![yyyymm, tz_modifier], |row| {
                Ok(ProviderSummary {
                    provider: row.get(0)?,
                    total_usd: row.get::<_, Option<f64>>(1)?,
                    call_count: row.get(2)?,
                })
            })
            .map_err(|e| UsageStoreError::Query(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| UsageStoreError::Query(e.to_string()))
    }

    /// Per-month, per-provider rollup across **all** recorded history,
    /// newest month first. Each entry carries the same [`ProviderSummary`]
    /// rows [`summary_for_month`](Self::summary_for_month) returns, so the
    /// Cost & Usage panel renders its whole month list from one query
    /// instead of a round-trip per month. Months with no `api_calls` rows
    /// never appear; the caller seeds the current month when it wants an
    /// always-present header.
    pub fn summary_by_month(&self) -> Result<Vec<MonthlySummary>, UsageStoreError> {
        self.summary_by_month_bucketed(LOCAL_MONTH_MODIFIER)
    }

    /// Bucketing core of [`summary_by_month`](Self::summary_by_month).
    /// `tz_modifier` is bound as a parameter (injection-safe) exactly as in
    /// [`summary_for_month_bucketed`](Self::summary_for_month_bucketed);
    /// tests pass a fixed offset to make month boundaries deterministic.
    fn summary_by_month_bucketed(
        &self,
        tz_modifier: &str,
    ) -> Result<Vec<MonthlySummary>, UsageStoreError> {
        let conn = self.conn.lock().expect("usage conn poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT strftime('%Y-%m', created_at, 'unixepoch', ?1) AS ym,
                        provider,
                        SUM(cost_usd) AS total_usd,
                        COUNT(*) AS call_count
                 FROM api_calls
                 GROUP BY ym, provider
                 ORDER BY ym DESC, provider",
            )
            .map_err(|e| UsageStoreError::Query(e.to_string()))?;
        let rows = stmt
            .query_map(params![tz_modifier], |row| {
                let year_month: String = row.get(0)?;
                Ok((
                    year_month,
                    ProviderSummary {
                        provider: row.get(1)?,
                        total_usd: row.get::<_, Option<f64>>(2)?,
                        call_count: row.get(3)?,
                    },
                ))
            })
            .map_err(|e| UsageStoreError::Query(e.to_string()))?;

        // Collapse the (month, provider) stream into one entry per month.
        // `ORDER BY ym DESC` groups each month's rows contiguously, so a
        // single pass preserves both the newest-first month order and the
        // alphabetical provider order within each month.
        let mut months: Vec<MonthlySummary> = Vec::new();
        for row in rows {
            let (year_month, provider) = row.map_err(|e| UsageStoreError::Query(e.to_string()))?;
            match months.last_mut() {
                Some(month) if month.year_month == year_month => {
                    month.provider_totals.push(provider);
                }
                _ => months.push(MonthlySummary {
                    year_month,
                    provider_totals: vec![provider],
                }),
            }
        }
        Ok(months)
    }

    /// Current calendar month (`YYYY-MM`) in the machine's local
    /// timezone, computed by SQLite so it uses the exact same
    /// `localtime` rules as [`summary_for_month`]'s bucketing — the
    /// current-month string and the row buckets can never drift apart.
    pub fn current_local_yyyymm(&self) -> Result<String, UsageStoreError> {
        let conn = self.conn.lock().expect("usage conn poisoned");
        conn.query_row("SELECT strftime('%Y-%m', 'now', 'localtime')", [], |row| {
            row.get(0)
        })
        .map_err(|e| UsageStoreError::Query(e.to_string()))
    }

    /// Per-model granularity for the dev pane. Bucketed by local month
    /// ([`LOCAL_MONTH_MODIFIER`]) to match [`summary_for_month`].
    pub fn summary_for_month_per_model(
        &self,
        yyyymm: &str,
    ) -> Result<Vec<ModelSummary>, UsageStoreError> {
        let conn = self.conn.lock().expect("usage conn poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT provider, model,
                        SUM(cost_usd) AS total_usd,
                        COUNT(*) AS call_count,
                        SUM(audio_seconds) AS audio_seconds,
                        SUM(input_tokens) AS input_tokens,
                        SUM(output_tokens) AS output_tokens
                 FROM api_calls
                 WHERE strftime('%Y-%m', created_at, 'unixepoch', ?2) = ?1
                 GROUP BY provider, model
                 ORDER BY provider, model",
            )
            .map_err(|e| UsageStoreError::Query(e.to_string()))?;
        let rows = stmt
            .query_map(params![yyyymm, LOCAL_MONTH_MODIFIER], |row| {
                Ok(ModelSummary {
                    provider: row.get(0)?,
                    model: row.get(1)?,
                    total_usd: row.get::<_, Option<f64>>(2)?,
                    call_count: row.get(3)?,
                    audio_seconds: row.get::<_, Option<f64>>(4)?,
                    input_tokens: row.get::<_, Option<i64>>(5)?,
                    output_tokens: row.get::<_, Option<i64>>(6)?,
                })
            })
            .map_err(|e| UsageStoreError::Query(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| UsageStoreError::Query(e.to_string()))
    }

    /// All `price_history` rows for `yyyymm`.
    pub fn list_prices_for_month(&self, yyyymm: &str) -> Result<Vec<PriceRow>, UsageStoreError> {
        let conn = self.conn.lock().expect("usage conn poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT effective_month, provider, model, kind,
                        usd_per_second, usd_per_input_token, usd_per_output_token,
                        source_url, fetched_at
                 FROM price_history
                 WHERE effective_month = ?1
                 ORDER BY provider, model",
            )
            .map_err(|e| UsageStoreError::Query(e.to_string()))?;
        let rows = stmt
            .query_map(params![yyyymm], map_price_row)
            .map_err(|e| UsageStoreError::Query(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| UsageStoreError::Query(e.to_string()))
    }

    /// Total `price_history` rows. The seed bootstrap in
    /// `lib.rs::setup` checks this to decide whether to insert
    /// `DEFAULT_PRICES`.
    pub fn count_price_history(&self) -> Result<i64, UsageStoreError> {
        let conn = self.conn.lock().expect("usage conn poisoned");
        conn.query_row("SELECT COUNT(*) FROM price_history", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|e| UsageStoreError::Query(e.to_string()))
    }

    /// Read a unix-seconds metadata value. `None` when the key isn't set.
    fn get_metadata_i64(&self, key: &str) -> Result<Option<i64>, UsageStoreError> {
        let conn = self.conn.lock().expect("usage conn poisoned");
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM app_metadata WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| UsageStoreError::Query(e.to_string()))?;
        Ok(value.and_then(|s| s.parse::<i64>().ok()))
    }

    fn set_metadata_i64(&self, key: &str, value: i64) -> Result<(), UsageStoreError> {
        let conn = self.conn.lock().expect("usage conn poisoned");
        conn.execute(
            "INSERT INTO app_metadata (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value.to_string()],
        )
        .map_err(|e| UsageStoreError::Query(e.to_string()))?;
        Ok(())
    }

    pub fn last_priced_success_at(&self) -> Result<Option<i64>, UsageStoreError> {
        self.get_metadata_i64(LAST_PRICED_SUCCESS_KEY)
    }

    pub fn set_last_priced_success_at(&self, unix_secs: i64) -> Result<(), UsageStoreError> {
        self.set_metadata_i64(LAST_PRICED_SUCCESS_KEY, unix_secs)
    }

    pub fn last_priced_attempt_at(&self) -> Result<Option<i64>, UsageStoreError> {
        self.get_metadata_i64(LAST_PRICED_ATTEMPT_KEY)
    }

    pub fn set_last_priced_attempt_at(&self, unix_secs: i64) -> Result<(), UsageStoreError> {
        self.set_metadata_i64(LAST_PRICED_ATTEMPT_KEY, unix_secs)
    }
}

fn read_schema_version(conn: &Connection) -> rusqlite::Result<i32> {
    conn.query_row(
        "SELECT value FROM app_metadata WHERE key = ?1",
        params![SCHEMA_VERSION_KEY],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map(|opt| opt.and_then(|s| s.parse::<i32>().ok()).unwrap_or(0))
}

fn write_schema_version(conn: &Connection, version: i32) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO app_metadata (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![SCHEMA_VERSION_KEY, version.to_string()],
    )?;
    Ok(())
}

fn map_price_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PriceRow> {
    Ok(PriceRow {
        effective_month: row.get(0)?,
        provider: row.get(1)?,
        model: row.get(2)?,
        kind: row.get(3)?,
        usd_per_second: row.get::<_, Option<f64>>(4)?,
        usd_per_input_token: row.get::<_, Option<f64>>(5)?,
        usd_per_output_token: row.get::<_, Option<f64>>(6)?,
        source_url: row.get::<_, Option<String>>(7)?,
        fetched_at: row.get(8)?,
    })
}

/// Derive the `kind` wire string for a `PriceRow` from a typed
/// `PriceKind`. Used by callers building rows from `DEFAULT_PRICES`
/// or scraper API responses.
pub fn kind_to_wire(kind: PriceKind) -> String {
    kind.as_wire().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::DEFAULT_PRICES;
    use tempfile::tempdir;

    fn fresh_store() -> (UsageStore, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        // Open the history store first so the `dictation_records` FK
        // target exists before `api_calls` references it.
        let path = crate::history_store::HistoryStore::default_path(dir.path());
        let _hist = crate::history_store::HistoryStore::open(&path).expect("open history");
        let store = UsageStore::open(&path).expect("open usage");
        (store, dir)
    }

    fn sample_call(created_at: i64) -> NewApiCall {
        NewApiCall {
            created_at,
            provider: "deepgram".into(),
            model: "nova-3".into(),
            call_kind: "asr".into(),
            audio_seconds: Some(60.0),
            input_tokens: None,
            output_tokens: None,
            cost_usd: Some(60.0 * 0.000_08),
            latency_ms: Some(450),
            status: "ok".into(),
            request_id: Some("req-1".into()),
            session_id: None,
        }
    }

    /// Count rows in `sqlite_master` for a table name — a table exists
    /// iff this returns 1.
    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    }

    #[test]
    fn fresh_db_has_tables_and_schema_version_2() {
        let (store, _dir) = fresh_store();
        assert_eq!(store.count_price_history().unwrap(), 0);
        let conn = store.conn.lock().unwrap();
        let v = read_schema_version(&conn).unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION);
        assert_eq!(CURRENT_SCHEMA_VERSION, 2);
        // v2 introduced `press_timings` alongside the v1 tables.
        assert!(table_exists(&conn, "api_calls"));
        assert!(table_exists(&conn, "price_history"));
        assert!(table_exists(&conn, "press_timings"));
    }

    /// An existing v1 install (api_calls/price_history present, version
    /// stamped 1, no `press_timings`) must migrate to v2 on reopen:
    /// `press_timings` appears and the version bumps, without disturbing
    /// existing rows. Simulated by winding the stamped version back to 1
    /// and dropping the v2 table, then reopening.
    #[test]
    fn v1_db_upgrades_to_v2_on_reopen() {
        let dir = tempdir().expect("tempdir");
        let path = crate::history_store::HistoryStore::default_path(dir.path());
        let _hist = crate::history_store::HistoryStore::open(&path).expect("open history");

        // Open once at current schema, seed a row, then simulate a v1 DB
        // by dropping `press_timings` and rewinding the version stamp.
        {
            let store = UsageStore::open(&path).unwrap();
            store.insert_call(sample_call(1_700_000_000)).unwrap();
            let conn = store.conn.lock().unwrap();
            conn.execute_batch("DROP TABLE press_timings;").unwrap();
            write_schema_version(&conn, 1).unwrap();
            assert!(!table_exists(&conn, "press_timings"));
        }

        // Reopen — run_migrations sees version 1 and applies the v2 step.
        let store2 = UsageStore::open(&path).unwrap();
        let conn = store2.conn.lock().unwrap();
        assert_eq!(read_schema_version(&conn).unwrap(), 2);
        assert!(table_exists(&conn, "press_timings"));
        // The pre-existing api_calls row survived the upgrade untouched.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM api_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    /// The v2 migration step must be idempotent even when a *partial* v1→v2
    /// upgrade crashed after `execute_batch` created `press_timings` but
    /// before `write_schema_version` stamped v2 — the `IF NOT EXISTS`
    /// promise in `run_migrations` (see the comment there). Unlike
    /// `v1_db_upgrades_to_v2_on_reopen` (which drops the table first), this
    /// leaves the v2 table in place and only rewinds the stamp, so reopening
    /// re-runs `CREATE TABLE/INDEX IF NOT EXISTS` against an already-present
    /// table — which must succeed rather than error on "table already
    /// exists".
    #[test]
    fn v2_migration_is_idempotent_over_an_existing_press_timings_table() {
        let dir = tempdir().expect("tempdir");
        let path = crate::history_store::HistoryStore::default_path(dir.path());
        let _hist = crate::history_store::HistoryStore::open(&path).expect("open history");

        {
            let store = UsageStore::open(&path).unwrap();
            let conn = store.conn.lock().unwrap();
            // Rewind the stamp WITHOUT dropping press_timings — mimics a
            // crash between the v2 batch and the version stamp.
            write_schema_version(&conn, 1).unwrap();
            assert!(table_exists(&conn, "press_timings"));
        }

        // Reopen: the v2 step re-runs over the existing table and index.
        let store2 =
            UsageStore::open(&path).expect("v2 migration must be idempotent over existing table");
        let conn = store2.conn.lock().unwrap();
        assert_eq!(read_schema_version(&conn).unwrap(), 2);
        assert!(table_exists(&conn, "press_timings"));
    }

    fn sample_timing(created_at: i64) -> NewPressTiming {
        NewPressTiming {
            created_at,
            session_id: None,
            route: "deepgram".into(),
            audio_ms: Some(4200),
            asr_ms: Some(380),
            lid_wait_ms: None,
            cleanup_ms: Some(420),
            inject_ms: Some(35),
            total_ms: 850,
        }
    }

    #[test]
    fn insert_press_timing_round_trips_with_nulls_preserved() {
        let (store, _dir) = fresh_store();
        let id = store
            .insert_press_timing(sample_timing(1_779_192_000))
            .unwrap();
        assert!(id > 0);
        let conn = store.conn.lock().unwrap();
        // `lid_wait_ms` was None — it must land as SQL NULL, never 0.
        let (route, lid, total): (String, Option<i64>, i64) = conn
            .query_row(
                "SELECT route, lid_wait_ms, total_ms FROM press_timings WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(route, "deepgram");
        assert_eq!(lid, None);
        assert_eq!(total, 850);
    }

    #[test]
    fn purge_press_timings_keeps_recent_drops_old() {
        let (store, _dir) = fresh_store();
        // Recent row: insert via the public API so created_at = now.
        let recent_id = store
            .insert_press_timing(sample_timing(unix_seconds_now()))
            .unwrap();
        // Stale row: backdate 60 days via the connection directly.
        let cutoff = unix_seconds_now() - 60 * SECONDS_PER_DAY;
        store.insert_press_timing(sample_timing(cutoff)).unwrap();

        let removed = store.purge_press_timings_older_than(30).unwrap();
        assert_eq!(removed, 1);
        let conn = store.conn.lock().unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM press_timings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
        let surviving_id: i64 = conn
            .query_row("SELECT id FROM press_timings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(surviving_id, recent_id);
    }

    #[test]
    fn reopen_is_idempotent() {
        let dir = tempdir().expect("tempdir");
        let path = crate::history_store::HistoryStore::default_path(dir.path());
        let _hist = crate::history_store::HistoryStore::open(&path).expect("open history");
        let store = UsageStore::open(&path).unwrap();
        let id = store.insert_call(sample_call(1_700_000_000)).unwrap();
        assert!(id > 0);
        drop(store);
        // Reopen — must not blow away rows or re-run a destructive migration.
        let store2 = UsageStore::open(&path).unwrap();
        let conn = store2.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM api_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn insert_round_trips_with_summary_aggregation() {
        let (store, _dir) = fresh_store();
        // Two deepgram calls and one groq call, all in May 2026 UTC.
        // Unix 2026-05-15T12:00:00Z = 1779192000.
        let may_15: i64 = 1_779_192_000;
        let mut a = sample_call(may_15);
        a.cost_usd = Some(0.10);
        store.insert_call(a).unwrap();
        let mut b = sample_call(may_15 + 60);
        b.cost_usd = Some(0.20);
        store.insert_call(b).unwrap();
        let mut c = NewApiCall {
            created_at: may_15 + 120,
            provider: "groq".into(),
            model: "openai/gpt-oss-120b".into(),
            call_kind: "cleanup".into(),
            audio_seconds: None,
            input_tokens: Some(100),
            output_tokens: Some(50),
            cost_usd: Some(0.05),
            latency_ms: Some(700),
            status: "ok".into(),
            request_id: None,
            session_id: None,
        };
        c.status = "ok".into();
        store.insert_call(c).unwrap();

        let summary = store.summary_for_month("2026-05").unwrap();
        assert_eq!(summary.len(), 2);
        let dg = summary.iter().find(|s| s.provider == "deepgram").unwrap();
        assert_eq!(dg.call_count, 2);
        assert!((dg.total_usd.unwrap() - 0.30).abs() < 1e-9);
        let groq = summary.iter().find(|s| s.provider == "groq").unwrap();
        assert_eq!(groq.call_count, 1);
        assert!((groq.total_usd.unwrap() - 0.05).abs() < 1e-9);

        // Different month yields nothing.
        assert!(store.summary_for_month("2026-06").unwrap().is_empty());
    }

    #[test]
    fn summary_by_month_groups_newest_first_with_provider_rows() {
        let (store, _dir) = fresh_store();
        // May 2026: one deepgram + one groq call. Unix 2026-05-15T12:00Z.
        let may_15: i64 = 1_779_192_000;
        let mut may_dg = sample_call(may_15);
        may_dg.cost_usd = Some(0.10);
        store.insert_call(may_dg).unwrap();
        let mut may_groq = sample_call(may_15 + 60);
        may_groq.provider = "groq".into();
        may_groq.cost_usd = Some(0.05);
        store.insert_call(may_groq).unwrap();
        // April 2026: one deepgram call. Unix 2026-04-15T12:00Z.
        let apr_15: i64 = 1_776_513_600;
        let mut apr_dg = sample_call(apr_15);
        apr_dg.cost_usd = Some(0.20);
        store.insert_call(apr_dg).unwrap();

        // Force UTC bucketing so the assertion is host-timezone independent.
        let months = store.summary_by_month_bucketed("+0 hours").unwrap();
        assert_eq!(months.len(), 2, "two distinct months recorded");
        // Newest month first.
        assert_eq!(months[0].year_month, "2026-05");
        assert_eq!(months[1].year_month, "2026-04");
        // May has both providers, alphabetically ordered.
        assert_eq!(
            months[0]
                .provider_totals
                .iter()
                .map(|p| p.provider.as_str())
                .collect::<Vec<_>>(),
            vec!["deepgram", "groq"],
        );
        let may_dg_row = &months[0].provider_totals[0];
        assert_eq!(may_dg_row.call_count, 1);
        assert!((may_dg_row.total_usd.unwrap() - 0.10).abs() < 1e-9);
        // April has just the one provider.
        assert_eq!(months[1].provider_totals.len(), 1);
        assert_eq!(months[1].provider_totals[0].provider, "deepgram");
        assert!((months[1].provider_totals[0].total_usd.unwrap() - 0.20).abs() < 1e-9);
    }

    #[test]
    fn summary_by_month_is_empty_for_fresh_store() {
        let (store, _dir) = fresh_store();
        assert!(store.summary_by_month().unwrap().is_empty());
    }

    #[test]
    fn summary_per_model_includes_raw_units() {
        let (store, _dir) = fresh_store();
        let may_15: i64 = 1_779_192_000;
        store.insert_call(sample_call(may_15)).unwrap();

        let per_model = store.summary_for_month_per_model("2026-05").unwrap();
        assert_eq!(per_model.len(), 1);
        let row = &per_model[0];
        assert_eq!(row.provider, "deepgram");
        assert_eq!(row.model, "nova-3");
        assert_eq!(row.call_count, 1);
        assert!((row.audio_seconds.unwrap() - 60.0).abs() < 1e-9);
    }

    /// The bucket honors its timezone modifier rather than always using
    /// UTC. A fixed `+14 hours` offset (host-independent, unlike the
    /// production `localtime` modifier) pushes a call from the last hours
    /// of one UTC month into the next calendar month. Proves the rollup
    /// follows the user's local clock, not UTC.
    #[test]
    fn bucket_shifts_month_with_timezone_modifier() {
        let (store, _dir) = fresh_store();
        // 2026-05-31T12:00:00Z. +14h => 2026-06-01T02:00, still May in UTC.
        let ts = time::macros::datetime!(2026-05-31 12:00:00 UTC).unix_timestamp();
        store.insert_call(sample_call(ts)).unwrap();

        // UTC bucketing keeps it in May; +14h pushes it into June.
        assert_eq!(
            store
                .summary_for_month_bucketed("2026-05", "+0 hours")
                .unwrap()
                .len(),
            1
        );
        assert!(store
            .summary_for_month_bucketed("2026-06", "+0 hours")
            .unwrap()
            .is_empty());
        assert!(store
            .summary_for_month_bucketed("2026-05", "+14 hours")
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .summary_for_month_bucketed("2026-06", "+14 hours")
                .unwrap()
                .len(),
            1
        );
    }

    /// `current_local_yyyymm` and `summary_for_month`'s production
    /// bucketing both use SQLite `localtime`, so a call stamped "now"
    /// always lands in the reported current month — on any host
    /// timezone, with no boundary gap between the two computations.
    #[test]
    fn current_month_rollup_includes_a_just_now_call() {
        let (store, _dir) = fresh_store();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        store.insert_call(sample_call(now)).unwrap();

        let month = store.current_local_yyyymm().unwrap();
        let summary = store.summary_for_month(&month).unwrap();
        assert_eq!(
            summary.len(),
            1,
            "now-stamped call must appear in the current local month"
        );
        assert_eq!(summary[0].provider, "deepgram");
    }

    #[test]
    fn upsert_price_replaces_previous_row() {
        let (store, _dir) = fresh_store();
        let row = PriceRow {
            effective_month: "2026-05".into(),
            provider: "deepgram".into(),
            model: "nova-3".into(),
            kind: "per_audio_second".into(),
            usd_per_second: Some(0.000_08),
            usd_per_input_token: None,
            usd_per_output_token: None,
            source_url: Some("https://deepgram.com/pricing".into()),
            fetched_at: 1_779_192_000,
        };
        store.upsert_price(&row).unwrap();

        // Same key, different rate — must overwrite, not duplicate.
        let mut row2 = row.clone();
        row2.usd_per_second = Some(0.000_09);
        row2.fetched_at = 1_779_278_400;
        store.upsert_price(&row2).unwrap();

        let listed = store.list_prices_for_month("2026-05").unwrap();
        assert_eq!(listed.len(), 1);
        assert!((listed[0].usd_per_second.unwrap() - 0.000_09).abs() < 1e-12);
        assert_eq!(listed[0].fetched_at, 1_779_278_400);
    }

    #[test]
    fn lookup_price_returns_none_for_missing_row() {
        let (store, _dir) = fresh_store();
        let opt = store.lookup_price("2026-05", "deepgram", "nova-3").unwrap();
        assert!(opt.is_none());
    }

    /// Regression: at a month rollover the refresher hasn't yet inserted
    /// the new month's row, so a call stamped in the new month must
    /// inherit the most recent prior-month rate instead of freezing
    /// `cost_usd = NULL`. Verifies both the fallback (June → May row) and
    /// that lookups never reach forward to a future-month row.
    #[test]
    fn lookup_price_falls_back_to_most_recent_prior_month() {
        let (store, _dir) = fresh_store();
        let may = PriceRow {
            effective_month: "2026-05".into(),
            provider: "deepgram".into(),
            model: "nova-3".into(),
            kind: "per_audio_second".into(),
            usd_per_second: Some(0.000_08),
            usd_per_input_token: None,
            usd_per_output_token: None,
            source_url: Some("https://deepgram.com/pricing".into()),
            fetched_at: 1_779_192_000,
        };
        store.upsert_price(&may).unwrap();

        // June has no row yet — must fall back to the May rate, not NULL.
        let june = store.lookup_price("2026-06", "deepgram", "nova-3").unwrap();
        let june = june.expect("June lookup must inherit the May rate");
        assert_eq!(june.effective_month, "2026-05");
        assert!((june.usd_per_second.unwrap() - 0.000_08).abs() < 1e-12);

        // A month before the earliest row must NOT reach forward to May.
        let april = store.lookup_price("2026-04", "deepgram", "nova-3").unwrap();
        assert!(
            april.is_none(),
            "lookup must never resolve a future-month price"
        );

        // Once June's own row lands, it wins over the May fallback.
        let mut jun = may.clone();
        jun.effective_month = "2026-06".into();
        jun.usd_per_second = Some(0.000_110);
        store.upsert_price(&jun).unwrap();
        let resolved = store
            .lookup_price("2026-06", "deepgram", "nova-3")
            .unwrap()
            .expect("June row now exists");
        assert_eq!(resolved.effective_month, "2026-06");
        assert!((resolved.usd_per_second.unwrap() - 0.000_110).abs() < 1e-12);
    }

    #[test]
    fn metadata_i64_round_trip() {
        let (store, _dir) = fresh_store();
        assert!(store.last_priced_success_at().unwrap().is_none());
        store.set_last_priced_success_at(1_779_192_000).unwrap();
        assert_eq!(store.last_priced_success_at().unwrap(), Some(1_779_192_000));
        // Update path.
        store.set_last_priced_success_at(1_779_278_400).unwrap();
        assert_eq!(store.last_priced_success_at().unwrap(), Some(1_779_278_400));
    }

    #[test]
    fn default_prices_seed_via_upsert_loop() {
        let (store, _dir) = fresh_store();
        for entry in DEFAULT_PRICES {
            let row = PriceRow {
                effective_month: "2026-05".into(),
                provider: entry.provider.into(),
                model: entry.model.into(),
                kind: entry.kind.as_wire().into(),
                usd_per_second: entry.usd_per_second,
                usd_per_input_token: entry.usd_per_input_token,
                usd_per_output_token: entry.usd_per_output_token,
                source_url: Some(entry.source_url.into()),
                fetched_at: 0,
            };
            store.upsert_price(&row).unwrap();
        }
        let listed = store.list_prices_for_month("2026-05").unwrap();
        assert_eq!(listed.len(), DEFAULT_PRICES.len());
    }
}
