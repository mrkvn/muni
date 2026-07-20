//! Thin HTTP client for the user-hosted scraper API that supplies
//! monthly provider prices (feature 005).
//!
//! Wire contract (frozen at `schema_version = 1`):
//! ```text
//! GET <MUNI_PRICES_URL>
//! Authorization: Bearer <MUNI_PRICES_TOKEN>
//!
//! 200 OK
//! {
//!   "schema_version": 1,
//!   "generated_at": "2026-05-06T12:34:56Z",
//!   "prices": [
//!     { "provider": "deepgram", "model": "nova-3",
//!       "kind": "per_audio_second",
//!       "usd_per_second": 0.00008,
//!       "usd_per_input_token": null, "usd_per_output_token": null,
//!       "source_url": "https://deepgram.com/pricing",
//!       "as_of": "2026-05-04T00:00:00Z" }
//!     /* ... */
//!   ]
//! }
//! ```
//!
//! Failures are non-fatal — caller logs and leaves `price_history`
//! untouched so the bootstrap `DEFAULT_PRICES` remain authoritative.

use std::time::Duration;

use serde::Deserialize;

/// Default URL for the scraper API. Placeholder until the user
/// deploys the muni-prices service; overridable at runtime via the
/// `MUNI_PRICES_URL` env var.
pub const DEFAULT_PRICES_URL: &str = "https://prices.muni.example/v1/prices";

/// Env var that overrides [`DEFAULT_PRICES_URL`] at runtime.
pub const PRICES_URL_ENV_VAR: &str = "MUNI_PRICES_URL";

/// Env var (and compile-time bake key) for the bearer token.
pub const PRICES_TOKEN_ENV_VAR: &str = "MUNI_PRICES_TOKEN";

/// Compile-time injection point for the bearer token. CI/release
/// pipeline sets `MUNI_PRICES_TOKEN` so the token never lives in
/// committed source. `None` in dev builds without the env injection
/// — the refresher then logs each failed attempt and keeps the
/// `DEFAULT_PRICES` bootstrap rows in place.
fn compile_time_token() -> Option<&'static str> {
    option_env!("MUNI_PRICES_TOKEN")
}

/// Resolve the bearer token, preferring a **runtime** `MUNI_PRICES_TOKEN`
/// env override over the compile-time `option_env!` baked value (plan 039
/// task 46). This lets the token be rotated by relaunching the app with a
/// fresh env value — no rebuild — while the baked token stays the fallback
/// for normally-launched `.app` bundles (which inherit no shell env). See
/// `docs/release.md` for the rotation runbook.
fn resolve_token() -> Option<String> {
    resolve_token_from(
        std::env::var(PRICES_TOKEN_ENV_VAR).ok(),
        compile_time_token(),
    )
}

/// Pure precedence rule behind [`resolve_token`], split out so the ordering is
/// unit-testable without mutating process env. A runtime value wins, but an
/// empty runtime value is treated as absent so an accidentally-cleared env var
/// falls back to the baked token instead of sending an empty bearer.
fn resolve_token_from(runtime: Option<String>, compiled: Option<&'static str>) -> Option<String> {
    runtime
        .filter(|s| !s.is_empty())
        .or_else(|| compiled.map(str::to_string))
}

/// Wire schema version this client speaks. Bumped only when the
/// scraper response format changes incompatibly.
const SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// HTTP request timeout. The scraper is small + cached, so a healthy
/// fetch takes a few hundred milliseconds; 10 s is comfortable for
/// flaky residential connections without pinning the refresher
/// thread on a stuck endpoint.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap captured error bodies so a verbose scraper-side error page
/// can't flood the logs.
const MAX_ERROR_BODY_BYTES: usize = 4_096;

/// One price row as it travels across the wire. Field shape mirrors
/// the brainstorm contract; deserialisation is non-strict so a future
/// scraper schema can add forward-compatible fields without a code
/// change here. The refresher converts to [`crate::usage_store::PriceRow`]
/// before upserting.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RawPriceEntry {
    pub provider: String,
    pub model: String,
    /// `"per_audio_second"` or `"per_token"` — see
    /// [`crate::pricing::PriceKind`].
    pub kind: String,
    pub usd_per_second: Option<f64>,
    pub usd_per_input_token: Option<f64>,
    pub usd_per_output_token: Option<f64>,
    pub source_url: Option<String>,
    pub as_of: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PricesResponseV1 {
    schema_version: u32,
    #[allow(dead_code)] // surfaced for diagnostics, not used in flow
    generated_at: Option<String>,
    prices: Vec<RawPriceEntry>,
}

