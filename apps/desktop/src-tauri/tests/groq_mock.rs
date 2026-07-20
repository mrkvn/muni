//! Integration test for `groq::GroqClient`.
//!
//! Drives the real `reqwest`-backed client against a `wiremock` HTTP server
//! bound to a localhost ephemeral port. Exercises the load-bearing branches
//! of the SSE aggregator:
//!
//! 1. Happy path — multiple `data: {delta}` lines + `data: [DONE]` →
//!    concatenated trimmed cleaned text.
//! 2. Non-streaming `data: {"choices":[{"message":{...}}]}` reply →
//!    accepted via the same code path.
//! 3. HTTP 5xx → typed `GroqServerError` with status + body; HTTP 401/403 →
//!    actionable `GroqKeyRejected` (rejected-key kind).
//! 4. Empty stream → empty string (caller's job to fall back to raw).
//! 5. The Authorization header is forwarded as `Bearer <KEY>`.
//!
//! The mock returns the entire SSE body in one shot rather than streaming it
//! line-by-line; the client's parser already handles both single- and
//! multi-line chunks, so the assertion is identical either way.

use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use muni_lib::error::MuniError;
use muni_lib::groq::{GroqClient, GroqMessage, GroqRequest};

const TEST_API_KEY: &str = "sk-test-****ABCD";

/// Pin the cleanup model used by every test in this file so the body
/// matchers don't drift the day someone bumps `groq::DEFAULT_MODEL`,
/// and so the host's `MUNI_CLEANUP_MODEL` (read by
/// `GroqClient::with_endpoint`) can't leak into the test.
const TEST_MODEL: &str = "llama-3.3-70b-versatile";

fn build_request() -> GroqRequest {
    GroqRequest::cleanup(
        vec![
            GroqMessage::system("Be terse.".into()),
            GroqMessage::user("hello world".into()),
        ],
        TEST_MODEL,
    )
}

async fn client_for(server: &MockServer) -> GroqClient {
    GroqClient::with_endpoint_and_model(
        format!("{}/v1/chat/completions", server.uri()),
        TEST_MODEL.to_string(),
    )
    .expect("build client")
}

fn sse_body(deltas: &[&str], with_done: bool) -> String {
    let mut body = String::new();
    for delta in deltas {
        let payload = json!({
            "choices": [{ "delta": { "content": delta } }]
        });
        body.push_str("data: ");
        body.push_str(&payload.to_string());
        body.push('\n');
    }
    if with_done {
        body.push_str("data: [DONE]\n");
    }
    body
}

#[tokio::test]
async fn aggregates_streaming_deltas_and_trims_result() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("Authorization", format!("Bearer {TEST_API_KEY}")))
        .and(header("Accept", "text/event-stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body(
                    &[" Hello", ", world", "."],
                    /* with_done */ true,
                )),
        )
        .mount(&server)
        .await;

    let client = client_for(&server).await;
    let result = client.complete(build_request(), TEST_API_KEY).await;
    let (text, usage) = result.unwrap();
    assert_eq!(text, "Hello, world.");
    assert!(usage.is_none(), "no usage chunk emitted in this fixture");
}

#[tokio::test]
async fn accepts_non_streaming_message_shape() {
    // Some Groq endpoints fall back to a non-streaming envelope when the
    // server can't cooperate with `stream: true`. The Swift client read
    // both shapes; the Rust client must too.
    let server = MockServer::start().await;
    let body = format!(
        "data: {}\ndata: [DONE]\n",
        json!({
            "choices": [{ "message": { "role": "assistant", "content": "Cleaned text." } }]
        })
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let client = client_for(&server).await;
    let result = client.complete(build_request(), TEST_API_KEY).await;
    let (text, _usage) = result.unwrap();
    assert_eq!(text, "Cleaned text.");
}

#[tokio::test]
async fn server_error_status_is_typed_with_status_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream exploded"))
        .mount(&server)
        .await;

    let client = client_for(&server).await;
    match client.complete(build_request(), TEST_API_KEY).await {
        Err(MuniError::GroqServerError { status, body }) => {
            assert_eq!(status, 500);
            assert!(body.contains("upstream exploded"));
        }
        other => panic!("expected GroqServerError, got {other:?}"),
    }
}

