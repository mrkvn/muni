//! Integration test for `deepgram::DeepgramClient`.
//!
//! Drives the real WebSocket client against a hand-rolled
//! `tokio-tungstenite` server bound to a localhost ephemeral port. The mock
//! exercises the three load-bearing branches of the client's receive loop:
//!
//! 1. `Results` accumulation + `Metadata` finalization (the happy path).
//! 2. `Error` server message → typed `DeepgramServerError`.
//! 3. Mid-stream disconnect → `DeepgramConnectionFailed`.
//!
//! The mock is intentionally minimal — it doesn't validate that the audio
//! frames carry plausible PCM, only that they arrive as binary frames before
//! `CloseStream` and that the client correctly sequences the protocol.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use muni_lib::deepgram::{ChunkConfidence, DeepgramClient, FinalizeOutcome};
use muni_lib::error::MuniError;

/// Spawn the mock server on a random local port and return the `ws://...`
/// URL the client should connect to.
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

/// Wait for at least one binary audio frame, then stream a sequence of
/// `Results` messages and finalize with `Metadata` once `CloseStream`
/// arrives. Mirrors what the production Deepgram endpoint does.
async fn happy_path_handler(stream: TcpStream) {
    let mut ws = accept_async(stream).await.expect("ws handshake");

    // Read until CloseStream, optionally observing binary audio frames.
    while let Some(msg) = ws.next().await {
        match msg.expect("ws read") {
            Message::Binary(_) => {
                // First binary frame triggers the canned interim+final stream.
                ws.send(Message::Text(json_results(false, "hello")))
                    .await
                    .expect("send interim");
                ws.send(Message::Text(json_results(true, "hello world")))
                    .await
                    .expect("send final 1");
                ws.send(Message::Text(json_results(true, "this is a test")))
                    .await
                    .expect("send final 2");
            }
            Message::Text(t) if t.contains("CloseStream") => {
                ws.send(Message::Text(
                    r#"{"type":"Metadata","request_id":"abc"}"#.to_string(),
                ))
                .await
                .expect("send metadata");
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
            Message::Text(t) if t.contains("CloseStream") => {
                ws.send(Message::Text(
                    r#"{"type":"Error","description":"Quota exceeded"}"#.to_string(),
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

/// Reject the WebSocket upgrade with a raw HTTP 401, mimicking a Deepgram
/// endpoint that refuses a deactivated/revoked API key. The client should
/// surface this as the actionable `DeepgramKeyRejected` kind rather than a
/// generic `DeepgramConnectionFailed`. Reads the request bytes first so the
/// client's write side doesn't error before it sees the 401 response.
async fn unauthorized_handshake_handler(mut stream: TcpStream) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Drain the upgrade request (best-effort; we only need to have read it
    // before replying so the TCP peer doesn't see a premature RST).
    let mut buf = [0_u8; 1024];
    let _ = stream.read(&mut buf).await;

    let response = "HTTP/1.1 401 Unauthorized\r\n\
        content-length: 0\r\n\
        connection: close\r\n\r\n";
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

async fn drop_mid_session_handler(stream: TcpStream) {
    let mut ws = accept_async(stream).await.expect("ws handshake");
    // Read first frame then drop connection abruptly without Metadata.
    if let Some(Ok(_first)) = ws.next().await {
        // Drop ws → TCP RST.
    }
}

/// Mimics a half-open socket: the handshake completes, frames are
/// accepted, but `CloseStream` is never acknowledged with `Metadata`.
/// Mirrors the QA repro where WiFi drops mid-press — the server is
/// gone but tungstenite's read side hasn't noticed yet, so the
/// receive task waits indefinitely on a frame that never arrives.
async fn never_replies_to_close_stream_handler(stream: TcpStream) {
    let mut ws = accept_async(stream).await.expect("ws handshake");
    // Drain incoming frames forever; never send anything back. The
    // forwarder will send audio + CloseStream into the void.
    while let Some(Ok(_)) = ws.next().await {}
}

/// Streams a couple of `is_final` Results frames on the first audio
/// frame (so the client accumulates a partial transcript), then drains
/// incoming frames forever WITHOUT ever sending `Metadata`. Mirrors the
/// 2026-06-15 incident: Deepgram transcribed the whole press but never
/// acked `CloseStream`, so `finalize()` must recover the accumulated
/// partial on the timeout path rather than discard the press.
async fn partial_results_then_never_acks_handler(stream: TcpStream) {
    let mut ws = accept_async(stream).await.expect("ws handshake");
    while let Some(msg) = ws.next().await {
        match msg.expect("ws read") {
            Message::Binary(_) => {
                // Send the finals immediately on the first audio frame so
                // they're accumulated well before CloseStream arrives.
                ws.send(Message::Text(json_results(true, "hello world")))
                    .await
                    .expect("send final 1");
                ws.send(Message::Text(json_results(true, "this is a test")))
                    .await
                    .expect("send final 2");
            }
            // Deliberately never reply to CloseStream — drain forever.
            _ => continue,
        }
    }
}

fn json_results(is_final: bool, transcript: &str) -> String {
    format!(
        r#"{{"type":"Results","is_final":{is_final},"channel":{{"alternatives":[{{"transcript":"{transcript}","confidence":0.99}}]}}}}"#
    )
}

/// Emit a `Results` message with a caller-supplied confidence value and
/// word-count so feature-019 trigger tests can exercise both the
/// counter-incrementing and counter-resetting paths.
fn json_results_with_confidence(
    is_final: bool,
    transcript: &str,
    confidence: f32,
    words: usize,
) -> String {
    let mut words_arr = String::from("[");
    for i in 0..words {
        if i > 0 {
            words_arr.push(',');
        }
        words_arr.push_str(&format!(
            r#"{{"word":"w{i}","start":0.0,"end":0.1,"confidence":{confidence}}}"#
        ));
    }
    words_arr.push(']');
    format!(
        r#"{{"type":"Results","is_final":{is_final},"channel":{{"alternatives":[{{"transcript":"{transcript}","confidence":{confidence},"words":{words_arr}}}]}}}}"#
    )
}

/// Handler that mimics a press that starts in English (a couple of
/// high-confidence finals so pass#1/pass#2 commit Deepgram) and then
/// degrades into a run of low-confidence finals (so the feature-019
/// confidence trigger arms and fires). Stays open until the client
/// sends `CloseStream`.
#[allow(dead_code)]
pub async fn low_confidence_after_pass2_handler(stream: TcpStream) {
    let mut ws = accept_async(stream).await.expect("ws handshake");
    let mut audio_seen = false;
    while let Some(msg) = ws.next().await {
        match msg.expect("ws read") {
            Message::Binary(_) if !audio_seen => {
                audio_seen = true;
                // Two high-confidence finals to land pass#1 and pass#2
                // as English.
                ws.send(Message::Text(json_results_with_confidence(
                    true, "hello", 0.95, 1,
                )))
                .await
                .expect("send high-conf 1");
                ws.send(Message::Text(json_results_with_confidence(
                    true,
                    "hello world",
                    0.95,
                    2,
                )))
                .await
                .expect("send high-conf 2");
                // Then a run of low-confidence finals — enough to clear
                // the default consecutive threshold (3) with headroom.
                for i in 0..5 {
                    let line = format!("low_conf_{i}");
                    ws.send(Message::Text(json_results_with_confidence(
                        true, &line, 0.40, 1,
                    )))
                    .await
                    .expect("send low-conf");
                    // A small gap so the receive side can interleave
                    // with the session-layer trigger scheduling.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            Message::Binary(_) => {}
            Message::Text(t) if t.contains("CloseStream") => {
                ws.send(Message::Text(
                    r#"{"type":"Metadata","request_id":"trigger-mock"}"#.to_string(),
                ))
                .await
                .expect("send metadata");
                break;
            }
            _ => continue,
        }
    }
    let _ = ws.close(None).await;
}

#[tokio::test(flavor = "current_thread")]
async fn final_results_concatenated_and_returned_on_metadata() {
    let url = start_mock_server(happy_path_handler).await;

    let client = DeepgramClient::open_at("test-token", &url)
        .await
        .expect("client opens");

    // One audio frame is enough to trigger the canned response.
    client.send(&[0_i16, 1, -1, 256]).await.expect("send chunk");

    // Allow the server to flush its Results before we finalize. Without this
    // the test races: CloseStream may be processed before all interim Results
    // have been queued, but the contract is "is_final fragments are returned
    // in order"; we test on the basis that the server flushes before reading
    // CloseStream — which the happy_path_handler does in order.
    let transcript = timeout(Duration::from_secs(5), client.finalize())
        .await
        .expect("finalize within timeout")
        .expect("finalize ok");

    assert_eq!(
        transcript,
        FinalizeOutcome::Complete("hello world this is a test".to_string())
    );
    client.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn server_error_message_surfaces_as_typed_error() {
    let url = start_mock_server(server_error_handler).await;

    let client = DeepgramClient::open_at("test-token", &url)
        .await
        .expect("client opens");
    client.send(&[0_i16; 8]).await.expect("send chunk");

    let result = timeout(Duration::from_secs(5), client.finalize())
        .await
        .expect("finalize within timeout");

    match result {
        Err(MuniError::DeepgramServerError { message }) => {
            assert_eq!(message, "Quota exceeded");
        }
        other => panic!("expected DeepgramServerError, got {other:?}"),
    }
    client.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn mid_session_disconnect_reports_connection_failed_with_no_transcript() {
    let url = start_mock_server(drop_mid_session_handler).await;

    let client = DeepgramClient::open_at("test-token", &url)
        .await
        .expect("client opens");
    client.send(&[0_i16; 8]).await.expect("send chunk");

    let result = timeout(Duration::from_secs(5), client.finalize())
        .await
        .expect("finalize within timeout");

    match result {
        Err(MuniError::DeepgramConnectionFailed { .. }) => {}
        Err(MuniError::DeepgramInvalidResponse) => {}
        other => panic!("expected DeepgramConnectionFailed/InvalidResponse, got {other:?}"),
    }
    client.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn handshake_401_surfaces_as_key_rejected() {
    // Feature 035 regression: a deactivated key fails the upgrade with an
    // HTTP 401. That must map to the actionable `DeepgramKeyRejected` kind
    // (silent notification → API Keys), NOT a generic connection failure.
    let url = start_mock_server(unauthorized_handshake_handler).await;

    let result = DeepgramClient::open_at("revoked-token", &url).await;

    match result {
        Err(MuniError::DeepgramKeyRejected) => {}
        Err(other) => panic!("expected DeepgramKeyRejected, got {other:?}"),
        Ok(_) => panic!("expected DeepgramKeyRejected, got a connected client"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn finalize_times_out_on_half_open_socket() {
    // QA-driven regression: with a half-open WebSocket (server gone
    // mid-press but tungstenite hasn't noticed), the original
    // `finalize` waited indefinitely for `Metadata`, blocking the
    // orchestrator's release path for ~6 s before the receive task
    // finally observed the disconnect. The new
    // `FINALIZE_TIMEOUT = 3 s` cap surfaces a typed
    // `DeepgramConnectionFailed` so the orchestrator can move on
    // and the broadcast event queue doesn't accumulate.
    let url = start_mock_server(never_replies_to_close_stream_handler).await;

    let mut client = DeepgramClient::open_at("test-token", &url)
        .await
        .expect("client opens");
    // Inject a short timeout so the empty-accumulation timeout path
    // surfaces in tens of ms instead of the production 3 s.
    client.set_finalize_timeout(Duration::from_millis(50));
    client.send(&[0_i16; 8]).await.expect("send chunk");

    let started = std::time::Instant::now();
    // Outer cap >> inner cap so the assertion proves the inner
    // finalize_timeout fired (not the outer guard).
    let result = timeout(Duration::from_secs(6), client.finalize())
        .await
        .expect("outer guard didn't fire — inner timeout regressed?");
    let elapsed = started.elapsed();

    match result {
        Err(MuniError::DeepgramConnectionFailed { reason }) => {
            assert!(
                reason.contains("timed out"),
                "expected timeout reason, got {reason}",
            );
        }
        other => panic!("expected DeepgramConnectionFailed (timeout), got {other:?}"),
    }
    // Must fail well under 1 s with the injected 50 ms timeout.
    assert!(
        elapsed < Duration::from_secs(1),
        "finalize timeout should fire near the injected 50 ms, took {elapsed:?}",
    );
    client.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn finalize_timeout_recovers_accumulated_partial() {
    // The 2026-06-15 incident: Deepgram streamed `is_final` Results for
    // the whole press but never acked `CloseStream`, so finalize timed
    // out and the transcript was discarded. The fix recovers the
    // accumulated partial on the timeout path instead of losing it.
    let url = start_mock_server(partial_results_then_never_acks_handler).await;

    let mut client = DeepgramClient::open_at("test-token", &url)
        .await
        .expect("client opens");
    client.set_finalize_timeout(Duration::from_millis(50));

    // The first audio frame triggers the canned finals. Give the
    // receive loop a beat to accumulate them before CloseStream lands.
    client.send(&[0_i16, 1, -1, 256]).await.expect("send chunk");

    let started = std::time::Instant::now();
    let result = timeout(Duration::from_secs(2), client.finalize())
        .await
        .expect("outer guard didn't fire — inner timeout regressed?");
    let elapsed = started.elapsed();

    let outcome = result.expect("timeout should recover the accumulated partial, not error");
    assert_eq!(
        outcome,
        FinalizeOutcome::Partial("hello world this is a test".to_string()),
    );
    // Proves the recovery rides the injected 50 ms timeout, not a hang.
    assert!(
        elapsed < Duration::from_secs(1),
        "partial recovery should fire near the injected timeout, took {elapsed:?}",
    );
    client.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receive_loop_forwards_per_chunk_confidence_to_installed_continuation() {
    // Feature 019: per-chunk confidence parsed out of `is_final=true`
    // Results and surfaced on the installed mpsc channel.
    let url = start_mock_server(low_confidence_after_pass2_handler).await;

    let client = DeepgramClient::open_at("test-token", &url)
        .await
        .expect("client opens");

    let (tx, mut rx) = tokio::sync::mpsc::channel::<ChunkConfidence>(64);
    client.install_confidence_continuation(tx).await;

    // Kick the mock with at least one audio frame so it streams the
    // canned high-then-low sequence.
    client.send(&[0_i16; 8]).await.expect("send chunk");

    // Drain events for up to 2 s. We expect at least two high-conf
    // events (for pass#1/pass#2) followed by several low-conf events.
    let mut high = 0;
    let mut low = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            Ok(Some(event)) => {
                assert!(event.is_final, "only is_final=true should forward");
                if event.confidence >= 0.7 {
                    high += 1;
                } else {
                    low += 1;
                }
                if high >= 2 && low >= 3 {
                    break;
                }
            }
            _ => break,
        }
    }
    client.close().await;

    assert!(high >= 2, "expected ≥2 high-confidence events, got {high}");
    assert!(low >= 3, "expected ≥3 low-confidence events, got {low}");
}

#[tokio::test(flavor = "current_thread")]
async fn open_with_empty_key_short_circuits_without_network() {
    // No mock server needed — the empty-key check happens client-side.
    match DeepgramClient::open("").await {
        Err(MuniError::DeepgramMissingApiKey) => {}
        Err(other) => panic!("expected DeepgramMissingApiKey, got {other:?}"),
        Ok(_) => panic!("expected error, got Ok"),
    }
}
