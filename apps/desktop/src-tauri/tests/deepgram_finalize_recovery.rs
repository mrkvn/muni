//! Integration tests for the Deepgram finalize-recovery hardening
//! (plan 039 slice 3, task 6).
//!
//! Complements `tests/deepgram_mock.rs` (which stays untouched as a
//! behavioural lock) with the two failure shapes the finalize-ordering +
//! Metadata-guard fixes target:
//!
//! 1. `CloseStream` send fails / never acks after `is_final` Results already
//!    accumulated → `finalize()` recovers them as `Partial` rather than
//!    discarding the press (task 6a: no `?`-propagation on send failure, and
//!    `finalize_tx` is always cleared).
//! 2. A server-initiated `Metadata` frame arrives mid-press (before the user
//!    releases) then the socket drops → the accumulated finals SURVIVE to the
//!    eventual `finalize()` (task 6b: Metadata only drains the transcript when
//!    a finalize is actually waiting).

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use muni_lib::deepgram::{DeepgramClient, FinalizeOutcome};

async fn start_mock_server<F, Fut>(handler: F) -> String
where
    F: FnOnce(TcpStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock listener");
    let addr = listener.local_addr().expect("local addr");
    let url = format!("ws://{addr}/v1/listen");
    tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.expect("accept");
        handler(stream).await;
    });
    url
}

fn json_results(is_final: bool, transcript: &str) -> String {
    format!(
        r#"{{"type":"Results","is_final":{is_final},"channel":{{"alternatives":[{{"transcript":"{transcript}","confidence":0.99}}]}}}}"#
    )
}

/// Streams two `is_final` Results on the first audio frame, then drops the
/// connection WITHOUT ever acking `CloseStream`. The client accumulates the
/// finals; when it later sends `CloseStream` into the dead socket the send
/// fails (or the ack never lands and the short finalize timeout fires) — either
/// way the accumulated partial must be recovered, never discarded.
async fn finals_then_drop_handler(stream: TcpStream) {
    let mut ws = accept_async(stream).await.expect("ws handshake");
    while let Some(msg) = ws.next().await {
        match msg.expect("ws read") {
            Message::Binary(_) => {
                ws.send(Message::Text(json_results(true, "hello world")))
                    .await
                    .expect("send final 1");
                ws.send(Message::Text(json_results(true, "this is a test")))
                    .await
                    .expect("send final 2");
                // Drop the connection now — never wait for CloseStream.
                break;
            }
            _ => continue,
        }
    }
    // ws dropped here → socket torn down.
}

/// Streams two `is_final` Results, then a server-initiated `Metadata` frame
/// mid-press (no finalize is waiting yet), then drops. Task 6b: the Metadata
/// must NOT drain the accumulated finals — they have to survive to the
/// eventual `finalize()`.
async fn finals_then_unsolicited_metadata_then_drop_handler(stream: TcpStream) {
    let mut ws = accept_async(stream).await.expect("ws handshake");
    while let Some(msg) = ws.next().await {
        match msg.expect("ws read") {
            Message::Binary(_) => {
                ws.send(Message::Text(json_results(true, "hello world")))
                    .await
                    .expect("send final 1");
                ws.send(Message::Text(json_results(true, "this is a test")))
                    .await
                    .expect("send final 2");
                // Unsolicited Metadata mid-press (with a duration field).
                ws.send(Message::Text(
                    r#"{"type":"Metadata","duration":2.0,"request_id":"mid"}"#.to_string(),
                ))
                .await
                .expect("send mid-press metadata");
                break;
            }
            _ => continue,
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn close_stream_send_failure_recovers_accumulated_partial() {
    let url = start_mock_server(finals_then_drop_handler).await;

    let mut client = DeepgramClient::open_at("test-token", &url)
        .await
        .expect("client opens");
    client.set_finalize_timeout(Duration::from_millis(100));

    client.send(&[0_i16, 1, -1, 256]).await.expect("send chunk");
    // Let the receive loop accumulate the finals and observe the drop before
    // we finalize into the dead socket.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let outcome = timeout(Duration::from_secs(2), client.finalize())
        .await
        .expect("outer guard didn't fire")
        .expect("finalize recovers the partial instead of erroring");
    assert_eq!(
        outcome,
        FinalizeOutcome::Partial("hello world this is a test".to_string())
    );
    client.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn mid_press_metadata_does_not_discard_finals() {
    let url = start_mock_server(finals_then_unsolicited_metadata_then_drop_handler).await;

    let mut client = DeepgramClient::open_at("test-token", &url)
        .await
        .expect("client opens");
    client.set_finalize_timeout(Duration::from_millis(100));

    client.send(&[0_i16, 1, -1, 256]).await.expect("send chunk");
    // Give the receive loop time to process finals + the unsolicited Metadata
    // + the disconnect before we finalize.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let outcome = timeout(Duration::from_secs(2), client.finalize())
        .await
        .expect("outer guard didn't fire")
        .expect("finals survive the mid-press Metadata and are recovered");
    assert_eq!(
        outcome,
        FinalizeOutcome::Partial("hello world this is a test".to_string())
    );
    // The mid-press Metadata's duration was still cached even though it didn't
    // drain the transcript.
    assert_eq!(client.last_metadata_duration().await, Some(2.0));
    client.close().await;
}
