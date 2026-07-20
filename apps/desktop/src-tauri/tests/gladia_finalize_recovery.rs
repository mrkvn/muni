//! Integration test for Gladia finalize partial-recovery (plan 039 slice 3,
//! task 7a). Complements `tests/gladia_mock.rs` (untouched behavioural lock).
//!
//! Shape: the server streams an `is_final` transcript frame (the client
//! accumulates it), then when `stop_recording` arrives it STALLS — never
//! sending the terminal `post_final_transcript`. Pre-fix, `finalize()` timed
//! out and discarded the accumulated text. Now it recovers it as a
//! `FinalizeOutcome::Partial`, so the rescue path pastes the salvaged
//! transcript instead of losing the press.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use muni_lib::asr_stream::FinalizeOutcome;
use muni_lib::gladia::GladiaClient;

const TEST_API_KEY: &str = "gld-test-****ABCD";

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

async fn start_mock_post(server: &MockServer, ws_url: &str, session_id: &str) {
    Mock::given(method("POST"))
        .and(path("/v2/live"))
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

/// Sends one final transcript on the first audio frame, then stalls: it keeps
/// the socket open and reads frames forever but never replies to
/// `stop_recording` with the terminal envelope.
async fn final_then_stall_handler(stream: TcpStream) {
    let mut ws = accept_async(stream).await.expect("ws handshake");
    let mut sent_final = false;
    while let Some(msg) = ws.next().await {
        match msg.expect("ws read") {
            Message::Binary(b) if !b.is_empty() && !sent_final => {
                ws.send(Message::Text(
                    json!({
                        "type": "transcript",
                        "data": {
                            "is_final": true,
                            "utterance": { "text": "salvaged thought", "language": "en" },
                            "audio_duration": 1.0
                        }
                    })
                    .to_string(),
                ))
                .await
                .expect("send final transcript");
                sent_final = true;
            }
            // Deliberately never reply to stop_recording — stall.
            _ => continue,
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn finalize_stall_recovers_accumulated_transcript() {
    let post_server = MockServer::start().await;
    let ws_url = start_mock_ws(final_then_stall_handler).await;
    start_mock_post(&post_server, &ws_url, "test-session-stall-recovery").await;

    let client = GladiaClient::open_at(TEST_API_KEY, &post_endpoint(&post_server))
        .await
        .expect("client opens");
    client.send(&[0_i16, 1, -1, 256]).await.expect("send chunk");
    // Let the receive loop accumulate the final before we finalize.
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Pin a short finalize budget so the stall→recovery path fires fast.
    client.set_finalize_timeout(Duration::from_millis(100));

    let outcome = timeout(Duration::from_secs(2), client.finalize())
        .await
        .expect("outer guard didn't fire")
        .expect("stall recovers the accumulated transcript instead of erroring");
    assert_eq!(
        outcome,
        FinalizeOutcome::Partial("salvaged thought".to_string())
    );
    assert!(outcome.is_partial());
    client.close().await;
}
