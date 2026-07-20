//! Durable PostHog analytics queue (Feature 033, Phase 2).
//!
//! Operational-health events ([`super::events`]) are routed through the Rust
//! backend rather than `posthog-js`, which loses queued events on app quit and
//! needs CSP juggling. This module owns the durable spine:
//!
//! 1. A `telemetry_queue` table in the existing `history.sqlite3` database — one
//!    row per pending event, append-only, bounded (oldest dropped past
//!    [`MAX_QUEUE_ROWS`]). Durable across crashes: events queued before a hard
//!    exit are flushed on the next launch.
//! 2. A background flush task (`tauri::async_runtime::spawn`) that POSTs batches
//!    to `{host}/batch/` on a timer, deleting rows only on a 2xx. A short
//!    reqwest timeout and fully-swallowed errors keep telemetry failure
//!    invisible to the user.
//! 3. A best-effort [`TelemetryQueue::flush_now`] for app exit
//!    (`RunEvent::ExitRequested`).
//!
//! ## Privacy
//!
//! The queue stores whatever JSON [`super::events`] produced — and those events
//! are allowlist-built, so no transcript content can be here. The queue is a
//! dumb transport; the privacy guarantee lives one layer up in `events.rs`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Map, Value};
use tokio::time::interval;

use super::events::{PostHogEvent, PROP_PROCESS_PERSON_PROFILE};

/// Default PostHog ingestion host. Overridable via `MUNI_POSTHOG_HOST` so dev
/// can point at a mock; prod bakes `https://us.i.posthog.com`.
pub const DEFAULT_POSTHOG_HOST: &str = "https://us.i.posthog.com";

/// Env var for the ingestion host override (mirrors the key/DSN overrides).
const POSTHOG_HOST_ENV: &str = "MUNI_POSTHOG_HOST";

/// Max rows retained in the queue. A long offline stretch (or a dead PostHog)
/// must not grow the DB without bound — past this we drop the OLDEST rows, since
/// recent operational state is more useful than stale history for monitoring.
const MAX_QUEUE_ROWS: usize = 5_000;

/// How often the background task attempts a flush. Operational health doesn't
/// need real-time delivery; batching keeps request count (and cost) down.
const FLUSH_INTERVAL: Duration = Duration::from_secs(30);

/// Max events sent in one `/batch/` request. PostHog accepts large batches; we
/// cap so a backlog drains in bounded chunks rather than one huge body.
const FLUSH_BATCH_SIZE: usize = 100;

/// Per-request timeout for the flush POST. Short on purpose — a stalled PostHog
/// must never tie up a background worker; the rows stay queued for the next tick.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-request timeout for the on-exit best-effort flush. Tighter than the
/// periodic one so app quit isn't held up if PostHog is slow; unsent rows
/// survive in the DB.
const EXIT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Hard overall deadline for the on-exit flush (plan 039 task 41). A
/// slow-but-responsive endpoint with a full queue would otherwise let
/// `flush_drain` loop for `passes × EXIT_FLUSH_TIMEOUT` (tens of seconds),
/// stalling quit — the per-request timeout alone doesn't bound the *total* work
/// because each successful full batch keeps the drain looping. This caps the
/// whole exit flush; anything unsent is durable and drains on the next launch.
const EXIT_FLUSH_DEADLINE: Duration = Duration::from_secs(4);

/// Resolved ingestion config. `None` (no key) disables analytics entirely.
#[derive(Debug, Clone)]
pub struct PostHogConfig {
    /// Project key (`phc_…`). Client-safe, baked into prod / overridden in dev.
    pub api_key: String,
    /// Ingestion host (no trailing slash assumptions — we append `/batch/`).
    pub host: String,
}

