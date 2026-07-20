//! Bounded mpsc → SQLite writer task for per-call usage records (feature 005).
//!
//! Each successful provider call emits a [`UsageRecord`] on a single
//! `tokio::sync::mpsc::channel` (capacity [`USAGE_CHANNEL_CAPACITY`]).
//! A dedicated tokio task drains the channel, looks up the price for
//! the call's UTC calendar month, computes `cost_usd`, and inserts
//! one row into `api_calls`.
//!
//! ## Why a single writer
//!
//! - Centralises cost computation so the five provider call sites
//!   never need to know about prices.
//! - Keeps SQLite writes off the dictation hot path — the
//!   millisecond-or-so insert is hidden behind the channel send.
//!
//! ## Backpressure policy
//!
//! Capacity 256 absorbs ~50 presses' worth of calls (5 calls per press
//! worst case) before backpressure kicks in. On full, [`try_send_drop_oldest`]
//! drops the **incoming** record with a warn log — picking "drop newest"
//! over "drop oldest" because the writer task is what drains, and a
//! drained channel that's filling at peak rate has no realistic way to
//! also drop-and-replace without inviting reorder bugs. Per the
//! brainstorm: "better to lose a usage row than to backpressure the
//! dictation hot path."

use std::sync::Arc;

use time::macros::format_description;
use time::OffsetDateTime;
use tokio::sync::mpsc;

use crate::pricing::{self, PriceKind};
use crate::usage_store::{NewApiCall, UsageStore};

/// Bounded channel capacity. Sized for ~50 presses' worth of records
/// (5 per press worst case) — far more than any realistic burst the
/// user could trigger by holding the hotkey.
pub const USAGE_CHANNEL_CAPACITY: usize = 256;

/// One per-provider-call usage record. Built at each call site after
/// a successful response; sent on the writer channel; consumed once
/// by the writer task.
///
/// `created_at_unix` should be the wall-clock time of the response
/// (or the press start, on a partial-Deepgram fallback) — the writer
/// uses it to derive the UTC `YYYY-MM` for price lookup, NOT the
/// writer's own clock at drain time.
#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub provider: String,
    pub model: String,
    pub call_kind: String,
    pub audio_seconds: Option<f64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub latency_ms: Option<i64>,
    pub status: String,
    pub request_id: Option<String>,
    pub session_id: Option<i64>,
    pub created_at_unix: i64,
}

/// Spawn the writer task and return the producer side of the channel.
///
/// The task lives for the lifetime of the process: it exits only when
/// every clone of the returned sender is dropped, which in practice
/// happens at app shutdown.
///
/// Uses `tauri::async_runtime::spawn` rather than bare `tokio::spawn`
/// so the call works from Tauri's `setup` callback. `setup` runs on
/// the main thread before tokio's reactor is attached to it; a bare
/// `tokio::spawn` panics with "there is no reactor running" in that
/// context, while Tauri's wrapper always uses the runtime it owns.
pub fn spawn_writer(store: Arc<UsageStore>) -> mpsc::Sender<UsageRecord> {
    let (tx, mut rx) = mpsc::channel::<UsageRecord>(USAGE_CHANNEL_CAPACITY);
    tauri::async_runtime::spawn(async move {
        log::debug!(target: "usage", "writer task started");
        while let Some(record) = rx.recv().await {
            // Errors are already logged inside `process_record`; the
            // loop just needs to keep going.
            let _ = process_record(&store, &record).await;
        }
        log::debug!(target: "usage", "writer task exiting (all senders dropped)");
    });
    tx
}

/// Try to enqueue `record` on the writer channel. On a full channel
/// the new record is dropped with a warn log so the dictation hot
/// path stays unblocked. The error case is intentionally swallowed —
/// callers are fire-and-forget side-effect emitters.
pub fn try_send_drop_oldest(tx: &mpsc::Sender<UsageRecord>, record: UsageRecord) {
    let provider = record.provider.clone();
    let model = record.model.clone();
    if let Err(err) = tx.try_send(record) {
        match err {
            mpsc::error::TrySendError::Full(_) => {
                log::warn!(
                    target: "usage",
                    "channel full ({USAGE_CHANNEL_CAPACITY}) — dropping {provider}/{model} record"
                );
            }
            mpsc::error::TrySendError::Closed(_) => {
                log::warn!(
                    target: "usage",
                    "channel closed — dropping {provider}/{model} record (writer dead?)"
                );
            }
        }
    }
}

