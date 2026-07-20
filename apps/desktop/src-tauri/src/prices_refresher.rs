//! Background tokio task that keeps `price_history` current
//! (feature 005).
//!
//! ## Cadence
//!
//! 0. **Token gate** — if no bearer token was resolved at construction
//!    (dev builds, or before the prices API is live), the refresher
//!    logs once and idles: no launch burst, no hourly tick. Every fetch
//!    would only yield `MissingToken`, and the token can't materialise
//!    without a relaunch, so retrying just spams the log.
//! 1. **Launch burst** — if the stored `last_priced_success_at`'s UTC
//!    `YYYY-MM` ≠ today's, three attempts at offsets 0/5/15 s. First
//!    success commits the rows for the current month and stamps
//!    `last_priced_success_at`.
//! 2. **Hourly tick thereafter** — every hour the gate is rechecked
//!    (and an attempt fires only when the month has rolled over since
//!    the last success). Catches mid-uptime month changes and
//!    transient API outages without needing a relaunch.
//!
//! `tokio::time::interval` is used for the hourly tick so monotonic
//! firing covers post-suspend wake-ups uniformly.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use time::macros::format_description;
use time::OffsetDateTime;

use crate::prices_client::{PricesClient, PricesFetchError, RawPriceEntry};
use crate::usage_store::{PriceRow, UsageStore};

/// Hourly tick cadence. Cheap (one HTTP fetch worst-case per hour);
/// catches month rollovers and any post-suspend gap without a
/// platform-specific wake hook.
const HOURLY_TICK: Duration = Duration::from_secs(3_600);

/// Launch-burst offsets: first attempt fires immediately, then 5 s
/// and 15 s later. Three attempts is the brainstorm-spec'd ceiling
/// — no exponential backoff between them since once-a-month traffic
/// can't realistically saturate any sane scraper.
const LAUNCH_BURST_DELAYS: &[Duration] = &[
    Duration::from_secs(0),
    Duration::from_secs(5),
    Duration::from_secs(15),
];

/// Spawn the refresher task. Runs for the lifetime of the process.
pub fn spawn(
    store: Arc<UsageStore>,
    client: Arc<PricesClient>,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        run_refresher(store, client).await;
    })
}

async fn run_refresher(store: Arc<UsageStore>, client: Arc<PricesClient>) {
    log::info!(target: "pricing", "refresher starting");

    // No token → every fetch would fail with `MissingToken` and the
    // token can't appear until the process is relaunched (it's resolved
    // once at construction). Idle instead of spamming a WARN on the
    // launch burst and every hourly tick; the `DEFAULT_PRICES` bootstrap
    // rows stay in place. This is the common dev-build / pre-live-API
    // case. A present-but-invalid token is a different path (HTTP 401),
    // which still retries so a rotation can recover mid-uptime.
    if !client.has_token() {
        log::info!(
            target: "pricing",
            "prices token not injected — refresher idle, keeping DEFAULT_PRICES bootstrap rows"
        );
        return;
    }

    if is_fetch_due(&store) {
        log::info!(target: "pricing", "launch burst — month changed since last success");
        for (i, delay) in LAUNCH_BURST_DELAYS.iter().enumerate() {
            if !delay.is_zero() {
                tokio::time::sleep(*delay).await;
            }
            log::info!(
                target: "pricing",
                "launch burst attempt {}/{}",
                i + 1,
                LAUNCH_BURST_DELAYS.len()
            );
            if attempt_fetch_and_persist(&store, &client).await.is_ok() {
                log::info!(target: "pricing", "launch burst succeeded");
                break;
            }
        }
    } else {
        log::debug!(target: "pricing", "launch burst skipped — already current");
    }

    let mut ticker = tokio::time::interval(HOURLY_TICK);
    // Skip the immediate first tick — the launch burst above already
    // exercised the gate. Subsequent ticks fire every HOURLY_TICK.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        if !is_fetch_due(&store) {
            log::debug!(target: "pricing", "hourly tick — month unchanged, skipping");
            continue;
        }
        log::info!(target: "pricing", "hourly tick — month changed, attempting fetch");
        let _ = attempt_fetch_and_persist(&store, &client).await;
    }
}

/// True when the stored `last_priced_success_at`'s UTC `YYYY-MM` is
/// older than the current UTC `YYYY-MM`. A missing
/// `last_priced_success_at` (fresh install) also counts as due so the
/// refresher fires its launch burst on the very first run.
pub fn is_fetch_due(store: &UsageStore) -> bool {
    let last = store.last_priced_success_at().ok().flatten();
    let now_yyyymm = current_yyyymm();
    match last {
        Some(unix_secs) => {
            let last_yyyymm = unix_to_yyyymm(unix_secs).unwrap_or_default();
            last_yyyymm != now_yyyymm
        }
        None => true,
    }
}

/// Run one fetch + persist cycle. Always stamps
/// `last_priced_attempt_at` (success or failure); only stamps
/// `last_priced_success_at` after every row upsert lands.
pub async fn attempt_fetch_and_persist(
    store: &Arc<UsageStore>,
    client: &Arc<PricesClient>,
) -> Result<(), PricesFetchError> {
    let now = unix_seconds_now();
    if let Err(err) = store.set_last_priced_attempt_at(now) {
        log::warn!(
            target: "pricing",
            "set_last_priced_attempt_at failed: {} (continuing)",
            err.user_message()
        );
    }
    let prices = match client.fetch().await {
        Ok(p) => p,
        Err(err) => {
            log::warn!(target: "pricing", "fetch failed: {err}");
            return Err(err);
        }
    };
    let yyyymm = current_yyyymm();
    persist_prices(store, &prices, &yyyymm, now);
    if let Err(err) = store.set_last_priced_success_at(now) {
        log::warn!(
            target: "pricing",
            "set_last_priced_success_at failed: {} (rows landed; metadata stale)",
            err.user_message()
        );
    }
    log::info!(
        target: "pricing",
        "refreshed {} price rows for {}",
        prices.len(),
        yyyymm
    );
    Ok(())
}