impl PostHogConfig {
    /// Resolve from the baked/overridden key plus the host env (defaulting to
    /// [`DEFAULT_POSTHOG_HOST`]). Returns `None` when no key resolves — the
    /// dev/staging default, which leaves analytics fully off.
    pub fn resolve() -> Option<Self> {
        let api_key = super::resolve_posthog_key()?;
        let host = std::env::var(POSTHOG_HOST_ENV)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_POSTHOG_HOST.to_string());
        Some(Self { api_key, host })
    }
}

/// Durable, bounded, append-only event queue backed by SQLite.
///
/// Shares the `history.sqlite3` file but holds its OWN connection (rusqlite
/// `Connection` is `Send` but not `Sync`, so it lives behind a `Mutex`). Insert
/// + flush both complete in well under a millisecond against a tiny table.
pub struct TelemetryQueue {
    conn: std::sync::Mutex<Connection>,
    /// The install UUID — the per-event `distinct_id`, placed INSIDE each
    /// event's `properties` for the `/batch/` shape.
    distinct_id: String,
    config: PostHogConfig,
    http: reqwest::Client,
}

impl TelemetryQueue {
    /// Open the queue against the SQLite file at `db_path` (the same
    /// `history.sqlite3` the history/usage stores use), creating the
    /// `telemetry_queue` table if absent. The parent dir must already exist
    /// (the history store creates it at boot).
    pub fn open(
        db_path: &Path,
        distinct_id: String,
        config: PostHogConfig,
    ) -> Result<Self, rusqlite::Error> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(db_path, flags)?;
        // Plan 039 task 47 — this is the third of three writer connections
        // sharing `history.sqlite3` (alongside `HistoryStore`/`UsageStore`);
        // see `configure_shared_writer_connection`'s doc for the WAL/
        // busy_timeout/foreign_keys rationale.
        crate::history_store::configure_shared_writer_connection(&conn)?;
        Self::ensure_table(&conn)?;
        // A short timeout mirrors groq.rs's swallow-everything posture: a
        // telemetry HTTP failure is invisible to the user, so we never want it
        // to hold a connection or worker for long.
        let http = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(300))
            .timeout(REQUEST_TIMEOUT)
            .build()
            // A client-build failure is non-fatal for telemetry; fall back to a
            // default client so enqueue still works (flush just no-ops on the
            // unreachable network, same as offline).
            .unwrap_or_default();
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
            distinct_id,
            config,
            http,
        })
    }

    fn ensure_table(conn: &Connection) -> Result<(), rusqlite::Error> {
        // Own table inside the shared DB; `IF NOT EXISTS` so it's idempotent and
        // doesn't interact with the history/usage `user_version` migrations.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS telemetry_queue (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at INTEGER NOT NULL,
                payload TEXT NOT NULL
            );",
        )
    }

    /// Append one event to the queue (synchronous, sub-millisecond). Builds the
    /// `/batch/`-shaped JSON object — `event`, the allowlisted `properties`, and
    /// the per-event `distinct_id` placed inside `properties` per the batch API
    /// — then enforces the bound by dropping the oldest rows. Best-effort: any
    /// SQLite error is logged and swallowed (telemetry never fails a press).
    pub fn enqueue(&self, event: &PostHogEvent) {
        let payload = self.serialize(event);
        let Ok(payload_str) = serde_json::to_string(&payload) else {
            log::warn!(target: "telemetry", "serialize telemetry event failed");
            return;
        };
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Err(err) = conn.execute(
            "INSERT INTO telemetry_queue (created_at, payload) VALUES (?1, ?2)",
            params![now_unix(), payload_str],
        ) {
            log::warn!(target: "telemetry", "enqueue telemetry event failed: {err}");
            return;
        }
        Self::enforce_bound(&conn);
    }

    /// Build the `/batch/`-event object for one event: `{event, properties}`
    /// with `distinct_id` spliced into `properties` (the batch-API convention).
    fn serialize(&self, event: &PostHogEvent) -> Value {
        let mut properties: Map<String, Value> = event.properties().clone();
        properties.insert("distinct_id".into(), Value::from(self.distinct_id.clone()));
        json!({ "event": event.name, "properties": properties })
    }

    /// Drop oldest rows past [`MAX_QUEUE_ROWS`]. Cheap: one COUNT, then a single
    /// bounded DELETE only when over the cap.
    fn enforce_bound(conn: &Connection) {
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM telemetry_queue", [], |r| r.get(0))
            .unwrap_or(0);
        let over = count - MAX_QUEUE_ROWS as i64;
        if over <= 0 {
            return;
        }
        if let Err(err) = conn.execute(
            "DELETE FROM telemetry_queue WHERE id IN (
                SELECT id FROM telemetry_queue ORDER BY id ASC LIMIT ?1
             )",
            params![over],
        ) {
            log::warn!(target: "telemetry", "telemetry queue trim failed: {err}");
        }
    }

    /// Read up to `limit` of the oldest queued rows as `(id, payload_json)`.
    fn read_batch(&self, limit: usize) -> Vec<(i64, Value)> {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut stmt = match conn
            .prepare("SELECT id, payload FROM telemetry_queue ORDER BY id ASC LIMIT ?1")
        {
            Ok(s) => s,
            Err(err) => {
                log::warn!(target: "telemetry", "telemetry read_batch prepare failed: {err}");
                return Vec::new();
            }
        };
        let rows = stmt.query_map(params![limit as i64], |row| {
            let id: i64 = row.get(0)?;
            let payload: String = row.get(1)?;
            Ok((id, payload))
        });
        let Ok(rows) = rows else {
            return Vec::new();
        };
        rows.filter_map(Result::ok)
            .filter_map(|(id, payload)| {
                serde_json::from_str::<Value>(&payload)
                    .ok()
                    .map(|v| (id, v))
            })
            .collect()
    }

    /// Delete the given row ids (called only after a 2xx). Swallows errors.
    fn delete_rows(&self, ids: &[i64]) {
        if ids.is_empty() {
            return;
        }
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Build a parameterised IN-list; ids are our own row ids so the count is
        // bounded by FLUSH_BATCH_SIZE.
        let placeholders = std::iter::repeat("?")
            .take(ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("DELETE FROM telemetry_queue WHERE id IN ({placeholders})");
        let params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        if let Err(err) = conn.execute(&sql, params.as_slice()) {
            log::warn!(target: "telemetry", "telemetry delete_rows failed: {err}");
        }
    }

    /// Current queued row count (test/diagnostic helper).
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(poisoned) => poisoned.into_inner(),
        };
        conn.query_row("SELECT COUNT(*) FROM telemetry_queue", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize
    }

    /// `true` when the queue holds no rows.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// POST one batch and delete its rows on success. Returns the number of
    /// events delivered (0 on empty queue, network failure, or non-2xx — in all
    /// of which the rows stay queued for the next attempt). Never errors out:
    /// telemetry failure is invisible.
    pub async fn flush_once(&self, timeout: Duration) -> usize {
        let batch = self.read_batch(FLUSH_BATCH_SIZE);
        if batch.is_empty() {
            return 0;
        }
        let (ids, events): (Vec<i64>, Vec<Value>) = batch.into_iter().unzip();
        let body = json!({
            "api_key": self.config.api_key,
            "batch": events,
        });
        let url = format!("{}/batch/", self.config.host.trim_end_matches('/'));
        let sent = events.len();

        let result = self
            .http
            .post(&url)
            .timeout(timeout)
            .json(&body)
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                self.delete_rows(&ids);
                log::debug!(target: "telemetry", "flushed {sent} telemetry events");
                sent
            }
            Ok(resp) => {
                // Non-2xx: keep the rows, try again next tick. Don't log the
                // body (PostHog echoes nothing sensitive, but the discipline is
                // to never surface a provider body).
                log::warn!(
                    target: "telemetry",
                    "telemetry flush rejected (status={}) — {sent} events requeued",
                    resp.status()
                );
                0
            }
            Err(err) => {
                // Offline / timeout / DNS — expected and harmless; rows persist.
                log::debug!(target: "telemetry", "telemetry flush offline: {err}");
                0
            }
        }
    }

    /// Drain the queue, sending as many full batches as are pending (capped so a
    /// huge backlog can't spin forever in one pass). Used by the background tick
    /// and, with a tight timeout, by the on-exit flush.
    pub async fn flush_drain(&self, timeout: Duration) {
        // Bound the per-pass work: at most enough batches to clear the cap once.
        let max_passes = MAX_QUEUE_ROWS / FLUSH_BATCH_SIZE + 1;
        for _ in 0..max_passes {
            let sent = self.flush_once(timeout).await;
            if sent < FLUSH_BATCH_SIZE {
                // Either the queue is empty, a partial last batch went, or the
                // network failed — nothing more to gain this pass.
                break;
            }
        }
    }

    /// Best-effort flush for app exit, bounded by [`EXIT_FLUSH_DEADLINE`]. Tight
    /// per-request timeout AND a hard overall deadline so quit is never held up,
    /// even by a slow-but-responsive endpoint with a full queue. Anything unsent
    /// survives in the DB for the next launch.
    pub async fn flush_on_exit(&self) {
        self.flush_on_exit_with_deadline(EXIT_FLUSH_DEADLINE).await;
    }

    /// [`flush_on_exit`](Self::flush_on_exit) with an injectable overall
    /// deadline (tests pass a short one). On deadline the in-flight flush is
    /// dropped and the rows-remaining count is logged; those rows are durable.
    async fn flush_on_exit_with_deadline(&self, deadline: Duration) {
        if tokio::time::timeout(deadline, self.flush_drain(EXIT_FLUSH_TIMEOUT))
            .await
            .is_err()
        {
            log::warn!(
                target: "telemetry",
                "exit flush hit the {deadline:?} deadline — {} telemetry rows remain, will drain next launch",
                self.len()
            );
        }
    }
}