/// Failure modes for [`PricesClient::fetch`]. Each variant is meant to
/// be loggable and dropped — none reach the user.
#[derive(Debug)]
pub enum PricesFetchError {
    /// Token wasn't injected at compile time. Refresher logs and skips.
    MissingToken,
    /// Transport-level failure (DNS, TLS, timeout, etc.).
    Network(String),
    /// 4xx/5xx response from the scraper.
    Http { status: u16, body: String },
    /// JSON didn't deserialise into the expected envelope.
    Parse(String),
    /// Server replied with a `schema_version` we don't speak.
    SchemaMismatch { got: u32 },
}

impl std::fmt::Display for PricesFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingToken => write!(f, "MUNI_PRICES_TOKEN not injected"),
            Self::Network(msg) => write!(f, "network: {msg}"),
            Self::Http { status, body } => write!(f, "http {status}: {body}"),
            Self::Parse(msg) => write!(f, "parse: {msg}"),
            Self::SchemaMismatch { got } => write!(
                f,
                "schema mismatch: got {got}, expected {SUPPORTED_SCHEMA_VERSION}"
            ),
        }
    }
}

/// HTTP client for the scraper API. Cheap to construct; reuses a
/// `reqwest::Client` so TLS sessions persist across hourly fetches.
pub struct PricesClient {
    url: String,
    token: Option<String>,
    http: reqwest::Client,
    timeout: Duration,
}

impl PricesClient {
    /// Resolve the URL from `MUNI_PRICES_URL` (env override) →
    /// [`DEFAULT_PRICES_URL`], resolve the token via [`resolve_token`]
    /// (runtime env override → compile-time bake), and construct the
    /// reqwest client.
    pub fn new() -> Result<Self, PricesFetchError> {
        let url =
            std::env::var(PRICES_URL_ENV_VAR).unwrap_or_else(|_| DEFAULT_PRICES_URL.to_string());
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| PricesFetchError::Network(e.to_string()))?;
        Ok(Self {
            url,
            token: resolve_token(),
            http,
            timeout: FETCH_TIMEOUT,
        })
    }

    /// Test-only constructor that overrides the URL and token. Used
    /// by `tests/prices_refresher_gating.rs` to point at a wiremock
    /// server without depending on env-var ordering across tests.
    pub fn for_testing(url: String, token: Option<&str>) -> Result<Self, PricesFetchError> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| PricesFetchError::Network(e.to_string()))?;
        Ok(Self {
            url,
            token: token.map(str::to_string),
            http,
            timeout: FETCH_TIMEOUT,
        })
    }

    /// Whether a bearer token was resolved at construction (runtime env
    /// override or compile-time bake). `false` means the refresher would
    /// only ever produce [`PricesFetchError::MissingToken`], so the
    /// caller can skip fetching entirely rather than retrying a request
    /// that cannot succeed until the process is relaunched with a token.
    /// The token is resolved once in [`Self::new`] and never mutates, so
    /// this verdict is stable for the client's lifetime.
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// GET the configured URL, validate `schema_version`, and return
    /// the parsed price entries. Errors are non-fatal at the call
    /// site — see module-level docs.
    pub async fn fetch(&self) -> Result<Vec<RawPriceEntry>, PricesFetchError> {
        let token = self
            .token
            .as_deref()
            .ok_or(PricesFetchError::MissingToken)?;

        let response = self
            .http
            .get(&self.url)
            .timeout(self.timeout)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| PricesFetchError::Network(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|e| format!("(failed to read error body: {e})"));
            return Err(PricesFetchError::Http {
                status: status.as_u16(),
                body: truncate_for_log(&body, MAX_ERROR_BODY_BYTES),
            });
        }

        let body = response
            .text()
            .await
            .map_err(|e| PricesFetchError::Network(e.to_string()))?;
        let parsed: PricesResponseV1 =
            serde_json::from_str(&body).map_err(|e| PricesFetchError::Parse(e.to_string()))?;
        if parsed.schema_version != SUPPORTED_SCHEMA_VERSION {
            return Err(PricesFetchError::SchemaMismatch {
                got: parsed.schema_version,
            });
        }
        Ok(parsed.prices)
    }
}

