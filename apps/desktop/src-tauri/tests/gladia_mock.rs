//! Integration tests for `gladia::GladiaClient` (feature 010).
//!
//! Mirrors `tests/deepgram_mock.rs` and `tests/soniox_mock.rs` — drives
//! the real client end-to-end against a hand-rolled mock pair:
//!
//! - A `wiremock` HTTP server handles `POST /v2/live`, returning a
//!   canned `{id, url}` whose `url` points at...
//! - A `tokio-tungstenite` server bound to a localhost ephemeral port
//!   which plays the canned per-test conversation (transcript frames,
//!   error frames, mid-session disconnect, half-open stall).
//!
//! Covers the load-bearing branches of the press cycle:
//!
//! 1. Happy-path final transcript + audio duration → typed value
//!    surfaces on `last_metadata_duration()`.
//! 2. Server `error` envelope → typed `GladiaServerError`.
//! 3. Mid-stream disconnect → `GladiaConnectionFailed`.
//! 4. Finalize timeout on a half-open socket (3 s FINALIZE_TIMEOUT
//!    enforced; the receive task would otherwise wait forever).
//! 5. `POST /v2/live` returning 401 → `GladiaKeyRejected` (actionable
//!    rejected-key kind); the client never reaches the WS phase.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use muni_lib::error::MuniError;
use muni_lib::gladia::GladiaClient;

const TEST_API_KEY: &str = "gld-test-****ABCD";

/// Spawn a tokio-tungstenite mock WS server on an ephemeral local
/// port and return the `ws://...` URL. The handler is invoked once
/// — these are one-shot fixtures, mirroring the Soniox mock.
async fn start_mock_ws<F, Fut>(handler: F) -> String
where
    F: FnOnce(TcpStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock listener");
    let addr = listener.local_addr().expect("local addr");
    let url = format!("ws://{addr}/v2/live/session");

    tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.expect("accept");
        handler(stream).await;
    });

    url
}

/// Wire the `POST /v2/live` mock so it returns `{ id, url }` where
/// `url` is the ws URL of the local mock WS handler. Returns the POST
/// endpoint the client should be pointed at.
async fn start_mock_post(server: &MockServer, ws_url: &str, session_id: &str) {
    Mock::given(method("POST"))
        .and(path("/v2/live"))
        .and(header("x-gladia-key", TEST_API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": session_id,
            "url": ws_url,
        })))
        .mount(server)
        .await;
}

fn post_endpoint(server: &MockServer) -> String {
    format!("{}/v2/live", server.uri())
}

/// Happy-path handler: read the binary audio frame, send a final
/// transcript envelope, then a `post_final_transcript` carrying the
/// audio duration when the client sends the `stop_recording` control
/// frame.
async fn happy_path_handler(stream: TcpStream) {
    let mut ws = accept_async(stream).await.expect("ws handshake");
    while let Some(msg) = ws.next().await {
        match msg.expect("ws read") {
            Message::Binary(b) if !b.is_empty() => {
                // First non-empty audio frame triggers the final
                // transcript. The `is_final: true` flag is what the
                // accumulator gates on.
                ws.send(Message::Text(
                    json!({
                        "type": "transcript",
                        "data": {
                            "is_final": true,
                            "utterance": { "text": "hello world", "language": "en" },
                            "audio_duration": 1.5
                        }
                    })
                    .to_string(),
                ))
                .await
                .expect("send transcript");
            }
            Message::Text(t) if t.contains("stop_recording") => {
                ws.send(Message::Text(
                    json!({
                        "type": "post_final_transcript",
                        "data": { "audio_duration": 1.5 }
                    })
                    .to_string(),
                ))
                .await
                .expect("send post_final_transcript");
                break;
            }
            _ => continue,
        }
    }
    let _ = ws.close(None).await;
}

async fn server_error_handler(stream: TcpStream) {
    let mut ws = accept_async(stream).await.expect("ws handshake");
    while let Some(msg) = ws.next().await {
        match msg.expect("ws read") {
            Message::Text(t) if t.contains("stop_recording") => {
                ws.send(Message::Text(
                    json!({ "type": "error", "message": "rate limited" }).to_string(),
                ))
                .await
                .expect("send error");
                break;
            }
            _ => continue,
        }
    }
    let _ = ws.close(None).await;
}

