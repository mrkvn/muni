//! Failure-shape coverage for the text-LID clients' `classify_with_key`
//! (plan 039 slice 6, task 22).
//!
//! Text-LID drives routing only and self-falls-back on failure, so these
//! typed errors must never surface as a panic or a mislabelled kind. Each
//! provider (`GroqLidClient`, `GeminiLidClient`) is exercised against a
//! `wiremock` server for the two upstream failure shapes the plan calls out:
//!
//! - non-2xx status  → provider `*ServerError { status, body }`
//! - 2xx malformed JSON → provider `*InvalidResponse`
//!
//! The test passing at all is itself the "no panic" assertion — a panic in
//! `classify_with_key` would fail the `#[tokio::test]` rather than reach the
//! `match`.

use muni_lib::error::MuniError;
use muni_lib::gemini_lid::GeminiLidClient;
use muni_lib::groq_lid::GroqLidClient;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

// A non-empty placeholder key: `classify_with_key` short-circuits to the
// `*MissingApiKey` variant on an empty key, so the request never reaches the
// mock. A shaped-but-fake value exercises the real request path.
const TEST_KEY: &str = "lid-test-****ABCD";

// ---- Groq LID client -----------------------------------------------------

#[tokio::test]
async fn groq_non_2xx_status_maps_to_server_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream overloaded"))
        .mount(&server)
        .await;

    let client = GroqLidClient::with_endpoint(server.uri(), "openai/gpt-oss-120b".into())
        .expect("client builds");
    match client.classify_with_key("hello", TEST_KEY).await {
        Err(MuniError::GroqServerError { status, body }) => {
            assert_eq!(status, 503);
            assert!(
                body.contains("upstream overloaded"),
                "error body captured, got {body:?}"
            );
        }
        other => panic!("expected GroqServerError, got {other:?}"),
    }
}

#[tokio::test]
async fn groq_malformed_json_maps_to_invalid_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        // 2xx but not the expected chat-completions shape.
        .respond_with(ResponseTemplate::new(200).set_body_string("{not valid json"))
        .mount(&server)
        .await;

    let client = GroqLidClient::with_endpoint(server.uri(), "openai/gpt-oss-120b".into())
        .expect("client builds");
    match client.classify_with_key("hello", TEST_KEY).await {
        Err(MuniError::GroqInvalidResponse) => {}
        other => panic!("expected GroqInvalidResponse, got {other:?}"),
    }
}

// ---- Gemini LID client ---------------------------------------------------

#[tokio::test]
async fn gemini_non_2xx_status_maps_to_server_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string("quota exceeded"))
        .mount(&server)
        .await;

    // `with_endpoint` treats the first arg as the endpoint *base*; the client
    // appends `/{model}:generateContent`. The mock matches any POST path.
    let client = GeminiLidClient::with_endpoint(server.uri(), "gemini-2.5-flash-lite".into())
        .expect("client builds");
    match client.classify_with_key("hello", TEST_KEY).await {
        Err(MuniError::GeminiServerError { status, body }) => {
            assert_eq!(status, 429);
            assert!(
                body.contains("quota exceeded"),
                "error body captured, got {body:?}"
            );
        }
        other => panic!("expected GeminiServerError, got {other:?}"),
    }
}

#[tokio::test]
async fn gemini_malformed_json_maps_to_invalid_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>not json</html>"))
        .mount(&server)
        .await;

    let client = GeminiLidClient::with_endpoint(server.uri(), "gemini-2.5-flash-lite".into())
        .expect("client builds");
    match client.classify_with_key("hello", TEST_KEY).await {
        Err(MuniError::GeminiInvalidResponse) => {}
        other => panic!("expected GeminiInvalidResponse, got {other:?}"),
    }
}