fn persist_prices(store: &Arc<UsageStore>, prices: &[RawPriceEntry], yyyymm: &str, now: i64) {
    for raw in prices {
        let row = PriceRow {
            effective_month: yyyymm.to_string(),
            provider: raw.provider.clone(),
            model: raw.model.clone(),
            kind: raw.kind.clone(),
            usd_per_second: raw.usd_per_second,
            usd_per_input_token: raw.usd_per_input_token,
            usd_per_output_token: raw.usd_per_output_token,
            source_url: raw.source_url.clone(),
            fetched_at: now,
        };
        if let Err(err) = store.upsert_price(&row) {
            log::warn!(
                target: "pricing",
                "upsert failed for {}/{}: {}",
                raw.provider,
                raw.model,
                err.user_message()
            );
        }
    }
}

fn unix_seconds_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn current_yyyymm() -> String {
    const FMT: &[time::format_description::FormatItem<'_>] = format_description!("[year]-[month]");
    OffsetDateTime::now_utc()
        .format(&FMT)
        .unwrap_or_else(|_| "1970-01".into())
}

fn unix_to_yyyymm(unix_secs: i64) -> Option<String> {
    const FMT: &[time::format_description::FormatItem<'_>] = format_description!("[year]-[month]");
    OffsetDateTime::from_unix_timestamp(unix_secs)
        .ok()
        .and_then(|dt| dt.format(&FMT).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_store::HistoryStore;
    use serde_json::json;
    use tempfile::tempdir;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn open_store() -> (Arc<UsageStore>, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let path = HistoryStore::default_path(dir.path());
        let _hist = HistoryStore::open(&path).unwrap();
        (Arc::new(UsageStore::open(&path).unwrap()), dir)
    }

    #[test]
    fn is_fetch_due_returns_true_on_fresh_install() {
        let (store, _d) = open_store();
        assert!(is_fetch_due(&store));
    }

    #[test]
    fn is_fetch_due_returns_false_when_same_month_recorded() {
        let (store, _d) = open_store();
        store
            .set_last_priced_success_at(unix_seconds_now())
            .unwrap();
        assert!(!is_fetch_due(&store));
    }

    #[test]
    fn is_fetch_due_returns_true_when_month_rolls_over() {
        let (store, _d) = open_store();
        // 2025-04-15T12:00:00Z = 1744718400 — definitely a different
        // month than `now()`.
        store.set_last_priced_success_at(1_744_718_400).unwrap();
        assert!(is_fetch_due(&store));
    }

    #[tokio::test]
    async fn attempt_fetch_persists_rows_and_bumps_success_at() {
        let (store, _d) = open_store();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({
                        "schema_version": 1,
                        "generated_at": "2026-05-06T00:00:00Z",
                        "prices": [
                            {
                                "provider": "deepgram",
                                "model": "nova-3",
                                "kind": "per_audio_second",
                                "usd_per_second": 0.00008,
                                "usd_per_input_token": null,
                                "usd_per_output_token": null,
                                "source_url": "https://deepgram.com/pricing"
                            }
                        ]
                    })),
            )
            .mount(&server)
            .await;
        let client =
            Arc::new(PricesClient::for_testing(server.uri(), Some("test-token-****ABCD")).unwrap());

        attempt_fetch_and_persist(&store, &client).await.unwrap();
        let yyyymm = current_yyyymm();
        let listed = store.list_prices_for_month(&yyyymm).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(store.last_priced_success_at().unwrap().is_some());
        assert!(store.last_priced_attempt_at().unwrap().is_some());
    }

    #[tokio::test]
    async fn run_refresher_idles_without_a_token_and_never_attempts() {
        // Regression: a token-less build (dev, or before the prices API
        // is live) must NOT enter the launch burst / hourly loop. Doing
        // so spammed `[pricing][WARN] fetch failed: MUNI_PRICES_TOKEN not
        // injected` at 0/5/15 s then hourly. The token gate should make
        // run_refresher return immediately without ever stamping an
        // attempt.
        let (store, _d) = open_store();
        // `None` token → has_token() == false. URL is irrelevant; it must
        // never be dialed.
        let client = Arc::new(
            PricesClient::for_testing("http://127.0.0.1:1/never-dialed".to_string(), None).unwrap(),
        );
        assert!(!client.has_token(), "precondition: client has no token");

        // With the gate, this returns instantly. Without it, the burst's
        // 5 s/15 s sleeps (then the infinite hourly loop) would blow the
        // timeout — so a timeout here IS the regression signal.
        let outcome =
            tokio::time::timeout(Duration::from_secs(2), run_refresher(store.clone(), client))
                .await;
        assert!(
            outcome.is_ok(),
            "token-less refresher must return promptly, not sit in the burst/hourly loop"
        );
        assert!(
            store.last_priced_attempt_at().unwrap().is_none(),
            "a token-less refresher must never attempt a fetch (no attempt timestamp stamped)"
        );
    }

    #[tokio::test]
    async fn attempt_fetch_failure_bumps_attempt_but_not_success() {
        let (store, _d) = open_store();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("server down"))
            .mount(&server)
            .await;
        let client =
            Arc::new(PricesClient::for_testing(server.uri(), Some("test-token-****ABCD")).unwrap());

        let result = attempt_fetch_and_persist(&store, &client).await;
        assert!(result.is_err());
        assert!(store.last_priced_success_at().unwrap().is_none());
        assert!(store.last_priced_attempt_at().unwrap().is_some());
    }
}
