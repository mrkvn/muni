//! Integration test for `session::DeepgramPool` warm-WS pre-warming.
//!
//! Asserts the load-bearing invariant of plan §002 Task 17:
//! after `take()` returns, the pool has already scheduled the next warmer
//! so the next press finds a ready socket. Without this, every press pays
//! the 200–500 ms TLS + WS handshake cost and the head of the user's
//! utterance is dropped to the broadcast channel buffer (the "talk-too-soon"
//! gap reported in Phase 5 manual QA).
//!
//! The test drives a real `DeepgramPool` against a hand-rolled
//! tokio-tungstenite mock that accepts handshakes and immediately closes —
//! enough to satisfy `DeepgramClient::open_at()` and let the warmer park
//! a client. The pool's internal `warmer_count` then makes the swap
//! observable from the test without exposing additional internals.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use muni_lib::session::{fixed_deepgram_key, DeepgramPool};

/// Spawn a long-lived mock that accepts an unbounded number of WS handshakes.
/// Each accepted connection is immediately gracefully closed — the warmer
/// only needs the handshake to succeed; it parks the client and never sends
/// audio in this test.
async fn start_warming_mock() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock listener");
    let addr = listener.local_addr().expect("local addr");
    let url = format!("ws://{addr}/v1/listen");

    tokio::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                break;
            };
            tokio::spawn(handle_connection(stream));
        }
    });

    url
}

async fn handle_connection(stream: TcpStream) {
    let Ok(mut ws) = accept_async(stream).await else {
        return;
    };
    // Hold the connection open briefly so the client's send() / close()
    // doesn't race the server's hangup. The pool drops the parked client
    // when the test ends; we don't need any traffic on this socket.
    let _ = ws.close(None).await;
}

/// Poll `condition()` until it returns `true` or the deadline elapses.
async fn wait_until<F>(timeout: Duration, mut condition: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    condition()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn take_schedules_next_warmer_immediately() {
    let url = start_warming_mock().await;
    let pool = DeepgramPool::spawn_with_endpoint(fixed_deepgram_key("test-token"), url);

    // 1. Wait for the initial warmer to park its client.
    assert!(
        wait_until(Duration::from_secs(5), || pool.warmer_count() >= 1).await,
        "initial warmer never parked a client (count={})",
        pool.warmer_count()
    );

    let after_initial = pool.warmer_count();
    assert!(after_initial >= 1);

    // 2. Take the parked client. This must immediately schedule the next
    //    warmer — the press path doesn't await the warmer; it overlaps it
    //    with the rest of the press's audio path.
    let _client = pool.take().await.expect("take returns parked client");

    // 3. Wait for the second warmer to park. Strict greater-than because
    //    the same warmer-inflight slot now flips: the count had better
    //    advance past the snapshot we just took.
    assert!(
        wait_until(Duration::from_secs(5), || {
            pool.warmer_count() > after_initial
        })
        .await,
        "next warmer didn't run after take() (count={}, expected > {})",
        pool.warmer_count(),
        after_initial
    );
}

/// Mock that holds the connection open and counts every `KeepAlive` text
/// frame it receives. Used by `parked_socket_receives_keepalive_pings`.
async fn start_keepalive_counting_mock(counter: Arc<AtomicUsize>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind keepalive mock");
    let addr = listener.local_addr().expect("local addr");
    let url = format!("ws://{addr}/v1/listen");

    tokio::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                break;
            };
            let counter = counter.clone();
            tokio::spawn(async move {
                let Ok(mut ws) = accept_async(stream).await else {
                    return;
                };
                while let Some(msg) = ws.next().await {
                    let Ok(msg) = msg else {
                        break;
                    };
                    match msg {
                        Message::Text(t) if t.contains("KeepAlive") => {
                            counter.fetch_add(1, Ordering::SeqCst);
                        }
                        Message::Close(_) => break,
                        _ => continue,
                    }
                }
            });
        }
    });

    url
}

/// Regression test for the bug where the parked WS got served as a corpse
/// after Deepgram closed it for being idle. Asserts that the parked socket
/// stays alive indefinitely because the keepalive task is sending pings
/// faster than the idle timeout would close the socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parked_socket_receives_keepalive_pings() {
    let keepalives = Arc::new(AtomicUsize::new(0));
    let url = start_keepalive_counting_mock(keepalives.clone()).await;

    // 200 ms cadence keeps the test under a second while exercising the
    // exact same code path the 5 s production cadence uses.
    let pool = DeepgramPool::spawn_with_endpoint_and_keepalive(
        fixed_deepgram_key("test-token"),
        url,
        Duration::from_millis(200),
    );

    // Wait for the warmer to park.
    assert!(
        wait_until(Duration::from_secs(5), || pool.warmer_count() >= 1).await,
        "initial warmer never parked"
    );

    // Sleep for ~3 keepalive intervals.
    tokio::time::sleep(Duration::from_millis(700)).await;

    let pings = keepalives.load(Ordering::SeqCst);
    assert!(
        pings >= 2,
        "expected the parked socket to receive >=2 KeepAlive pings, got {pings}"
    );

    // The slot must STILL be parked — the keepalive shouldn't have torn it
    // down. A successful take here returns the same warm socket without
    // bumping warmer_count past 1 (until the next warmer kicks in).
    let _client = pool.take().await.expect("parked socket still alive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn back_to_back_takes_each_schedule_a_warmer() {
    let url = start_warming_mock().await;
    let pool = DeepgramPool::spawn_with_endpoint(fixed_deepgram_key("test-token"), url);

    // Initial warmer parks #1.
    assert!(
        wait_until(Duration::from_secs(5), || pool.warmer_count() >= 1).await,
        "initial warmer never parked"
    );

    // Take #1, wait for #2 to park.
    let _c1 = pool.take().await.expect("first take");
    assert!(
        wait_until(Duration::from_secs(5), || pool.warmer_count() >= 2).await,
        "second warmer didn't run (count={})",
        pool.warmer_count()
    );

    // Take #2, wait for #3.
    let _c2 = pool.take().await.expect("second take");
    assert!(
        wait_until(Duration::from_secs(5), || pool.warmer_count() >= 3).await,
        "third warmer didn't run (count={})",
        pool.warmer_count()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_parked_drops_socket_and_reschedules_warmer() {
    // QA-driven regression: if `clear_parked` didn't kill the existing
    // entry, "Remove saved key" + immediate press would stream through
    // the previously-authenticated parked WS — exactly what users
    // reported with the empty-key test in §3 of the QA doc.
    //
    // Test invariant: after `clear_parked`, a fresh warmer must run
    // (warmer_count strictly increases) and the parked entry must be
    // a *new* one (the previous client handle is dropped). We assert
    // the count delta as the observable proxy.
    let url = start_warming_mock().await;
    let pool = DeepgramPool::spawn_with_endpoint(fixed_deepgram_key("test-token"), url);

    // Initial warmer parks #1.
    assert!(
        wait_until(Duration::from_secs(5), || pool.warmer_count() >= 1).await,
        "initial warmer never parked"
    );
    let before = pool.warmer_count();

    pool.clear_parked().await;

    assert!(
        wait_until(Duration::from_secs(5), || pool.warmer_count() > before).await,
        "clear_parked must reschedule a fresh warmer (count stuck at {})",
        pool.warmer_count()
    );
}
