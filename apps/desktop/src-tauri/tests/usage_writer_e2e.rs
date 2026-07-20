//! End-to-end integration test for the usage-writer task (feature 005).
//!
//! Drives the public spawn_writer surface, sends a representative
//! mix of records, and asserts the resulting `api_calls` rows reflect
//! the expected cost computation.

use std::sync::Arc;
use std::time::Duration;

use muni_lib::history_store::HistoryStore;
use muni_lib::pricing::DEFAULT_PRICES;
use muni_lib::usage_store::{PriceRow, UsageStore};
use muni_lib::usage_writer::{spawn_writer, UsageRecord};
use tempfile::tempdir;

fn open_store(dir: &tempfile::TempDir) -> Arc<UsageStore> {
    let path = HistoryStore::default_path(dir.path());
    let _hist = HistoryStore::open(&path).unwrap();
    Arc::new(UsageStore::open(&path).unwrap())
}

fn seed_prices(store: &UsageStore, yyyymm: &str) {
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

/// 2026-05-15T12:00:00Z.
const MAY_15: i64 = 1_779_192_000;

#[tokio::test]
async fn end_to_end_mix_of_records_lands_with_correct_costs() {
    let dir = tempdir().unwrap();
    let store = open_store(&dir);
    seed_prices(&store, "2026-05");

    let tx = spawn_writer(Arc::clone(&store));

    // Deepgram ASR — 30 seconds.
    tx.send(UsageRecord {
        provider: "deepgram".into(),
        model: "nova-3".into(),
        call_kind: "asr".into(),
        audio_seconds: Some(30.0),
        input_tokens: None,
        output_tokens: None,
        latency_ms: Some(500),
        status: "ok".into(),
        request_id: None,
        session_id: None,
        created_at_unix: MAY_15,
    })
    .await
    .unwrap();

    // Groq cleanup — 1000 input + 200 output tokens.
    tx.send(UsageRecord {
        provider: "groq".into(),
        model: "openai/gpt-oss-120b".into(),
        call_kind: "cleanup".into(),
        audio_seconds: None,
        input_tokens: Some(1_000),
        output_tokens: Some(200),
        latency_ms: Some(800),
        status: "ok".into(),
        request_id: None,
        session_id: None,
        created_at_unix: MAY_15 + 30,
    })
    .await
    .unwrap();

    // Gemini LID — 50 input + 1 output tokens.
    tx.send(UsageRecord {
        provider: "gemini".into(),
        model: "gemini-2.5-flash-lite".into(),
        call_kind: "lid".into(),
        audio_seconds: None,
        input_tokens: Some(50),
        output_tokens: Some(1),
        latency_ms: Some(900),
        status: "ok".into(),
        request_id: None,
        session_id: None,
        created_at_unix: MAY_15 + 60,
    })
    .await
    .unwrap();

    // Drop sender so the writer drains and exits.
    drop(tx);
    tokio::time::sleep(Duration::from_millis(80)).await;

    let summary = store.summary_for_month("2026-05").unwrap();
    assert_eq!(summary.len(), 3, "three providers");

    let dg = summary.iter().find(|s| s.provider == "deepgram").unwrap();
    assert_eq!(dg.call_count, 1);
    // Deepgram nova-3 rate corrected to $0.000110/s in #119 ("fix(cost):
    // correct Deepgram rate"); this assertion was missed in that commit and
    // still expected the old $0.00008/s. Kept in lockstep with DEFAULT_PRICES.
    let dg_expected = 30.0 * 0.000_110;
    assert!((dg.total_usd.unwrap() - dg_expected).abs() < 1e-12);

    let groq = summary.iter().find(|s| s.provider == "groq").unwrap();
    // gpt-oss-120b: $0.15/M input, $0.60/M output (DEFAULT_PRICES). Updated
    // from the decommissioned llama-3.3-70b-versatile entry; kept in lockstep
    // with the live rate sheet so the cost lookup resolves a non-null cost.
    let groq_expected = 1_000.0 * 0.000_000_15 + 200.0 * 0.000_000_60;
    assert!((groq.total_usd.unwrap() - groq_expected).abs() < 1e-12);

    let gem = summary.iter().find(|s| s.provider == "gemini").unwrap();
    let gem_expected = 50.0 * 0.000_000_1 + 1.0 * 0.000_000_4;
    assert!((gem.total_usd.unwrap() - gem_expected).abs() < 1e-12);
}

#[tokio::test]
async fn missing_price_records_row_with_null_cost_but_keeps_usage() {
    // Don't seed prices. The row still lands so cost can be re-derived
    // later if a price arrives — feature 005 contract.
    let dir = tempdir().unwrap();
    let store = open_store(&dir);
    let tx = spawn_writer(Arc::clone(&store));

    tx.send(UsageRecord {
        provider: "groq".into(),
        model: "llama-3.3-70b-versatile".into(),
        call_kind: "cleanup".into(),
        audio_seconds: None,
        input_tokens: Some(1_234),
        output_tokens: Some(56),
        latency_ms: Some(700),
        status: "ok".into(),
        request_id: None,
        session_id: None,
        created_at_unix: MAY_15,
    })
    .await
    .unwrap();
    drop(tx);
    tokio::time::sleep(Duration::from_millis(60)).await;

    let per_model = store.summary_for_month_per_model("2026-05").unwrap();
    assert_eq!(per_model.len(), 1);
    assert!(per_model[0].total_usd.is_none());
    assert_eq!(per_model[0].input_tokens.unwrap(), 1234);
    assert_eq!(per_model[0].output_tokens.unwrap(), 56);
}
