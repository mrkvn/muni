//! Integration test for the prices-refresher gating logic
//! (feature 005). Exercises `is_fetch_due` and
//! `attempt_fetch_and_persist` against a live wiremock server.

use std::sync::Arc;

use muni_lib::history_store::HistoryStore;
use muni_lib::prices_client::PricesClient;
use muni_lib::prices_refresher::{attempt_fetch_and_persist, is_fetch_due};
use muni_lib::usage_store::UsageStore;
use serde_json::json;
use tempfile::tempdir;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_TOKEN: &str = "test-bearer-****ABCD";

fn open_store() -> (Arc<UsageStore>, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let path = HistoryStore::default_path(dir.path());
    let _ = HistoryStore::open(&path).unwrap();
    (Arc::new(UsageStore::open(&path).unwrap()), dir)
}

fn fixture(provider: &str, model: &str, usd_per_second: Option<f64>) -> serde_json::Value {
    json!({
        "schema_version": 1,
        "generated_at": "2026-05-06T00:00:00Z",
        "prices": [{
            "provider": provider,
            "model": model,
            "kind": "per_audio_second",
            "usd_per_second": usd_per_second,
            "usd_per_input_token": null,
            "usd_per_output_token": null,
            "source_url": "https://example.test/pricing"
        }]
    })
}

#[tokio::test]
async fn fresh_store_is_due_then_success_clears_due_state() {
    let (store, _d) = open_store();
    assert!(is_fetch_due(&store), "fresh install must be due");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(fixture("deepgram", "nova-3", Some(0.00008))),
        )
        .mount(&server)
        .await;
    let client = Arc::new(PricesClient::for_testing(server.uri(), Some(TEST_TOKEN)).unwrap());

    attempt_fetch_and_persist(&store, &client).await.unwrap();

    // Same calendar month → no longer due.
    assert!(!is_fetch_due(&store));
    let success = store.last_priced_success_at().unwrap();
    assert!(success.is_some());
    let attempt = store.last_priced_attempt_at().unwrap();
    assert!(attempt.is_some());
}

#[tokio::test]
async fn http_failure_does_not_bump_success_at() {
    let (store, _d) = open_store();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).set_body_string("scraper down"))
        .mount(&server)
        .await;
    let client = Arc::new(PricesClient::for_testing(server.uri(), Some(TEST_TOKEN)).unwrap());

    let result = attempt_fetch_and_persist(&store, &client).await;
    assert!(result.is_err());
    assert!(store.last_priced_success_at().unwrap().is_none());
    assert!(store.last_priced_attempt_at().unwrap().is_some());
    // Still due — refresher's hourly tick will retry.
    assert!(is_fetch_due(&store));
}

#[tokio::test]
async fn second_success_overwrites_existing_price_for_same_month() {
    let (store, _d) = open_store();
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(fixture("deepgram", "nova-3", Some(0.00010))),
        )
        .mount(&server)
        .await;
    let client = Arc::new(PricesClient::for_testing(server.uri(), Some(TEST_TOKEN)).unwrap());

    attempt_fetch_and_persist(&store, &client).await.unwrap();

    // Force "due" again by predating last_priced_success_at to a
    // distant prior month, then re-attempt with a different price.
    store.set_last_priced_success_at(0).unwrap();
    assert!(is_fetch_due(&store));

    // Re-mount with the new value.
    let server2 = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(fixture("deepgram", "nova-3", Some(0.00012))),
        )
        .mount(&server2)
        .await;
    let client2 = Arc::new(PricesClient::for_testing(server2.uri(), Some(TEST_TOKEN)).unwrap());
    attempt_fetch_and_persist(&store, &client2).await.unwrap();

    // Look up the row for the current month — exactly one,
    // overwritten with the latest rate.
    let yyyymm = current_month();
    let listed = store.list_prices_for_month(&yyyymm).unwrap();
    assert_eq!(listed.len(), 1);
    assert!((listed[0].usd_per_second.unwrap() - 0.00012).abs() < 1e-12);
}

fn current_month() -> String {
    use time::macros::format_description;
    use time::OffsetDateTime;
    const FMT: &[time::format_description::FormatItem<'_>] = format_description!("[year]-[month]");
    OffsetDateTime::now_utc().format(&FMT).unwrap()
}
