//! Integration tests for the cost-tracking schema migrations
//! (feature 005).
//!
//! These exercise the public surface (`UsageStore::open` against a
//! shared SQLite file) rather than poking the connection directly,
//! so the test would catch any regression that breaks the
//! reopen-is-idempotent invariant the unit tests already cover.

use muni_lib::history_store::{HistoryStore, NewDictationRecord, SERVED_BY_GLADIA_PRIMARY};
use muni_lib::pricing::DEFAULT_PRICES;
use muni_lib::usage_store::{NewApiCall, PriceRow, UsageStore, API_CALLS_RETENTION_DAYS};
use rusqlite::Connection;
use tempfile::tempdir;

fn fresh_paths() -> (std::path::PathBuf, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");
    let path = HistoryStore::default_path(dir.path());
    // History store first — owns `PRAGMA user_version`.
    let _ = HistoryStore::open(&path).expect("history open");
    (path, dir)
}

#[test]
fn fresh_db_has_zero_rows_and_runs_migration_idempotently() {
    let (path, _dir) = fresh_paths();
    let store = UsageStore::open(&path).expect("usage open");
    assert_eq!(store.count_price_history().unwrap(), 0);
    assert!(store.list_prices_for_month("2026-05").unwrap().is_empty());
    drop(store);

    // Reopen — must be a no-op (no INSERT failures, schema version
    // unchanged).
    let store2 = UsageStore::open(&path).expect("reopen");
    assert_eq!(store2.count_price_history().unwrap(), 0);
}

#[test]
fn reopen_after_inserts_preserves_data() {
    let (path, _dir) = fresh_paths();
    let store = UsageStore::open(&path).unwrap();

    // Seed a few price rows + one api_call row, then reopen.
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
            fetched_at: 1_779_000_000,
        };
        store.upsert_price(&row).unwrap();
    }
    let id = store
        .insert_call(NewApiCall {
            created_at: 1_779_192_000,
            provider: "deepgram".into(),
            model: "nova-3".into(),
            call_kind: "asr".into(),
            audio_seconds: Some(60.0),
            input_tokens: None,
            output_tokens: None,
            cost_usd: Some(60.0 * 0.000_08),
            latency_ms: Some(450),
            status: "ok".into(),
            request_id: None,
            session_id: None,
        })
        .unwrap();
    assert!(id > 0);
    drop(store);

    let store2 = UsageStore::open(&path).unwrap();
    assert_eq!(
        store2.count_price_history().unwrap() as usize,
        DEFAULT_PRICES.len()
    );
    let summary = store2.summary_for_month("2026-05").unwrap();
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0].provider, "deepgram");
    assert_eq!(summary[0].call_count, 1);
}

fn sample_call(created_at: i64, session_id: Option<i64>) -> NewApiCall {
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
        request_id: None,
        session_id,
    }
}

/// Plan 039 task 47 — every writer connection onto `history.sqlite3` (here:
/// `HistoryStore`'s and `UsageStore`'s) must switch the file to WAL journal
/// mode. WAL is stamped into the file header itself, so a brand-new, plain
/// `rusqlite::Connection` opened straight against the path (no store
/// wrapper) must observe `wal` without ever having set it itself.
#[test]
fn wal_journal_mode_persists_in_file_after_store_open() {
    let (path, _dir) = fresh_paths();
    let _usage = UsageStore::open(&path).expect("usage open");

    let raw = Connection::open(&path).expect("raw reopen");
    let mode: String = raw
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("read journal_mode");
    assert_eq!(mode.to_lowercase(), "wal");
}

/// Plan 039 task 47's FK resolution: enforce `api_calls.session_id`'s
/// `ON DELETE SET NULL` rather than drop the (correct) constraint. Purging a
/// `dictation_records` row through `HistoryStore` must null out any
/// `api_calls` row that referenced it — no orphaned session ids left behind.
#[test]
fn purging_history_row_nulls_referencing_api_call_session_id() {
    let dir = tempdir().expect("tempdir");
    let path = HistoryStore::default_path(dir.path());
    let history = HistoryStore::open(&path).expect("history open");
    let usage = UsageStore::open(&path).expect("usage open");

    let record_id = history
        .insert(NewDictationRecord {
            raw_text: "raw hello".into(),
            cleaned_text: "hello".into(),
            target_app_bundle_id: None,
            served_by: SERVED_BY_GLADIA_PRIMARY.into(),
        })
        .expect("insert dictation record");

    let call_id = usage
        .insert_call(sample_call(1_700_000_000, Some(record_id)))
        .expect("insert api_call");

    // Backdate the dictation record so `purge_older_than` actually removes
    // it, then purge everything older than 0 days (i.e. everything).
    let deleted = history.delete(record_id).expect("delete dictation record");
    assert_eq!(deleted, 1);

    let session_id: Option<i64> = {
        let raw = Connection::open(&path).expect("raw reopen");
        raw.query_row(
            "SELECT session_id FROM api_calls WHERE id = ?1",
            [call_id],
            |row| row.get(0),
        )
        .expect("read api_calls row")
    };
    assert_eq!(
        session_id, None,
        "foreign_keys=ON must SET NULL session_id when the referenced dictation_records row is deleted"
    );
}

/// `api_calls` retention (plan 039 task 47) — no user-facing slider, fixed
/// [`API_CALLS_RETENTION_DAYS`] window. Mirrors
/// `history_store::purge_older_than_keeps_recent_drops_old`.
#[test]
fn purge_api_calls_older_than_keeps_recent_drops_old() {
    let (path, _dir) = fresh_paths();
    let store = UsageStore::open(&path).unwrap();

    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let recent_id = store.insert_call(sample_call(now, None)).unwrap();
    let stale_cutoff = now - (API_CALLS_RETENTION_DAYS as i64 + 1) * 86_400;
    let stale_id = store.insert_call(sample_call(stale_cutoff, None)).unwrap();

    let removed = store
        .purge_api_calls_older_than(API_CALLS_RETENTION_DAYS)
        .unwrap();
    assert_eq!(removed, 1);

    let raw = Connection::open(&path).expect("raw reopen");
    let remaining_ids: Vec<i64> = {
        let mut stmt = raw.prepare("SELECT id FROM api_calls").unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(remaining_ids, vec![recent_id]);
    assert!(!remaining_ids.contains(&stale_id));
}
