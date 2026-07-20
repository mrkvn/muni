//! TLS + WS pre-warm probe for Gladia Solaria-1 (feature 010
//! follow-up research).
//!
//! Marked `#[ignore]` because it hits the real Gladia endpoint and
//! consumes whatever billing window Gladia attaches to a POST that
//! opens a session + a held-open WebSocket. **Always check the Gladia
//! dashboard "audio-hours" before and after running.** That difference
//! is the answer.
//!
//! Run manually:
//!
//! ```sh
//! cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml \
//!   --no-default-features --test gladia_tls_probe \
//!   -- --ignored --nocapture
//!
//! # Required env: a real Gladia key in MUNI_GLADIA_KEY (or .env).
//! ```
//!
//! ## Question
//!
//! Per `docs/research/08-gladia-evaluation.md` §3.2, Gladia's support
//! article says billing covers audio streamed through the WebSocket —
//! including spoken audio, silence, background noise, and **empty
//! audio frames**. What the docs do **not** clarify is whether the
//! WebSocket upgrade itself starts the meter. Three plausible answers:
//!
//! - **A.** Bills at WebSocket upgrade (any open session, even without
//!   audio, accrues billed time).
//! - **B.** Bills only when the first audio frame is sent.
//! - **C.** Bills only on streamed audio bytes (POST + WS open are
//!   free; the meter starts on first binary frame and runs only while
//!   audio is being delivered).
//!
//! Production code today opens the WS as soon as `POST /v2/live`
//! returns and parks an idle slot in `GladiaPool` between presses, so
//! the bake-off cannot distinguish A from B/C without this probe.
//! Cost: ~$0.0002 per minute of probe — trivially cheap.
//!
//! ## Interpreting the result
//!
//! Compare the Gladia dashboard's "audio-hours" (or per-session
//! billed-time) immediately before vs. immediately after the probe
//! finishes:
//!
//! - audio-hours went **up by ~the probe's hold duration** → answer A.
//!   Gladia bills from WS upgrade. The warm-pool architecture itself
//!   becomes expensive — every parked idle slot meters, and the §6.2
//!   cutover thesis collapses.
//! - audio-hours **did not move** (or moved much less than the hold)
//!   → answer B or C. We can park a session at the WS layer for free
//!   and only pay for the actual audio of a press. The warm-pool
//!   pattern is safe.
//!
//! ## Edge case: Gladia idle-closes our session
//!
//! Per `docs/research/08-gladia-evaluation.md` §2.3, Gladia auto-closes
//! a WebSocket after ~30 s of inactivity (close code 4408) and after
//! ~1 minute without transcribed text (close code 4504). The probe's
//! `tokio::select!` notices the close and reports it; the
//! wall-clock connection time is what the dashboard delta should
//! reflect. If the close fires at ~30 s and the dashboard went up by
//! ~30 s, that's still answer A; if the dashboard stayed flat, that's
//! still answer B/C.

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::json;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const POST_ENDPOINT: &str = "https://api.gladia.io/v2/live";
const HOLD_DURATION_SECS: u64 = 60;
const API_KEY_ENV: &str = "MUNI_GLADIA_KEY";