async fn process_record(store: &Arc<UsageStore>, record: &UsageRecord) -> Result<(), ()> {
    let yyyymm = match unix_to_yyyymm(record.created_at_unix) {
        Some(s) => s,
        None => {
            log::warn!(
                target: "usage",
                "could not format created_at={} as YYYY-MM — skipping cost computation",
                record.created_at_unix,
            );
            String::new()
        }
    };

    let mut cost_usd: Option<f64> = None;
    if !yyyymm.is_empty() {
        match store.lookup_price(&yyyymm, &record.provider, &record.model) {
            Ok(Some(price)) => {
                let kind = PriceKind::from_wire(&price.kind);
                if let Some(kind) = kind {
                    cost_usd = pricing::compute_cost(
                        kind,
                        record.audio_seconds,
                        record.input_tokens,
                        record.output_tokens,
                        price.usd_per_second,
                        price.usd_per_input_token,
                        price.usd_per_output_token,
                    );
                    if cost_usd.is_none() {
                        log::warn!(
                            target: "usage",
                            "compute_cost returned None for {}/{} ({}-priced) — usage units missing",
                            record.provider, record.model, price.kind,
                        );
                    }
                } else {
                    log::warn!(
                        target: "usage",
                        "unrecognised price kind {:?} for {}/{} — recording cost=NULL",
                        price.kind, record.provider, record.model,
                    );
                }
            }
            Ok(None) => {
                log::warn!(
                    target: "usage",
                    "no price for {}/{} in {} — recording cost=NULL",
                    record.provider, record.model, yyyymm,
                );
            }
            Err(err) => {
                log::warn!(
                    target: "usage",
                    "price lookup failed for {}/{}: {} — recording cost=NULL",
                    record.provider, record.model, err.user_message(),
                );
            }
        }
    }

    let new_call = NewApiCall {
        created_at: record.created_at_unix,
        provider: record.provider.clone(),
        model: record.model.clone(),
        call_kind: record.call_kind.clone(),
        audio_seconds: record.audio_seconds,
        input_tokens: record.input_tokens,
        output_tokens: record.output_tokens,
        cost_usd,
        latency_ms: record.latency_ms,
        status: record.status.clone(),
        request_id: record.request_id.clone(),
        session_id: record.session_id,
    };

    if let Err(err) = store.insert_call(new_call) {
        log::warn!(
            target: "usage",
            "insert_call failed for {}/{}: {}",
            record.provider, record.model, err.user_message(),
        );
        return Err(());
    }
    log::debug!(
        target: "usage",
        "recorded {}/{} kind={} cost={:?} status={}",
        record.provider, record.model, record.call_kind, cost_usd, record.status,
    );
    Ok(())
}