#[tokio::test]
async fn unauthorised_status_surfaces_as_key_rejected() {
    // Feature 035: a 401/403 from Groq means the key is bad (expired/revoked),
    // not a transient server hiccup. It surfaces as the actionable
    // `GroqKeyRejected` kind (silent notification → API Keys), distinct from
    // a 5xx `GroqServerError`. Cleanup still has a raw-text fallback, so the
    // paste is not blocked.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized: bad key"))
        .mount(&server)
        .await;

    let client = client_for(&server).await;
    match client.complete(build_request(), TEST_API_KEY).await {
        Err(MuniError::GroqKeyRejected) => {}
        other => panic!("expected GroqKeyRejected, got {other:?}"),
    }
}

#[tokio::test]
async fn empty_stream_returns_empty_string() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: [DONE]\n"),
        )
        .mount(&server)
        .await;

    let client = client_for(&server).await;
    let result = client.complete(build_request(), TEST_API_KEY).await;
    let (text, _usage) = result.unwrap();
    assert_eq!(text, "");
}

#[tokio::test]
async fn bad_lines_are_skipped_not_fatal() {
    // A heartbeat comment, an unknown event, and one line with malformed
    // JSON should all be silently skipped while the legitimate deltas still
    // aggregate. Mirrors Swift v1's per-line `try?` parse loop.
    let server = MockServer::start().await;
    let body = format!(
        ":heartbeat\nevent: ping\ndata: not-json\n{}{}data: [DONE]\n",
        format_args!(
            "data: {}\n",
            json!({ "choices": [{ "delta": { "content": "Good " } }] })
        ),
        format_args!(
            "data: {}\n",
            json!({ "choices": [{ "delta": { "content": "morning." } }] })
        ),
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let client = client_for(&server).await;
    let result = client.complete(build_request(), TEST_API_KEY).await;
    let (text, _usage) = result.unwrap();
    assert_eq!(text, "Good morning.");
}

#[tokio::test]
async fn request_body_carries_v1_defaults_and_messages() {
    use wiremock::matchers::body_partial_json;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({
            "model": "llama-3.3-70b-versatile",
            "stream": true,
            "temperature": 0.2,
            "messages": [
                { "role": "system", "content": "Be terse." },
                { "role": "user", "content": "hello world" },
            ]
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: [DONE]\n"),
        )
        .mount(&server)
        .await;

    let client = client_for(&server).await;
    client
        .complete(build_request(), TEST_API_KEY)
        .await
        .expect("body matchers cover the contract");
}

#[tokio::test]
async fn usage_chunk_after_deltas_is_captured() {
    // Feature 005 — the cleanup helper sends
    // `stream_options.include_usage = true`. The server replies with a
    // final `{"choices":[],"usage":{...}}` chunk before `[DONE]`. The
    // client must surface that block alongside the aggregated text so
    // cost tracking can record token counts.
    let server = MockServer::start().await;
    let mut body = sse_body(&["Cleaned ", "text"], /* with_done */ false);
    body.push_str(
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":40,\"total_tokens\":160}}\n",
    );
    body.push_str("data: [DONE]\n");
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let client = client_for(&server).await;
    let (text, usage) = client
        .complete(build_request(), TEST_API_KEY)
        .await
        .expect("ok");
    assert_eq!(text, "Cleaned text");
    let usage = usage.expect("usage chunk captured");
    assert_eq!(usage.prompt_tokens, 120);
    assert_eq!(usage.completion_tokens, 40);
}

#[tokio::test]
async fn cleanup_request_body_includes_stream_options() {
    use wiremock::matchers::body_partial_json;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({
            "stream": true,
            "stream_options": { "include_usage": true }
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: [DONE]\n"),
        )
        .mount(&server)
        .await;

    let client = client_for(&server).await;
    client
        .complete(build_request(), TEST_API_KEY)
        .await
        .expect("body matcher confirms stream_options.include_usage");
}
