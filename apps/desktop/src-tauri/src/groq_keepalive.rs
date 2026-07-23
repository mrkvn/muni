//! Plan 041 (task 8) — Groq connection-pool keepalive.
//!
//! A detached, process-lifetime background task that pings Groq's
//! lightweight `GET /openai/v1/models` endpoint every 240 s so the
//! shared [`crate::groq::shared_groq_http`] connection pool never goes
//! cold between presses. Groq (behind a CDN) drops idle TCP/TLS
//! connections after a few minutes; the first press after that gap pays
//! a fresh handshake (~200–500 ms) on top of model latency. This task
//! keeps one connection alive so that tax is never charged during
//! normal use.
//!
//! ## Why the SAME pool
//!
//! The keepalive is spawned with a *clone* of the exact
//! `reqwest::Client` the cleanup / Whisper / LID clients use. Cloning
//! shares the pool (reqwest `Client` is internally `Arc`), so the
//! connection this task keeps warm is the one a real press reuses —
//! pinging a different client would warm a pool nobody dictates through.
//!
//! ## Skip gate
//!
//! Each tick first checks [`GroqActivity::secs_since_call`]: if a real
//! Groq request landed inside the tick window, the pool is already warm
//! and the ping is skipped. The keepalive deliberately does NOT call
//! `note_call` / `note_prefix_touch` — it warms TCP, not Groq's prompt
//! cache, and counting its own pings as "activity" would mask real
//! traffic staleness from the re-warm gate.
//!
//! ## Off-switch
//!
//! `MUNI_GROQ_KEEPALIVE=false` (case-insensitive, trim-ws) disables the
//! task at its spawn site. Default is enabled — the safe default for a
//! latency-win feature is "on".

use std::sync::Arc;
use std::time::Duration;

use tauri::async_runtime::JoinHandle;

use crate::groq_activity::GroqActivity;
use crate::secrets;

/// Env var that disables the keepalive at its spawn site. Default
/// behaviour (unset / any other value) is "keepalive enabled."
pub const KEEPALIVE_ENV: &str = "MUNI_GROQ_KEEPALIVE";

/// Groq's cheapest authenticated GET — lists available models. Used
/// purely to send bytes over (and so keep warm) a pooled connection;
/// the response is drained and discarded. Parameterised on
/// [`spawn`] so tests can point it at a `wiremock` server.
pub const DEFAULT_MODELS_ENDPOINT: &str = "https://api.groq.com/openai/v1/models";

/// Ping cadence. 240 s (4 min) sits comfortably inside Groq's idle-drop
/// window while staying well under the 300 s `pool_idle_timeout` the
/// shared client sets, so a kept-warm connection is never evicted from
/// the pool between pings.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(240);

/// Per-request timeout for the ping itself. Short — a keepalive that
/// stalls is worthless; better to drop it and try again next tick.
const KEEPALIVE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Read `MUNI_GROQ_KEEPALIVE` and return `false` only when it is
/// explicitly set to `"false"` (case-insensitive, trim-ws). Mirrors
/// [`crate::groq_warmup::is_enabled`] — default enabled.
pub fn is_enabled() -> bool {
    !std::env::var(KEEPALIVE_ENV)
        .map(|v| v.trim().eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

/// What one keepalive tick did. Returned by [`keepalive_tick`] so the
/// loop can log and tests can assert on the outcome without scraping
/// logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepaliveOutcome {
    /// A real Groq call landed inside the tick window — pool already
    /// warm, no ping sent.
    SkippedRecentCall,
    /// No Groq API key resolved — nothing to authenticate with; idle
    /// until a key is saved (mirrors the warm-up's missing-key skip).
    NoApiKey,
    /// Ping sent and answered with this HTTP status.
    Pinged(u16),
    /// Ping attempted but the transport failed (timeout / connection
    /// error). Logged at warn; the next tick retries.
    Failed,
}

/// Spawn the detached keepalive task. Runs for the lifetime of the
/// process; the caller drops the returned `JoinHandle` on the floor
/// (mirrors [`crate::prices_refresher::spawn`]).
///
/// `http` MUST be a clone of the shared Groq client so the pinged
/// connection is the one real presses reuse. `endpoint` is the models
/// URL ([`DEFAULT_MODELS_ENDPOINT`] in production, a mock URL in tests).
pub fn spawn(
    http: reqwest::Client,
    activity: Arc<GroqActivity>,
    endpoint: String,
) -> JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        run_keepalive(http, activity, endpoint).await;
    })
}

/// Task body: interval loop that pings once per tick (subject to the
/// skip gate). Extracted from [`spawn`] so the loop shape is obvious and
/// the single-tick logic ([`keepalive_tick`]) stays independently
/// testable.
async fn run_keepalive(http: reqwest::Client, activity: Arc<GroqActivity>, endpoint: String) {
    log::info!(
        target: "groq_keepalive",
        "keepalive starting (interval={}s endpoint={})",
        KEEPALIVE_INTERVAL.as_secs(),
        endpoint
    );
    let mut ticker = tokio::time::interval(KEEPALIVE_INTERVAL);
    // Consume the immediate first tick — `tokio::time::interval` fires
    // at t=0. Pinging on boot is redundant with the Boot warm-up that
    // just warmed the pool, so wait a full interval before the first
    // real ping.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let _ = keepalive_tick(&http, &activity, &endpoint).await;
    }
}