async fn drop_mid_session_handler(stream: TcpStream) {
    let mut ws = accept_async(stream).await.expect("ws handshake");
    if let Some(Ok(_first)) = ws.next().await {
        // Drop the WS — TCP RST.
    }
}

/// Half-open socket: drain everything but never reply to the EOF.
async fn never_replies_to_finalize_handler(stream: TcpStream) {
    let mut ws = accept_async(stream).await.expect("ws handshake");
    while let Some(Ok(_)) = ws.next().await {}
}

/// Backlog 0019 fixture — replays the production failure shape:
/// the WS ACKs the first audio frame with an `audio_chunk` envelope
/// (so the client knows audio was flowing), then stops sending
/// anything for the rest of the connection's life. Stop-recording
/// arrives, the client's finalize timer expires after 3 s, and the
/// new classification logic should pick `GladiaFinalizeTimeoutAfterAudio`
/// (Loud) instead of the previous `GladiaConnectionFailed` (Quiet).
async fn acks_audio_then_silence_handler(stream: TcpStream) {
    let mut ws = accept_async(stream).await.expect("ws handshake");
    let mut sent_ack = false;
    while let Some(msg) = ws.next().await {
        match msg.expect("ws read") {
            Message::Binary(b) if !b.is_empty() && !sent_ack => {
                ws.send(Message::Text(
                    json!({
                        "type": "audio_chunk",
                        "data": {
                            "byte_range": [0, b.len()],
                            "time_range": [0.0, 0.95]
                        }
                    })
                    .to_string(),
                ))
                .await
                .expect("send audio_chunk ack");
                sent_ack = true;
            }
            // Keep reading so the WS stays open, but never reply
            // to `stop_recording` — emulating the half-open
            // server-side stall the production bug exhibits.
            _ => continue,
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn final_transcript_returned_on_finalize() {
    let post_server = MockServer::start().await;
    let ws_url = start_mock_ws(happy_path_handler).await;
    start_mock_post(&post_server, &ws_url, "test-session-happy").await;

    let client = GladiaClient::open_at(TEST_API_KEY, &post_endpoint(&post_server))
        .await
        .expect("client opens");

    assert_eq!(client.session_id(), "test-session-happy");

    client.send(&[0_i16, 1, -1, 256]).await.expect("send chunk");

    let transcript = timeout(Duration::from_secs(5), client.finalize())
        .await
        .expect("finalize within timeout")
        .expect("finalize ok");

    assert_eq!(transcript, "hello world");
    // The transcript-frame's audio_duration cached for cost tracking.
    assert_eq!(client.last_metadata_duration().await, Some(1.5));
    client.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn server_error_message_surfaces_as_typed_error() {
    let post_server = MockServer::start().await;
    let ws_url = start_mock_ws(server_error_handler).await;
    start_mock_post(&post_server, &ws_url, "test-session-error").await;

    let client = GladiaClient::open_at(TEST_API_KEY, &post_endpoint(&post_server))
        .await
        .expect("client opens");
    client.send(&[0_i16; 8]).await.expect("send chunk");

    let result = timeout(Duration::from_secs(5), client.finalize())
        .await
        .expect("finalize within timeout");

    match result {
        Err(MuniError::GladiaServerError { message }) => {
            assert_eq!(message, "rate limited");
        }
        other => panic!("expected GladiaServerError, got {other:?}"),
    }
    client.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn mid_session_disconnect_reports_connection_failed_with_no_transcript() {
    let post_server = MockServer::start().await;
    let ws_url = start_mock_ws(drop_mid_session_handler).await;
    start_mock_post(&post_server, &ws_url, "test-session-drop").await;

    let client = GladiaClient::open_at(TEST_API_KEY, &post_endpoint(&post_server))
        .await
        .expect("client opens");
    client.send(&[0_i16; 8]).await.expect("send chunk");

    let result = timeout(Duration::from_secs(5), client.finalize())
        .await
        .expect("finalize within timeout");

    match result {
        Err(MuniError::GladiaConnectionFailed { .. }) => {}
        Err(MuniError::GladiaInvalidResponse) => {}
        other => panic!("expected GladiaConnectionFailed/InvalidResponse, got {other:?}"),
    }
    client.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn finalize_times_out_on_half_open_socket() {
    // Regression: with a half-open WS, the receive task would wait
    // forever on a transcript frame that never arrives. The 3 s
    // FINALIZE_TIMEOUT surfaces a typed GladiaConnectionFailed.
    let post_server = MockServer::start().await;
    let ws_url = start_mock_ws(never_replies_to_finalize_handler).await;
    start_mock_post(&post_server, &ws_url, "test-session-stall").await;

    let client = GladiaClient::open_at(TEST_API_KEY, &post_endpoint(&post_server))
        .await
        .expect("client opens");
    client.send(&[0_i16; 8]).await.expect("send chunk");

    let started = std::time::Instant::now();
    let result = timeout(Duration::from_secs(6), client.finalize())
        .await
        .expect("outer guard didn't fire — inner timeout regressed?");
    let elapsed = started.elapsed();

    match result {
        Err(MuniError::GladiaConnectionFailed { reason }) => {
            assert!(
                reason.contains("timed out"),
                "expected timeout reason, got {reason}"
            );
        }
        other => panic!("expected GladiaConnectionFailed (timeout), got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_millis(4500),
        "finalize timeout should fire near 3 s, took {elapsed:?}"
    );
    client.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn finalize_timeout_after_audio_ack_surfaces_loud_variant() {
    // Backlog 0019 regression: when Gladia ACKs initial audio and
    // then stops sending anything (production-observed transient
    // outage), finalize() must classify the resulting timeout as
    // `GladiaFinalizeTimeoutAfterAudio` so the error_presenter
    // routes it Loud (system notification) instead of Quiet (HUD
    // pill, silent empty paste for the user). Pre-fix this same
    // wire shape returned `GladiaConnectionFailed`.
    let post_server = MockServer::start().await;
    let ws_url = start_mock_ws(acks_audio_then_silence_handler).await;
    start_mock_post(&post_server, &ws_url, "test-session-acks-then-silence").await;

    let client = GladiaClient::open_at(TEST_API_KEY, &post_endpoint(&post_server))
        .await
        .expect("client opens");
    client.send(&[0_i16; 8]).await.expect("send chunk");

    // The receive task processes frames asynchronously. If
    // finalize() races ahead and the audio_chunk envelope has not
    // yet been applied to the shared State, the timeout branch
    // would still see `received_audio_chunk_ack = false` and
    // regress to the old (Quiet) classification. A short sleep
    // gives the localhost round-trip time to settle.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let result = timeout(Duration::from_secs(6), client.finalize())
        .await
        .expect("outer guard didn't fire — inner finalize timeout regressed?");

    assert!(
        matches!(result, Err(MuniError::GladiaFinalizeTimeoutAfterAudio)),
        "expected GladiaFinalizeTimeoutAfterAudio, got {result:?}"
    );
    client.close().await;
}

#[tokio::test]
async fn post_unauthorised_surfaces_as_key_rejected() {
    // Feature 035: a 401 on the init POST means the `x-gladia-key` was
    // rejected (expired/revoked). It now surfaces as the actionable
    // `GladiaKeyRejected` kind (silent notification → API Keys) rather than
    // a generic `GladiaServerError` HUD chip.
    let post_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/live"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid key"))
        .mount(&post_server)
        .await;

    let result = GladiaClient::open_at(TEST_API_KEY, &post_endpoint(&post_server)).await;
    match result {
        Err(MuniError::GladiaKeyRejected) => {}
        Err(other) => panic!("expected GladiaKeyRejected, got {other:?}"),
        Ok(_) => panic!("expected GladiaKeyRejected, got Ok"),
    }
}

#[tokio::test]
async fn open_with_empty_key_short_circuits_without_network() {
    match GladiaClient::open("").await {
        Err(MuniError::GladiaMissingApiKey) => {}
        Err(other) => panic!("expected GladiaMissingApiKey, got {other:?}"),
        Ok(_) => panic!("expected error, got Ok"),
    }
}