fn truncate_for_log(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TEST_TOKEN: &str = "test-bearer-****ABCD";

    fn fixture_response(version: u32) -> serde_json::Value {
        json!({
            "schema_version": version,
            "generated_at": "2026-05-06T12:34:56Z",
            "prices": [
                {
                    "provider": "deepgram",
                    "model": "nova-3",
                    "kind": "per_audio_second",
                    "usd_per_second": 0.00008,
                    "usd_per_input_token": null,
                    "usd_per_output_token": null,
                    "source_url": "https://deepgram.com/pricing",
                    "as_of": "2026-05-04T00:00:00Z"
                }
            ]
        })
    }

    #[test]
    fn resolve_token_prefers_runtime_over_compiled() {
        // Runtime env value wins over the baked compile-time token so the
        // token can be rotated without a rebuild (plan 039 task 46).
        assert_eq!(
            resolve_token_from(Some("runtime-****WXYZ".to_string()), Some("baked-****ABCD")),
            Some("runtime-****WXYZ".to_string())
        );
    }

    #[test]
    fn resolve_token_falls_back_to_compiled_when_no_runtime() {
        assert_eq!(
            resolve_token_from(None, Some("baked-****ABCD")),
            Some("baked-****ABCD".to_string())
        );
    }

    #[test]
    fn resolve_token_treats_empty_runtime_as_absent() {
        // An accidentally-cleared env var must not shadow the baked token
        // (and must not send an empty bearer).
        assert_eq!(
            resolve_token_from(Some(String::new()), Some("baked-****ABCD")),
            Some("baked-****ABCD".to_string())
        );
    }

    #[test]
    fn resolve_token_is_none_when_neither_present() {
        assert_eq!(resolve_token_from(None, None), None);
    }

    #[tokio::test]
    async fn fetch_with_missing_token_returns_typed_error() {
        let server = MockServer::start().await;
        let client = PricesClient::for_testing(server.uri(), None).unwrap();
        match client.fetch().await {
            Err(PricesFetchError::MissingToken) => {}
            other => panic!("expected MissingToken, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_returns_prices_on_2xx_with_correct_schema() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .and(header("Authorization", format!("Bearer {TEST_TOKEN}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(fixture_response(1)),
            )
            .mount(&server)
            .await;

        let client = PricesClient::for_testing(server.uri(), Some(TEST_TOKEN)).unwrap();
        let prices = client.fetch().await.expect("ok");
        assert_eq!(prices.len(), 1);
        assert_eq!(prices[0].provider, "deepgram");
        assert_eq!(prices[0].model, "nova-3");
        assert_eq!(prices[0].kind, "per_audio_second");
        assert!((prices[0].usd_per_second.unwrap() - 0.00008).abs() < 1e-12);
    }

    #[tokio::test]
    async fn fetch_rejects_unknown_schema_version() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(fixture_response(99)),
            )
            .mount(&server)
            .await;
        let client = PricesClient::for_testing(server.uri(), Some(TEST_TOKEN)).unwrap();
        match client.fetch().await {
            Err(PricesFetchError::SchemaMismatch { got }) => assert_eq!(got, 99),
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_returns_http_error_for_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized: bad token"))
            .mount(&server)
            .await;
        let client = PricesClient::for_testing(server.uri(), Some(TEST_TOKEN)).unwrap();
        match client.fetch().await {
            Err(PricesFetchError::Http { status, body }) => {
                assert_eq!(status, 401);
                assert!(body.contains("unauthorized"));
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_returns_parse_error_on_malformed_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{not json"))
            .mount(&server)
            .await;
        let client = PricesClient::for_testing(server.uri(), Some(TEST_TOKEN)).unwrap();
        match client.fetch().await {
            Err(PricesFetchError::Parse(_)) => {}
            other => panic!("expected Parse, got {other:?}"),
        }
    }
}