/// Perform one keepalive tick: gate, resolve key, ping, drain. Pure of
/// timing so tests can drive it directly against a `wiremock` server.
async fn keepalive_tick(
    http: &reqwest::Client,
    activity: &GroqActivity,
    endpoint: &str,
) -> KeepaliveOutcome {
    // Skip when real traffic already kept the pool warm inside the
    // window. `secs_since_call() == i64::MAX` (never called) is always
    // ≥ the window, so a fresh boot still pings.
    let interval_secs = KEEPALIVE_INTERVAL.as_secs() as i64;
    if activity.secs_since_call() < interval_secs {
        log::debug!(
            target: "groq_keepalive",
            "skip: real Groq call within {interval_secs}s"
        );
        return KeepaliveOutcome::SkippedRecentCall;
    }

    // Resolve the key per-tick (never cached, never logged) — mirrors
    // the warm-up's missing-key skip so a key saved mid-session takes
    // effect on the next tick.
    let api_key = match secrets::get(secrets::GROQ_ACCOUNT) {
        Ok(k) if !k.is_empty() => k,
        _ => {
            log::debug!(target: "groq_keepalive", "skip: no Groq API key");
            return KeepaliveOutcome::NoApiKey;
        }
    };

    match http
        .get(endpoint)
        .bearer_auth(api_key)
        .timeout(KEEPALIVE_REQUEST_TIMEOUT)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            // Drain the (small but non-empty) body so the connection is
            // released back to the pool cleanly instead of being closed
            // with an unread body.
            let _ = resp.bytes().await;
            log::debug!(target: "groq_keepalive", "ping ok: status={status}");
            KeepaliveOutcome::Pinged(status)
        }
        Err(err) => {
            // Never interpolate the key — `reqwest`'s error carries the
            // URL and cause only, never the Authorization header.
            log::warn!(target: "groq_keepalive", "ping failed: {err}");
            KeepaliveOutcome::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::GROQ_ENV_VAR;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TEST_API_KEY: &str = "sk-test-****ABCD";

    /// Serialize env-mutating tests (`MUNI_GROQ_KEEPALIVE`,
    /// `MUNI_GROQ_KEY`, `MUNI_KEYCHAIN_SUFFIX`). `tokio::sync::Mutex`
    /// (vs `std::sync::Mutex`) because these tests await across the lock,
    /// and a sync guard held across `.await` trips
    /// `clippy::await_holding_lock`. Mirrors `groq_warmup_mock`'s
    /// env-lock choice.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn now_unix() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn is_enabled_defaults_true_and_respects_explicit_false() {
        let _guard = ENV_LOCK.lock().await;
        std::env::remove_var(KEEPALIVE_ENV);
        assert!(is_enabled(), "default = enabled");
        for value in ["false", "FALSE", "  false  ", "fAlSe"] {
            std::env::set_var(KEEPALIVE_ENV, value);
            assert!(!is_enabled(), "MUNI_GROQ_KEEPALIVE={value:?} disables");
        }
        for value in ["true", "1", "", "yes", "garbage"] {
            std::env::set_var(KEEPALIVE_ENV, value);
            assert!(is_enabled(), "only explicit `false` disables");
        }
        std::env::remove_var(KEEPALIVE_ENV);
    }

    #[tokio::test]
    async fn tick_pings_models_endpoint_with_bearer_auth() {
        let _guard = ENV_LOCK.lock().await;
        std::env::set_var(GROQ_ENV_VAR, TEST_API_KEY);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/openai/v1/models"))
            .and(header("Authorization", format!("Bearer {TEST_API_KEY}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(r#"{"object":"list","data":[]}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let http = crate::groq::shared_groq_http().expect("shared client");
        // Fresh activity → secs_since_call() == i64::MAX → not skipped.
        let activity = GroqActivity::new();
        let endpoint = format!("{}/openai/v1/models", server.uri());

        let outcome = keepalive_tick(&http, &activity, &endpoint).await;
        assert_eq!(outcome, KeepaliveOutcome::Pinged(200));

        std::env::remove_var(GROQ_ENV_VAR);
    }

    #[tokio::test]
    async fn tick_skips_when_a_real_call_landed_recently() {
        let _guard = ENV_LOCK.lock().await;
        std::env::set_var(GROQ_ENV_VAR, TEST_API_KEY);

        let server = MockServer::start().await;
        // expect(0): the skip gate must short-circuit before any HTTP.
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let http = crate::groq::shared_groq_http().expect("shared client");
        let activity = GroqActivity::new();
        // A real call 10 s ago → inside the 240 s window → skip.
        activity.seed_last_call_unix(now_unix() - 10);
        let endpoint = format!("{}/openai/v1/models", server.uri());

        let outcome = keepalive_tick(&http, &activity, &endpoint).await;
        assert_eq!(outcome, KeepaliveOutcome::SkippedRecentCall);

        std::env::remove_var(GROQ_ENV_VAR);
    }

    #[tokio::test]
    async fn tick_idles_without_an_api_key() {
        let _guard = ENV_LOCK.lock().await;
        // Unique keychain suffix so a developer's real saved Groq key
        // can't satisfy `secrets::get`; env var unset → no env key.
        std::env::set_var(
            "MUNI_KEYCHAIN_SUFFIX",
            "rust_groq_keepalive_missing_key_test",
        );
        std::env::remove_var(GROQ_ENV_VAR);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let http = crate::groq::shared_groq_http().expect("shared client");
        let activity = GroqActivity::new();
        let endpoint = format!("{}/openai/v1/models", server.uri());

        let outcome = keepalive_tick(&http, &activity, &endpoint).await;
        assert_eq!(outcome, KeepaliveOutcome::NoApiKey);

        std::env::remove_var("MUNI_KEYCHAIN_SUFFIX");
    }

    #[tokio::test]
    async fn tick_skip_gate_is_exclusive_at_the_interval() {
        let _guard = ENV_LOCK.lock().await;
        std::env::set_var(GROQ_ENV_VAR, TEST_API_KEY);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/openai/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            // Exactly one ping: the at-the-interval case must ping, the
            // just-inside-the-window case must skip before any HTTP.
            .expect(1)
            .mount(&server)
            .await;

        let http = crate::groq::shared_groq_http().expect("shared client");
        let endpoint = format!("{}/openai/v1/models", server.uri());
        let interval = KEEPALIVE_INTERVAL.as_secs() as i64;

        // A real call comfortably inside the window (interval-5 s ago) →
        // secs_since_call() < interval → skip. (5 s of slack keeps the
        // assertion clear of the ~1 s clock jitter between seeding and the
        // internal `now`.)
        let inside = GroqActivity::new();
        inside.seed_last_call_unix(now_unix() - (interval - 5));
        assert_eq!(
            keepalive_tick(&http, &inside, &endpoint).await,
            KeepaliveOutcome::SkippedRecentCall,
            "a real call {}s ago (< {}s) must skip",
            interval - 5,
            interval
        );

        // A real call exactly `interval` s ago → secs_since_call() is
        // `interval`, and the gate is `< interval`, so it is NOT skipped
        // and pings. This pins the boundary as EXCLUSIVE: a `<`→`<=`
        // regression would skip here and the `expect(1)` would fail.
        let at_edge = GroqActivity::new();
        at_edge.seed_last_call_unix(now_unix() - interval);
        assert_eq!(
            keepalive_tick(&http, &at_edge, &endpoint).await,
            KeepaliveOutcome::Pinged(200),
            "a real call exactly {interval}s ago must ping (gate is exclusive)"
        );

        std::env::remove_var(GROQ_ENV_VAR);
    }

    #[tokio::test]
    async fn tick_reports_failed_on_transport_error() {
        let _guard = ENV_LOCK.lock().await;
        // Env key resolves first, so the tick passes the key gate and
        // actually attempts the request.
        std::env::set_var(GROQ_ENV_VAR, TEST_API_KEY);

        let http = crate::groq::shared_groq_http().expect("shared client");
        // Fresh activity passes the skip gate so the tick attempts the ping.
        let activity = GroqActivity::new();
        // Port 1 on loopback is unbound → immediate connection refusal, so
        // the request errors fast (well under KEEPALIVE_REQUEST_TIMEOUT).
        let endpoint = "http://127.0.0.1:1/openai/v1/models".to_string();

        let outcome = keepalive_tick(&http, &activity, &endpoint).await;
        assert_eq!(
            outcome,
            KeepaliveOutcome::Failed,
            "a transport error must map to Failed, never Pinged or a panic"
        );

        std::env::remove_var(GROQ_ENV_VAR);
    }

    #[tokio::test]
    async fn tick_never_touches_prefix_state() {
        let _guard = ENV_LOCK.lock().await;
        std::env::set_var(GROQ_ENV_VAR, TEST_API_KEY);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/openai/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let http = crate::groq::shared_groq_http().expect("shared client");
        let activity = GroqActivity::new();
        let endpoint = format!("{}/openai/v1/models", server.uri());

        let _ = keepalive_tick(&http, &activity, &endpoint).await;
        // The keepalive warms TCP, not Groq's prompt cache — the re-warm
        // staleness clock must stay untouched.
        assert_eq!(
            activity.secs_since_prefix_touch(),
            i64::MAX,
            "keepalive must never bump last_prefix_touch"
        );
        // It must also NOT count itself as real traffic for the skip
        // gate (would mask true staleness).
        assert_eq!(
            activity.secs_since_call(),
            i64::MAX,
            "keepalive must never bump last_any_call"
        );

        std::env::remove_var(GROQ_ENV_VAR);
    }
}