#[tokio::test]
#[ignore = "hits real Gladia endpoint; run manually + watch dashboard"]
async fn gladia_unauthenticated_ws_hold() {
    let api_key = match std::env::var(API_KEY_ENV) {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!();
            eprintln!(
                "SKIP: {API_KEY_ENV} not set. Export your Gladia key (e.g. \
                 `source .env`) and re-run."
            );
            return;
        }
    };

    let hold = Duration::from_secs(HOLD_DURATION_SECS);

    eprintln!();
    eprintln!("=== Gladia POST + TLS+WS pre-warm probe ===");
    eprintln!("POST endpoint:        {POST_ENDPOINT}");
    eprintln!(
        "Plan:                 POST /v2/live, open the returned WS, send NO audio, hold for {HOLD_DURATION_SECS}s, close."
    );
    eprintln!();
    eprintln!(">>> ACTION: open the Gladia dashboard NOW and note the current");
    eprintln!(">>>         'audio-hours' (or per-session billed-time) value.");
    eprintln!(">>>         The probe will run for up to {HOLD_DURATION_SECS}s.");
    eprintln!(">>>         Refresh the dashboard immediately after the probe");
    eprintln!(">>>         prints '=== Probe finished ==='.");
    eprintln!();

    // Phase 1: POST /v2/live to mint a session URL.
    let body = json!({
        "encoding": "wav/pcm",
        "sample_rate": 16000,
        "bit_depth": 16,
        "channels": 1,
        "language_config": { "languages": ["en", "tl"], "code_switching": true },
        "messages_config": {
            "receive_partial_transcripts": false,
            "receive_final_transcripts": true
        }
    });

    let post_start = Instant::now();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build http client");
    let resp = http
        .post(POST_ENDPOINT)
        .header("x-gladia-key", &api_key)
        .json(&body)
        .send()
        .await
        .expect("POST /v2/live");
    let post_dur = post_start.elapsed();
    let status = resp.status();
    let parsed: serde_json::Value = resp.json().await.expect("decode init json");
    let session_id = parsed
        .get("id")
        .or_else(|| parsed.get("session_id"))
        .and_then(|v| v.as_str())
        .expect("init response carries id/session_id");
    let url = parsed
        .get("url")
        .or_else(|| parsed.get("websocket_url"))
        .and_then(|v| v.as_str())
        .expect("init response carries url/websocket_url")
        .to_string();
    eprintln!(
        "✓ POST /v2/live → {status} in {:.0} ms (session_id={session_id})",
        post_dur.as_millis()
    );

    // Phase 2: open the WS.
    let request = url.into_client_request().expect("build ws request");
    let connect_start = Instant::now();
    let (ws, response) = connect_async(request).await.expect("ws handshake");
    let handshake_dur = connect_start.elapsed();
    eprintln!(
        "✓ WS connected; handshake took {:.0} ms, server responded with status {}",
        handshake_dur.as_millis(),
        response.status()
    );
    eprintln!("  Holding the socket open. NO audio frames will be sent.");

    let (_sink, mut stream) = ws.split();
    let outcome: &str = tokio::select! {
        _ = tokio::time::sleep(hold) => "timer_expired_naturally",
        msg = stream.next() => match msg {
            None => "server_closed_socket_cleanly",
            Some(Ok(frame)) => {
                eprintln!("  Server sent unexpected frame: {frame:?}");
                "server_sent_frame"
            }
            Some(Err(err)) => {
                eprintln!("  Stream error: {err}");
                "stream_error"
            }
        },
    };

    let total_held = connect_start.elapsed();

    eprintln!();
    eprintln!("=== Probe finished ===");
    eprintln!("Hold outcome:                 {outcome}");
    eprintln!(
        "POST round-trip:              {:.0} ms",
        post_dur.as_millis()
    );
    eprintln!(
        "WS handshake:                 {:.0} ms",
        handshake_dur.as_millis()
    );
    eprintln!(
        "Total wall-clock connection:  {:.2}s ({:.4}h)",
        total_held.as_secs_f64(),
        total_held.as_secs_f64() / 3600.0
    );
    eprintln!();
    eprintln!(">>> ACTION: refresh the Gladia dashboard now.");
    eprintln!(">>>");
    eprintln!(
        ">>> If audio-hours went up by ~{:.2}s (~{:.4}h):",
        total_held.as_secs_f64(),
        total_held.as_secs_f64() / 3600.0
    );
    eprintln!(">>>   → Answer A: bills at WS upgrade. Warm-pool slots cost money.");
    eprintln!(">>>     The §6.2 cutover thesis collapses; revisit before merging.");
    eprintln!(">>>");
    eprintln!(">>> If audio-hours did NOT move (or moved much less than the hold):");
    eprintln!(">>>   → Answer B/C: bills only on streamed audio. The warm-pool");
    eprintln!(">>>     architecture is safe; per-press cost is bounded by audio");
    eprintln!(">>>     duration alone. Proceed to bake-off rubric.");
}