/// Spawn the background flush task. Runs for the program lifetime, attempting a
/// drain every [`FLUSH_INTERVAL`]. The returned task handle is dropped by the
/// caller (the process exit reaps it); the queue itself is shared via `Arc`.
pub fn spawn_flush_task(queue: Arc<TelemetryQueue>) {
    tauri::async_runtime::spawn(async move {
        let mut ticker = interval(FLUSH_INTERVAL);
        loop {
            ticker.tick().await;
            queue.flush_drain(REQUEST_TIMEOUT).await;
        }
    });
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Reserved for a future low-frequency retention event (Phase 3, task 22) that
/// must set a person profile. Documents the one place `$process_person_profile`
/// is flipped back on; unused until Phase 3 wires it.
#[allow(dead_code)]
pub(crate) const PERSON_PROFILE_PROP: &str = PROP_PROCESS_PERSON_PROFILE;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::events;
    use tempfile::tempdir;

    fn test_config() -> PostHogConfig {
        PostHogConfig {
            api_key: "phc_test".into(),
            host: "http://127.0.0.1:1".into(), // unreachable: flush always offline
        }
    }

    fn fresh_queue() -> (TelemetryQueue, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("history.sqlite3");
        let q = TelemetryQueue::open(&path, "install-uuid".into(), test_config()).expect("open");
        (q, dir)
    }

    fn sample_event() -> PostHogEvent {
        events::dictation_empty("silent_press")
    }

    #[test]
    fn enqueue_then_count_round_trips() {
        let (q, _dir) = fresh_queue();
        assert!(q.is_empty());
        q.enqueue(&sample_event());
        q.enqueue(&sample_event());
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn serialized_event_carries_distinct_id_inside_properties() {
        let (q, _dir) = fresh_queue();
        let v = q.serialize(&sample_event());
        let props = v.get("properties").and_then(|p| p.as_object()).unwrap();
        assert_eq!(
            props.get("distinct_id").and_then(|v| v.as_str()),
            Some("install-uuid"),
            "distinct_id must live inside properties for /batch/"
        );
        assert_eq!(
            v.get("event").and_then(|e| e.as_str()),
            Some(events::EVENT_DICTATION_EMPTY)
        );
        // Mandatory segmentation props survive serialization.
        assert_eq!(
            props.get(events::PROP_SOURCE).and_then(|v| v.as_str()),
            Some(events::SOURCE_DESKTOP)
        );
    }

    #[test]
    fn queue_bound_drops_oldest_past_cap() {
        let (q, _dir) = fresh_queue();
        // Push a few past the cap and assert the count clamps to MAX_QUEUE_ROWS
        // and the SURVIVING rows are the newest (highest ids).
        let overflow = 5;
        for _ in 0..(MAX_QUEUE_ROWS + overflow) {
            q.enqueue(&sample_event());
        }
        assert_eq!(q.len(), MAX_QUEUE_ROWS, "queue clamps to the bound");

        // Lowest surviving id should be > overflow (the first `overflow`+? rows
        // were dropped). The exact min id moves as the trim runs each insert; we
        // just assert the oldest ids are gone.
        let conn = q.conn.lock().unwrap();
        let min_id: i64 = conn
            .query_row("SELECT MIN(id) FROM telemetry_queue", [], |r| r.get(0))
            .unwrap();
        assert!(
            min_id > overflow as i64,
            "oldest rows were dropped (min surviving id={min_id})"
        );
    }

    #[tokio::test]
    async fn flush_on_empty_queue_is_a_noop() {
        let (q, _dir) = fresh_queue();
        let sent = q.flush_once(Duration::from_millis(50)).await;
        assert_eq!(sent, 0);
    }

    #[tokio::test]
    async fn flush_offline_keeps_rows_queued() {
        // Host is an unreachable address; the POST fails and rows must persist.
        let (q, _dir) = fresh_queue();
        q.enqueue(&sample_event());
        q.enqueue(&sample_event());
        let sent = q.flush_once(Duration::from_millis(50)).await;
        assert_eq!(sent, 0, "offline flush sends nothing");
        assert_eq!(q.len(), 2, "rows survive an offline flush for retry");
    }

    #[tokio::test]
    async fn exit_flush_honors_hard_deadline_when_endpoint_stalls() {
        // Plan 039 task 41: a slow-but-responsive endpoint with a full queue must
        // not hold up quit. The response is delayed far past both the per-request
        // timeout and the (short, injected) overall deadline; the deadline must
        // cut the flush short and the rows must survive for the next launch.
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .mount(&server)
            .await;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("history.sqlite3");
        let config = PostHogConfig {
            api_key: "phc_test".into(),
            host: server.uri(),
        };
        let q = TelemetryQueue::open(&path, "id".into(), config).expect("open");
        q.enqueue(&sample_event());
        q.enqueue(&sample_event());

        let deadline = Duration::from_millis(300);
        let start = std::time::Instant::now();
        q.flush_on_exit_with_deadline(deadline).await;
        let elapsed = start.elapsed();

        // Well under the 2s per-request timeout, proving the OVERALL deadline —
        // not the per-request timeout — is what stopped the drain.
        assert!(
            elapsed < Duration::from_secs(1),
            "exit flush must honor the deadline; took {elapsed:?}"
        );
        // Nothing was delivered (endpoint never responded) — rows persist.
        assert_eq!(q.len(), 2, "unsent rows survive for the next launch");
    }

    #[test]
    fn reopening_the_db_preserves_queued_rows() {
        // Durability: a hard exit (process gone, no flush) must leave rows for
        // the next launch to deliver.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("history.sqlite3");
        {
            let q = TelemetryQueue::open(&path, "id".into(), test_config()).expect("open #1");
            q.enqueue(&sample_event());
            assert_eq!(q.len(), 1);
        }
        let q2 = TelemetryQueue::open(&path, "id".into(), test_config()).expect("open #2");
        assert_eq!(q2.len(), 1, "queued event survived a restart");
    }
}