/// Format a unix-seconds timestamp as a UTC `YYYY-MM` string. Returns
/// `None` only on pathological inputs (e.g. timestamps outside `time`'s
/// supported range — unreachable in practice).
fn unix_to_yyyymm(unix_secs: i64) -> Option<String> {
    const FMT: &[time::format_description::FormatItem<'_>] = format_description!("[year]-[month]");
    OffsetDateTime::from_unix_timestamp(unix_secs)
        .ok()
        .and_then(|dt| dt.format(&FMT).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::DEFAULT_PRICES;
    use crate::usage_store::{PriceRow, UsageStore};
    use std::time::Duration;
    use tempfile::tempdir;

    /// 2026-05-15T12:00:00Z.
    const MAY_15: i64 = 1_779_192_000;

    fn open_store(dir: &tempfile::TempDir) -> Arc<UsageStore> {
        let path = crate::history_store::HistoryStore::default_path(dir.path());
        let _hist = crate::history_store::HistoryStore::open(&path).unwrap();
        Arc::new(UsageStore::open(&path).unwrap())
    }

    fn seed_default_prices(store: &UsageStore, yyyymm: &str) {
        for entry in DEFAULT_PRICES {
            let row = PriceRow {
                effective_month: yyyymm.into(),
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
    }

    #[test]
    fn unix_to_yyyymm_formats_utc() {
        assert_eq!(unix_to_yyyymm(MAY_15).unwrap(), "2026-05");
    }

    #[tokio::test]
    async fn writer_drains_records_on_close() {
        let dir = tempdir().unwrap();
        let store = open_store(&dir);
        seed_default_prices(&store, "2026-05");

        let tx = spawn_writer(Arc::clone(&store));
        tx.send(UsageRecord {
            provider: "deepgram".into(),
            model: "nova-3".into(),
            call_kind: "asr".into(),
            audio_seconds: Some(60.0),
            input_tokens: None,
            output_tokens: None,
            latency_ms: Some(500),
            status: "ok".into(),
            request_id: Some("rq-1".into()),
            session_id: None,
            created_at_unix: MAY_15,
        })
        .await
        .unwrap();
        // Drop the sender so the writer task exits cleanly, then give it
        // a moment to drain.
        drop(tx);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let summary = store.summary_for_month("2026-05").unwrap();
        assert_eq!(summary.len(), 1);
        let row = &summary[0];
        assert_eq!(row.provider, "deepgram");
        assert_eq!(row.call_count, 1);
        // Rate must track the Deepgram nova-3 entry in `DEFAULT_PRICES`
        // (`pricing.rs`), which this test seeds via `seed_default_prices`.
        // It was corrected to 0.000_110/sec in 1aa31c3 (the old 0.000_08
        // under-counted the bill by ~37%); this assertion was left stale.
        assert!((row.total_usd.unwrap() - 60.0 * 0.000_110).abs() < 1e-12);
    }

    #[tokio::test]
    async fn writer_records_cost_null_when_price_missing() {
        let dir = tempdir().unwrap();
        let store = open_store(&dir);
        // No prices seeded.
        let tx = spawn_writer(Arc::clone(&store));
        tx.send(UsageRecord {
            provider: "groq".into(),
            model: "openai/gpt-oss-120b".into(),
            call_kind: "cleanup".into(),
            audio_seconds: None,
            input_tokens: Some(100),
            output_tokens: Some(50),
            latency_ms: Some(800),
            status: "ok".into(),
            request_id: None,
            session_id: None,
            created_at_unix: MAY_15,
        })
        .await
        .unwrap();
        drop(tx);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let per_model = store.summary_for_month_per_model("2026-05").unwrap();
        assert_eq!(per_model.len(), 1);
        // total_usd is NULL when every cost_usd is NULL — SUM(NULL) = NULL.
        assert!(per_model[0].total_usd.is_none());
        assert_eq!(per_model[0].input_tokens.unwrap(), 100);
        assert_eq!(per_model[0].output_tokens.unwrap(), 50);
    }

    #[tokio::test]
    async fn writer_handles_token_priced_call() {
        let dir = tempdir().unwrap();
        let store = open_store(&dir);
        seed_default_prices(&store, "2026-05");

        let tx = spawn_writer(Arc::clone(&store));
        tx.send(UsageRecord {
            provider: "groq".into(),
            model: "openai/gpt-oss-120b".into(),
            call_kind: "cleanup".into(),
            audio_seconds: None,
            input_tokens: Some(1_000_000),
            output_tokens: Some(500_000),
            latency_ms: Some(900),
            status: "ok".into(),
            request_id: None,
            session_id: None,
            created_at_unix: MAY_15,
        })
        .await
        .unwrap();
        drop(tx);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let summary = store.summary_for_month("2026-05").unwrap();
        assert_eq!(summary.len(), 1);
        let expected = 1_000_000.0 * 0.000_000_15 + 500_000.0 * 0.000_000_60;
        assert!((summary[0].total_usd.unwrap() - expected).abs() < 1e-9);
    }

    #[tokio::test]
    async fn try_send_drop_oldest_drops_when_channel_full() {
        // Tiny capacity simulated via a hand-built channel (we can't
        // reduce USAGE_CHANNEL_CAPACITY in tests). Verifies the helper
        // doesn't panic and logs a warn — which is all we promise.
        let (tx, _rx) = mpsc::channel::<UsageRecord>(1);
        // Fill the channel — `_rx` is held but never read, so the
        // single slot stays full.
        tx.try_send(UsageRecord {
            provider: "deepgram".into(),
            model: "nova-3".into(),
            call_kind: "asr".into(),
            audio_seconds: Some(1.0),
            input_tokens: None,
            output_tokens: None,
            latency_ms: None,
            status: "ok".into(),
            request_id: None,
            session_id: None,
            created_at_unix: MAY_15,
        })
        .unwrap();
        // The next try_send_drop_oldest must NOT panic and must drop
        // the new record cleanly.
        try_send_drop_oldest(
            &tx,
            UsageRecord {
                provider: "deepgram".into(),
                model: "nova-3".into(),
                call_kind: "asr".into(),
                audio_seconds: Some(2.0),
                input_tokens: None,
                output_tokens: None,
                latency_ms: None,
                status: "ok".into(),
                request_id: None,
                session_id: None,
                created_at_unix: MAY_15,
            },
        );
    }
}
