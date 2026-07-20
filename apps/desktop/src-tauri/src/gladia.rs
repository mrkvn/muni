//! Gladia Solaria-1 real-time streaming ASR client.
//!
//! Feature 010 — opt-in alternative to the Deepgram + Groq Whisper +
//! hybrid LID pipeline (see `.claude/feature_plans/010` and
//! `docs/research/08-gladia-evaluation.md`). Same single-shot
//! push-to-talk shape as `deepgram.rs` and the dormant `soniox.rs`: one
//! client = one PTT hold. After `finalize()` returns, the connection is
//! dead; open a new client for the next press.
//!
//! Wire protocol (per `docs/research/08-gladia-evaluation.md` §2 and
//! https://docs.gladia.io/api-reference/v2/live/init):
//!
//! - **Two-phase open**:
//!   1. `POST https://api.gladia.io/v2/live` with `x-gladia-key: <KEY>`
//!      header and the JSON config body (encoding, sample_rate,
//!      language_config, code_switching, custom_vocabulary,
//!      endpointing, messages_config). The response carries
//!      `{ id, url }` — `url` is the per-session `wss://...` endpoint.
//!   2. `connect_async(url)` opens the WebSocket. No further auth header
//!      is needed on the WS — the session-specific URL is itself the
//!      bearer.
//!
//! - **Audio**: `Message::Binary` frames of little-endian i16 PCM at
//!   the rate / channels declared in the config (16 kHz mono here).
//!
//! - **Finalize**: a `Message::Text("{\"type\":\"stop_recording\"}")`
//!   control frame signals end-of-audio. The server flushes pending
//!   transcripts and replies with the final `transcript` envelope
//!   carrying `data.is_final = true` and the session's audio duration.
//!
//! - **Keepalive**: deliberately none. Per
//!   `docs/research/08-gladia-evaluation.md` §3.2, empty audio frames
//!   bill toward the audio-time meter, so we never push silence to
//!   keep an idle WS alive. The warm pool tolerates Gladia's documented
//!   idle close (code 4408 at ~30 s, code 4504 at ~1 min without
//!   transcribed text) and re-warms on every take.
//!
//! - **Server envelopes**: JSON text frames tagged by a top-level
//!   `"type"`. The variants this module consumes are `transcript` (with
//!   `data.is_final`, `data.utterance.text`, and optional audio-duration
//!   metadata) and `error` (with `message`). Lifecycle frames
//!   (`speech_event`, `audio_chunk_acknowledged`, `start_session`,
//!   `end_session`, …) are ignored.
//!
//! Concurrency: the WebSocket is `split()` so `send`/`finalize` (writers)
//! and the spawned receive task (reader) can interleave without re-entry.
//! Receive-task ↔ `finalize()` synchronization uses a `oneshot` channel
//! installed by `finalize()` and consumed when the final transcript
//! frame, an `error` frame, or a transport error arrives.
//!
//! Intentionally mirrors `deepgram.rs` so the orchestrator code path
//! (forwarder + pool + finalize handler) reads the same on both sides.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::{oneshot, Mutex as TokioMutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::asr_stream::{self, FinalizeOutcome, WsRead, WsSink};
use crate::error::MuniError;

/// Maximum time the `POST /v2/live` round-trip is allowed to take.
/// Live POSTs complete in 80–200 ms (research §4.2); 5 s leaves
/// headroom for a flaky link but prevents a half-open TLS connection
/// from pinning the warmer or a press.
const POST_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum time `finalize()` will wait for Gladia's terminal frame.
/// Final-transcript latency is ~700 ms P50 on a clean network; capping
/// at 3 s mirrors `deepgram.rs::FINALIZE_TIMEOUT` and protects against
/// a half-open socket leaving the receive task waiting forever.
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(3);

/// Maximum time the WebSocket handshake is allowed to take. Same
/// rationale as `deepgram.rs::OPEN_TIMEOUT` — tungstenite has no
/// native timeouts and a half-open network would otherwise hang the
/// warmer thread on the ~120 s OS TCP timeout.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum time a single audio frame send is allowed to take. Same
/// rationale as `deepgram.rs::SEND_TIMEOUT` — a half-open kernel TCP
/// buffer pins the forwarder until the OS gives up the connection.
const SEND_TIMEOUT: Duration = Duration::from_secs(1);

/// Maximum time `close()` will wait while sending the WebSocket close
/// frame. Best-effort teardown — same shape as `deepgram.rs`.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(500);

/// Default `POST /v2/live` endpoint. Exposed so integration tests can
/// pin a deterministic URL pointing at a local hand-rolled HTTP mock.
pub const DEFAULT_POST_ENDPOINT: &str = "https://api.gladia.io/v2/live";

/// Dev-only override for the `/v2/live` POST endpoint, consulted by
/// [`GladiaClient::open`]. Used by session-level tests that mock the
/// full Gladia request flow (POST + WS) without standing up
/// [`open_at`](GladiaClient::open_at)-aware production wiring. Mirrors
/// the analogous `MUNI_GROQ_WHISPER_ENDPOINT` knob; unset in
/// production.
pub const POST_ENDPOINT_OVERRIDE_ENV: &str = "MUNI_GLADIA_POST_ENDPOINT";

/// The Solaria-1 model id used for pricing rows and usage records. The
/// model is not user-selectable on Gladia today — `/v2/live` exposes
/// Solaria-1 as the only option — but pinning the id explicitly keeps
/// the per-call usage row aligned with the price-history schema.
pub const MODEL: &str = "solaria-1";

/// Finalize control frame — flushes the server's transcription buffer
/// and triggers the terminal `transcript` reply.
const STOP_RECORDING_PAYLOAD: &str = r#"{"type":"stop_recording"}"#;

/// Rescue-mode finalize budget parameters (plan 039 slice 3, task 7b).
///
/// The primary Gladia route streams audio in real time and keeps the tight
/// 3 s `FINALIZE_TIMEOUT` to protect release latency. The cross-provider
/// rescue path (`session::attempt_gladia_fallback_transcribe`) instead replays
/// a whole press buffer at once, so Gladia needs proportionally longer to
/// flush its terminal frame — a 30 s press dumped in one burst was observed
/// to blow the 3 s cap and lose the rescue. `mark_rescue_replay` scales the
/// finalize budget with the replayed audio: base + per-second, capped.
const RESCUE_FINALIZE_BASE: Duration = Duration::from_secs(3);
const RESCUE_FINALIZE_PER_SECOND_MS: u64 = 250;
const RESCUE_FINALIZE_CAP: Duration = Duration::from_secs(15);

/// Compute the rescue-mode finalize budget for `audio_seconds` of replayed
/// audio: `3s + 250ms * audio_seconds`, capped at 15 s. Pure for testing.
fn rescue_finalize_budget(audio_seconds: f64) -> Duration {
    let extra_ms = (audio_seconds.max(0.0) * RESCUE_FINALIZE_PER_SECOND_MS as f64) as u64;
    (RESCUE_FINALIZE_BASE + Duration::from_millis(extra_ms)).min(RESCUE_FINALIZE_CAP)
}

/// Shared HTTP client for the `POST /v2/live` init call. Building a fresh
/// `reqwest::Client` per `open()` discarded the TCP/TLS connection pool, so
/// every press paid a fresh handshake. A process-wide client keeps the pool
/// warm across presses. The whole-request `POST_TIMEOUT` is baked in (it is a
/// constant) so callers no longer set it per request.
static POST_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(POST_TIMEOUT)
        .build()
        // A builder failure here is a TLS-backend / resolver init problem, not
        // a per-request error. There is no meaningful fallback: the only
        // alternative constructor, `reqwest::Client::new()`, unwraps the same
        // builder internally and would panic on the identical failure class
        // (and would silently drop POST_TIMEOUT if it somehow succeeded), so
        // we fail loudly and honestly here instead.
        .expect("reqwest client init (TLS backend / resolver)")
});

/// Build a [`MuniError::GladiaConnectionFailed`] from a reason string — the
/// `map_err` hook handed to [`asr_stream::send_frame_timed`].
fn gladia_connection_failed(reason: String) -> MuniError {
    MuniError::GladiaConnectionFailed { reason }
}

/// Mutable state shared between the public API and the receive task.
///
/// `final_transcript` accumulates the text of every `transcript` frame
/// whose `data.is_final` is true (partial frames are dropped under our
/// `receive_partial_transcripts: false` config but we still defensively
/// gate on the flag). `finalize_tx` is `Some` only between a caller
/// invoking `finalize()` and the receive task observing the
/// corresponding final frame (or a transport error / disconnect).
/// `last_metadata_duration` captures the audio-duration metadata
/// (seconds) the server reports on the final frame; the cost path
/// reads it to bill exact audio seconds against the Gladia line.
struct State {
    final_transcript: String,
    finalize_tx: Option<oneshot::Sender<Result<FinalizeOutcome, MuniError>>>,
    last_metadata_duration: Option<f64>,
    /// Set the first time the server emits an `audio_chunk` envelope
    /// (its ack for one of our binary frames). The flag is read by
    /// `finalize()`'s timeout branch to classify the failure:
    /// `true` means "WS opened, audio was flowing, then went silent
    /// mid-press" (backlog 0019 — surfaced Loud as
    /// `GladiaFinalizeTimeoutAfterAudio` so the user knows the press
    /// was lost), `false` means "WS opened but produced no traffic"
    /// (kept as the existing Quiet `GladiaConnectionFailed`).
    received_audio_chunk_ack: bool,
}

/// Push-to-talk Gladia WebSocket client.
///
/// Construct via [`GladiaClient::open`]. Lifetime: one client = one PTT
/// hold. After [`finalize`](Self::finalize) returns, the connection is
/// dead; open a new client for the next press.
pub struct GladiaClient {
    sink: Arc<TokioMutex<Option<WsSink>>>,
    state: Arc<TokioMutex<State>>,
    /// Held in a sync mutex so [`Drop`] can `.abort()` the receive task
    /// without an async context.
    receive_handle: StdMutex<Option<JoinHandle<()>>>,
    /// Session id returned by `POST /v2/live`. Logged on open and close
    /// so a misbehaving press can be cross-referenced against the
    /// Gladia dashboard.
    session_id: String,
    /// How long [`finalize`](Self::finalize) waits for the terminal frame
    /// before recovering the accumulated partial (millis). Defaults to
    /// [`FINALIZE_TIMEOUT`]; the rescue path widens it via
    /// [`mark_rescue_replay`](Self::mark_rescue_replay), and tests pin it
    /// short via [`set_finalize_timeout`](Self::set_finalize_timeout). Atomic
    /// so it stays `&self`-mutable alongside the split-sink concurrency model.
    finalize_timeout_ms: AtomicU64,
}

impl GladiaClient {
    /// Connect to Gladia and start the receive task.
    ///
    /// Two-phase: POST the JSON config to `/v2/live`, then open a WS to
    /// the per-session URL the server hands back.
    ///
    /// Returns [`MuniError::GladiaMissingApiKey`] if `api_key` is empty,
    /// [`MuniError::GladiaConnectionFailed`] for any handshake or
    /// transport failure, [`MuniError::GladiaServerError`] when the POST
    /// returns a non-2xx status, and [`MuniError::GladiaInvalidResponse`]
    /// when the response body cannot be parsed as `{id, url}`.
    pub async fn open(api_key: &str) -> Result<Self, MuniError> {
        let endpoint = std::env::var(POST_ENDPOINT_OVERRIDE_ENV)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_POST_ENDPOINT.to_string());
        Self::open_at(api_key, &endpoint).await
    }

    /// Variant of [`open`](Self::open) that accepts a custom POST endpoint
    /// URL. Used by `tests/gladia_mock.rs` to point the client at a
    /// local hand-rolled HTTP + WS mock pair.
    pub async fn open_at(api_key: &str, post_endpoint: &str) -> Result<Self, MuniError> {
        if api_key.is_empty() {
            return Err(MuniError::GladiaMissingApiKey);
        }

        let init = post_live_init(api_key, post_endpoint).await?;

        // Phase 2: open the WebSocket pointed at the per-session URL.
        let request =
            init.url
                .into_client_request()
                .map_err(|e| MuniError::GladiaConnectionFailed {
                    reason: e.to_string(),
                })?;
        let connect = tokio::time::timeout(OPEN_TIMEOUT, connect_async(request));
        let (ws, _resp) = match connect.await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                // The WS uses a per-session URL (no auth header), so a
                // 401/403 here is rare — but if the upgrade is rejected for
                // auth reasons, treat it as a rejected key for consistency
                // with the POST path rather than a transient connection blip.
                if let tokio_tungstenite::tungstenite::Error::Http(resp) = &e {
                    let status = resp.status().as_u16();
                    if status == 401 || status == 403 {
                        log::warn!(
                            target: "gladia",
                            "Gladia handshake rejected with HTTP {status} — treating as rejected API key"
                        );
                        return Err(MuniError::GladiaKeyRejected);
                    }
                }
                return Err(MuniError::GladiaConnectionFailed {
                    reason: e.to_string(),
                });
            }
            Err(_) => {
                return Err(MuniError::GladiaConnectionFailed {
                    reason: format!("handshake timed out after {:?}", OPEN_TIMEOUT),
                })
            }
        };

        let (sink, stream) = ws.split();
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: String::new(),
            finalize_tx: None,
            last_metadata_duration: None,
            received_audio_chunk_ack: false,
        }));
        let sink = Arc::new(TokioMutex::new(Some(sink)));
        let receive_handle = tokio::spawn(receive_loop(stream, state.clone()));

        // Session id logged at info so the dev shell can cross-reference
        // a misbehaving press against the Gladia dashboard. The api_key
        // travels in a request header (set in `post_live_init`) and is
        // never echoed.
        log::info!(
            target: "gladia",
            "Gladia WebSocket opened: session_id={}",
            init.session_id
        );

        Ok(Self {
            sink,
            state,
            receive_handle: StdMutex::new(Some(receive_handle)),
            session_id: init.session_id,
            finalize_timeout_ms: AtomicU64::new(FINALIZE_TIMEOUT.as_millis() as u64),
        })
    }

    /// Gladia-issued session id captured from `POST /v2/live`. Useful
    /// in logs to cross-reference a press against the dashboard.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Send one chunk of 16 kHz mono linear16 PCM audio.
    ///
    /// Samples are reinterpreted as little-endian bytes — matching the
    /// `encoding: "wav/pcm"` / `bit_depth: 16` config sent on POST.
    /// Returns [`MuniError::GladiaConnectionFailed`] if the socket has
    /// been closed or if the underlying write fails.
    pub async fn send(&self, audio: &[i16]) -> Result<(), MuniError> {
        let bytes = asr_stream::i16_slice_to_le_bytes(audio);
        let mut guard = self.sink.lock().await;
        let sink = guard
            .as_mut()
            .ok_or_else(|| MuniError::GladiaConnectionFailed {
                reason: "stream closed".into(),
            })?;
        asr_stream::send_frame_timed(
            sink,
            Message::Binary(bytes),
            SEND_TIMEOUT,
            "send",
            gladia_connection_failed,
        )
        .await
    }

    /// Widen the finalize budget for a rescue-mode replay of `audio_seconds`
    /// of buffered audio (plan 039 task 7b). No-op semantics for the primary
    /// streaming route, which never calls this and keeps the tight default.
    pub fn mark_rescue_replay(&self, audio_seconds: f64) {
        let budget = rescue_finalize_budget(audio_seconds);
        self.finalize_timeout_ms
            .store(budget.as_millis() as u64, Ordering::Relaxed);
    }

    /// Override the finalize timeout. Test-only: lets the integration suite
    /// drive the timeout→partial-recovery path in tens of milliseconds instead
    /// of the production 3 s. Gated behind `test-fixtures` so it never compiles
    /// into release bundles (mirrors `DeepgramClient::set_finalize_timeout`).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn set_finalize_timeout(&self, d: Duration) {
        self.finalize_timeout_ms
            .store(d.as_millis() as u64, Ordering::Relaxed);
    }

    fn finalize_timeout(&self) -> Duration {
        Duration::from_millis(self.finalize_timeout_ms.load(Ordering::Relaxed))
    }

    /// Send the `stop_recording` control frame and await the final
    /// transcript.
    ///
    /// Resolves when the receive task observes the corresponding final
    /// `transcript` frame, a server `error` frame, or the socket
    /// disconnects mid-flight. On a mid-flight disconnect with
    /// non-empty accumulated transcript, returns the partial transcript
    /// instead of an error (mirrors `deepgram.rs`).
    ///
    /// Bounded by [`FINALIZE_TIMEOUT`] — a half-open socket would
    /// otherwise leave the receive task waiting on a frame that will
    /// never arrive.
    pub async fn finalize(&self) -> Result<FinalizeOutcome, MuniError> {
        let rx = self.install_finalize_continuation().await?;
        // Mirror Deepgram (task 6a): on `stop_recording` send FAILURE do not
        // `?`-propagate — recover the accumulated finals instead of discarding
        // the press, and always clear `finalize_tx`.
        if let Err(send_err) = self.send_stop_recording().await {
            log::warn!(
                target: "gladia",
                "stop_recording send failed ({}) — recovering accumulated finals",
                send_err.user_message()
            );
            return self.recover_partial_on_finalize_failure(send_err).await;
        }
        let timeout = self.finalize_timeout();
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(MuniError::GladiaInvalidResponse),
            Err(_) => self.recover_on_finalize_timeout(timeout).await,
        }
    }

    /// Recover the accumulated transcript after a `stop_recording` send
    /// failure. Always clears `finalize_tx`; returns
    /// [`FinalizeOutcome::Partial`] when finals accumulated, else the send
    /// error.
    async fn recover_partial_on_finalize_failure(
        &self,
        fallback_err: MuniError,
    ) -> Result<FinalizeOutcome, MuniError> {
        let mut s = self.state.lock().await;
        let s = &mut *s;
        asr_stream::recover_partial(&mut s.finalize_tx, &mut s.final_transcript, || fallback_err)
    }

    /// Resolve a finalize timeout. When finals accumulated, recover them as a
    /// [`FinalizeOutcome::Partial`] (task 7a). Otherwise classify the empty
    /// timeout: if Gladia acked any audio chunk the press was lost to a
    /// mid-stream stall (backlog 0019) — surface Loud
    /// [`MuniError::GladiaFinalizeTimeoutAfterAudio`] — else the connection
    /// never produced anything useful and the Quiet
    /// [`MuniError::GladiaConnectionFailed`] is the right shape.
    async fn recover_on_finalize_timeout(
        &self,
        timeout: Duration,
    ) -> Result<FinalizeOutcome, MuniError> {
        let mut s = self.state.lock().await;
        let _ = s.finalize_tx.take();
        let transcript = std::mem::take(&mut s.final_transcript);
        let saw_audio_ack = s.received_audio_chunk_ack;
        drop(s);
        if !transcript.is_empty() {
            Ok(FinalizeOutcome::Partial(transcript))
        } else if saw_audio_ack {
            Err(MuniError::GladiaFinalizeTimeoutAfterAudio)
        } else {
            Err(MuniError::GladiaConnectionFailed {
                reason: format!("finalize timed out after {timeout:?}"),
            })
        }
    }

    /// Return the most recent audio-duration value (seconds) reported
    /// by the server in the final transcript frame, or `None` if no
    /// final frame has arrived (early-error close, mid-flight
    /// disconnect, etc.). Cost-tracking reads this after `finalize()`.
    pub async fn last_metadata_duration(&self) -> Option<f64> {
        self.state.lock().await.last_metadata_duration
    }

    /// Tear down the WebSocket. Idempotent.
    ///
    /// After `close()`, `send`/`finalize` will return
    /// [`MuniError::GladiaConnectionFailed`].
    pub async fn close(&self) {
        let sink = self.sink.lock().await.take();
        if let Some(mut s) = sink {
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, s.close()).await;
        }
        let handle = match self.receive_handle.lock() {
            Ok(mut g) => g.take(),
            Err(p) => p.into_inner().take(),
        };
        if let Some(h) = handle {
            h.abort();
        }
        let mut state = self.state.lock().await;
        if let Some(tx) = state.finalize_tx.take() {
            let _ = tx.send(Err(MuniError::GladiaConnectionFailed {
                reason: "client closed".into(),
            }));
        }
        log::info!(
            target: "gladia",
            "Gladia WebSocket closed: session_id={}",
            self.session_id
        );
    }

    async fn install_finalize_continuation(
        &self,
    ) -> Result<oneshot::Receiver<Result<FinalizeOutcome, MuniError>>, MuniError> {
        let (tx, rx) = oneshot::channel();
        let mut state = self.state.lock().await;
        if state.finalize_tx.is_some() {
            // Two concurrent finalize() calls — refuse the second so
            // the first's continuation is preserved.
            return Err(MuniError::GladiaInvalidResponse);
        }
        state.finalize_tx = Some(tx);
        Ok(rx)
    }

    async fn send_stop_recording(&self) -> Result<(), MuniError> {
        let mut guard = self.sink.lock().await;
        let sink = guard
            .as_mut()
            .ok_or_else(|| MuniError::GladiaConnectionFailed {
                reason: "stream closed".into(),
            })?;
        asr_stream::send_frame_timed(
            sink,
            Message::Text(STOP_RECORDING_PAYLOAD.to_string()),
            SEND_TIMEOUT,
            "stop_recording send",
            gladia_connection_failed,
        )
        .await
    }
}

impl Drop for GladiaClient {
    fn drop(&mut self) {
        let handle = match self.receive_handle.lock() {
            Ok(mut g) => g.take(),
            Err(p) => p.into_inner().take(),
        };
        if let Some(h) = handle {
            h.abort();
        }
    }
}

/// `POST /v2/live` response shape — only the two fields we consume.
struct InitResponse {
    session_id: String,
    url: String,
}

/// Build the JSON config payload sent in the `POST /v2/live` body.
///
/// Pulled out so the format is unit-testable and future config
/// additions (per-user vocabulary, endpointing tuning, language list
/// extension) land in one place.
fn build_post_body() -> serde_json::Value {
    json!({
        "encoding": "wav/pcm",
        "sample_rate": 16000,
        "bit_depth": 16,
        "channels": 1,
        "language_config": {
            // "tl" is the right Gladia code for Tagalog/Filipino — there
            // is no separate "fil" entry in the schema (research §2.2).
            "languages": ["en", "tl"],
            // Native code-switching — the headline feature this prototype
            // exists to evaluate (research §5.1).
            "code_switching": true
        },
        "realtime_processing": {
            "custom_vocabulary": true,
            "custom_vocabulary_config": {
                // App brand bias — the only term safe to ship as a
                // default (analogous to deepgram.rs::DEFAULT_KEYTERMS).
                "vocabulary": [{ "value": "muni", "intensity": 0.5 }],
                "default_intensity": 0.5
            }
        },
        // Tight endpoint silence so finals fire quickly when the user
        // releases (Solaria defaults are conversational; dictation
        // wants snappier — research §7 risk row 4).
        "endpointing": 0.05,
        // Long ceiling so a 30-second dictation press is not chopped
        // mid-utterance. Default is 5 s; bump to 60 s.
        "maximum_duration_without_endpointing": 60,
        // We only need finals — partials add parse noise and would
        // double the receive-loop traffic for no orchestrator benefit.
        "messages_config": {
            "receive_partial_transcripts": false,
            "receive_final_transcripts": true
        }
    })
}

async fn post_live_init(api_key: &str, post_endpoint: &str) -> Result<InitResponse, MuniError> {
    let resp = POST_CLIENT
        .post(post_endpoint)
        .header("x-gladia-key", api_key)
        .json(&build_post_body())
        .send()
        .await
        .map_err(|e| MuniError::GladiaConnectionFailed {
            reason: e.to_string(),
        })?;
    let status = resp.status();
    if !status.is_success() {
        // Surface the status code only; the body may carry rate-limit
        // detail that's useful in logs but not in user-facing copy.
        let body = resp.text().await.unwrap_or_default();
        log::warn!(
            target: "gladia",
            "POST /v2/live returned {} body={}",
            status,
            body.chars().take(256).collect::<String>()
        );
        // A 401/403 on the init POST means the `x-gladia-key` was rejected
        // (expired/revoked). Split it into the actionable `GladiaKeyRejected`
        // kind (silent notification → API Keys) instead of a generic HUD chip.
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(MuniError::GladiaKeyRejected);
        }
        return Err(MuniError::GladiaServerError {
            message: format!("POST /v2/live returned {status}"),
        });
    }
    let parsed: serde_json::Value = resp
        .json()
        .await
        .map_err(|_| MuniError::GladiaInvalidResponse)?;
    parse_init_response(&parsed).ok_or(MuniError::GladiaInvalidResponse)
}

fn parse_init_response(value: &serde_json::Value) -> Option<InitResponse> {
    let obj = value.as_object()?;
    // Gladia's docs name the response keys `id` and `url`; older
    // example payloads use `session_id` / `websocket_url`. Accept both
    // shapes defensively — the request-id-as-string is the only thing
    // we use semantically.
    let session_id = obj
        .get("id")
        .or_else(|| obj.get("session_id"))
        .and_then(|v| v.as_str())?
        .to_string();
    let url = obj
        .get("url")
        .or_else(|| obj.get("websocket_url"))
        .and_then(|v| v.as_str())?
        .to_string();
    if session_id.is_empty() || url.is_empty() {
        return None;
    }
    Some(InitResponse { session_id, url })
}

// -- receive loop -----------------------------------------------------------

async fn receive_loop(mut stream: WsRead, state: Arc<TokioMutex<State>>) {
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                log::trace!(target: "gladia", "ws inbound: {text}");
                if let Some(parsed) = parse_message(&text) {
                    apply_message(parsed, &state).await;
                }
            }
            Ok(Message::Binary(bytes)) => {
                // Defensive: Gladia sends JSON as text in practice; if
                // a binary envelope ever appears, treat it as UTF-8.
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    log::trace!(target: "gladia", "ws inbound (binary): {text}");
                    if let Some(parsed) = parse_message(text) {
                        apply_message(parsed, &state).await;
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(e) => {
                resolve_finalize_on_disconnect(&state, e.to_string()).await;
                return;
            }
        }
    }
    resolve_finalize_on_disconnect(&state, "stream ended".to_string()).await;
}

async fn apply_message(parsed: ParsedMessage, state: &Arc<TokioMutex<State>>) {
    match parsed {
        ParsedMessage::Transcript {
            is_final,
            text,
            audio_duration,
        } => {
            if !is_final {
                // Partials are disabled in the config, but defend
                // against a server reverting to default behaviour.
                return;
            }
            let mut s = state.lock().await;
            if !text.is_empty() {
                if !s.final_transcript.is_empty() {
                    // Gladia's per-utterance text already carries its
                    // own leading whitespace in practice; the
                    // single-space join here matches Deepgram's
                    // accumulator and keeps a clean transcript when
                    // multiple finals arrive within one press.
                    s.final_transcript.push(' ');
                }
                s.final_transcript.push_str(&text);
            }
            if let Some(seconds) = audio_duration {
                s.last_metadata_duration = Some(seconds);
            }
            log::debug!(
                target: "gladia",
                "accumulated final transcript ({} chars)",
                s.final_transcript.len()
            );
        }
        ParsedMessage::SessionEnded { audio_duration } => {
            let mut s = state.lock().await;
            if let Some(seconds) = audio_duration {
                s.last_metadata_duration = Some(seconds);
            }
            // Only drain the finals when a `finalize()` is actually waiting
            // (mirrors Deepgram task 6b) so a stray terminal frame can't throw
            // away a press the eventual finalize still needs.
            if let Some(tx) = s.finalize_tx.take() {
                let transcript = std::mem::take(&mut s.final_transcript);
                let _ = tx.send(Ok(FinalizeOutcome::Complete(transcript)));
            }
        }
        ParsedMessage::ServerError(message) => {
            let mut s = state.lock().await;
            if let Some(tx) = s.finalize_tx.take() {
                let _ = tx.send(Err(MuniError::GladiaServerError { message }));
            }
        }
        ParsedMessage::AudioChunkAck => {
            let mut s = state.lock().await;
            s.received_audio_chunk_ack = true;
        }
        ParsedMessage::Ignored => {}
    }
}

async fn resolve_finalize_on_disconnect(state: &Arc<TokioMutex<State>>, reason: String) {
    let mut s = state.lock().await;
    let s = &mut *s;
    asr_stream::resolve_disconnect(&mut s.finalize_tx, &mut s.final_transcript, || {
        MuniError::GladiaConnectionFailed { reason }
    });
}

// -- pure parser ------------------------------------------------------------

/// Parsed shape of a Gladia server message — only the fields we
/// actually consume. Returning a typed enum keeps the receive-loop
/// body tiny and lets us unit-test parsing without spinning up a
/// WebSocket.
#[derive(Debug, PartialEq)]
enum ParsedMessage {
    /// One `transcript` envelope. `is_final` discriminates between a
    /// partial (which we drop) and a final (which we accumulate).
    /// `audio_duration` is the running session audio-time in seconds,
    /// reported on most finals — cached for the cost-tracking path.
    Transcript {
        is_final: bool,
        text: String,
        audio_duration: Option<f64>,
    },
    /// Gladia's documented `post_final_transcript` / `end_session`
    /// envelope (the exact tag varies across protocol revisions; we
    /// accept the union). Carries the session-total audio duration
    /// when present.
    SessionEnded { audio_duration: Option<f64> },
    /// Gladia emitted an error envelope.
    ServerError(String),
    /// `audio_chunk` envelope — server ack for one of our binary
    /// audio frames. The contents (`byte_range`, `time_range`) are
    /// not used directly, but receiving one is a load-bearing
    /// signal that the WS is alive and audio is flowing: the
    /// receive loop sets `State::received_audio_chunk_ack` so a
    /// later finalize timeout can be classified as "stall after
    /// audio" (Loud) vs "no traffic at all" (Quiet).
    AudioChunkAck,
    /// Well-formed JSON we don't care about (speech_event,
    /// audio_chunk_acknowledged, start_session, …).
    Ignored,
}

fn parse_message(text: &str) -> Option<ParsedMessage> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let obj = value.as_object()?;
    let msg_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match msg_type {
        "transcript" | "final_transcript" | "partial_transcript" => {
            let data = obj.get("data").and_then(|v| v.as_object());
            // `is_final` lives at `data.is_final` in current docs; some
            // older example payloads put it at the top level. Accept
            // both and prefer the data-nested form.
            let is_final = data
                .and_then(|d| d.get("is_final").and_then(|v| v.as_bool()))
                .or_else(|| obj.get("is_final").and_then(|v| v.as_bool()))
                .unwrap_or_else(|| msg_type == "final_transcript");
            let text = data
                .and_then(|d| d.get("utterance").and_then(|u| u.as_object()))
                .and_then(|u| u.get("text").and_then(|t| t.as_str()))
                .or_else(|| data.and_then(|d| d.get("text").and_then(|t| t.as_str())))
                .unwrap_or("")
                .to_string();
            // Gladia reports audio-time progress per frame as
            // `data.time_end` or `data.audio_duration` (depending on
            // the schema revision). Accept both.
            let audio_duration = data
                .and_then(|d| {
                    d.get("audio_duration")
                        .and_then(|v| v.as_f64())
                        .or_else(|| d.get("time_end").and_then(|v| v.as_f64()))
                })
                .or_else(|| obj.get("audio_duration").and_then(|v| v.as_f64()));
            Some(ParsedMessage::Transcript {
                is_final,
                text,
                audio_duration,
            })
        }
        "post_final_transcript" | "end_session" => {
            // Gladia's actual schema (confirmed via trace dogfood
            // 2026-05-12 against the live `/v2/live` endpoint) nests
            // the billing fields under `data.metadata`:
            //   data.metadata.audio_duration   — submitted audio,
            //                                    integer seconds.
            //   data.metadata.billing_time     — what they invoice on,
            //                                    floored to whole seconds.
            //   data.metadata.transcription_time — server-side processing
            //                                      time (not billed).
            // We prefer `billing_time` so muni's cost column matches the
            // Gladia dashboard to the cent, then fall back to
            // `audio_duration` (semantically equivalent in current schema,
            // included in case Gladia drops one of the two later), then
            // legacy top-level `data.audio_duration` / `data.billed_time`
            // shapes for forward-compat.
            let data = obj.get("data").and_then(|d| d.as_object());
            let metadata = data.and_then(|d| d.get("metadata").and_then(|m| m.as_object()));
            let audio_duration = metadata
                .and_then(|m| {
                    m.get("billing_time")
                        .and_then(|v| v.as_f64())
                        .or_else(|| m.get("audio_duration").and_then(|v| v.as_f64()))
                })
                .or_else(|| {
                    data.and_then(|d| {
                        d.get("audio_duration")
                            .and_then(|v| v.as_f64())
                            .or_else(|| d.get("billed_time").and_then(|v| v.as_f64()))
                    })
                })
                .or_else(|| obj.get("audio_duration").and_then(|v| v.as_f64()));
            Some(ParsedMessage::SessionEnded { audio_duration })
        }
        "error" => {
            let message = obj
                .get("message")
                .or_else(|| obj.get("description"))
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Gladia error")
                .to_string();
            Some(ParsedMessage::ServerError(message))
        }
        // `audio_chunk` is the server ack for each binary frame we
        // sent — confirmed via trace dogfood 2026-05-12. It's
        // distinguished from the lifecycle/progress group below
        // because receiving one tells finalize-timeout
        // classification that audio was flowing before the stall
        // (backlog 0019).
        "audio_chunk" => Some(ParsedMessage::AudioChunkAck),
        // Lifecycle / progress frames we don't act on.
        // `post_transcript` is Gladia's intermediate "here is the
        // full transcript so far" before the final-with-metadata
        // frame (`post_final_transcript`) — confirmed via trace
        // dogfood 2026-05-12 (`docs/qa/004` §9 follow-up).
        // `stop_recording` is the server's ack of our finalize
        // sentinel.
        "start_session"
        | "speech_event"
        | "speech_start"
        | "speech_end"
        | "audio_chunk_acknowledged"
        | "stop_recording"
        | "post_transcript"
        | "lifecycle_event" => Some(ParsedMessage::Ignored),
        // Unknown but well-formed JSON.
        _ => Some(ParsedMessage::Ignored),
    }
}

// -- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_body_includes_required_fields() {
        let body = build_post_body();
        let obj = body.as_object().expect("object");
        assert_eq!(
            obj.get("encoding").and_then(|v| v.as_str()),
            Some("wav/pcm")
        );
        assert_eq!(obj.get("sample_rate").and_then(|v| v.as_i64()), Some(16000));
        assert_eq!(obj.get("bit_depth").and_then(|v| v.as_i64()), Some(16));
        assert_eq!(obj.get("channels").and_then(|v| v.as_i64()), Some(1));

        let lc = obj
            .get("language_config")
            .and_then(|v| v.as_object())
            .expect("language_config");
        let langs = lc
            .get("languages")
            .and_then(|v| v.as_array())
            .expect("languages array");
        assert_eq!(
            langs.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>(),
            vec!["en", "tl"]
        );
        assert_eq!(
            lc.get("code_switching").and_then(|v| v.as_bool()),
            Some(true)
        );

        assert_eq!(obj.get("endpointing").and_then(|v| v.as_f64()), Some(0.05));
        assert_eq!(
            obj.get("maximum_duration_without_endpointing")
                .and_then(|v| v.as_i64()),
            Some(60)
        );

        let msgs = obj
            .get("messages_config")
            .and_then(|v| v.as_object())
            .expect("messages_config");
        assert_eq!(
            msgs.get("receive_partial_transcripts")
                .and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            msgs.get("receive_final_transcripts")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn parse_init_response_accepts_id_url_shape() {
        let v = json!({ "id": "abc-123", "url": "wss://example/transcribe" });
        let init = parse_init_response(&v).expect("parses");
        assert_eq!(init.session_id, "abc-123");
        assert_eq!(init.url, "wss://example/transcribe");
    }

    #[test]
    fn parse_init_response_accepts_session_id_websocket_url_shape() {
        // Defensive: older Gladia example payloads use the verbose
        // field names. Both shapes should parse identically.
        let v = json!({
            "session_id": "xyz-789",
            "websocket_url": "wss://example/transcribe-789"
        });
        let init = parse_init_response(&v).expect("parses");
        assert_eq!(init.session_id, "xyz-789");
        assert_eq!(init.url, "wss://example/transcribe-789");
    }

    #[test]
    fn parse_init_response_rejects_missing_fields() {
        assert!(parse_init_response(&json!({ "id": "abc" })).is_none());
        assert!(parse_init_response(&json!({ "url": "wss://x" })).is_none());
        assert!(parse_init_response(&json!({ "id": "", "url": "wss://x" })).is_none());
        assert!(parse_init_response(&json!({})).is_none());
    }

    #[test]
    fn transcript_message_with_data_envelope_is_final() {
        let json = r#"{
            "type": "transcript",
            "data": {
                "is_final": true,
                "utterance": { "text": "hello world", "language": "en" },
                "audio_duration": 1.23
            }
        }"#;
        let parsed = parse_message(json).expect("parses");
        assert_eq!(
            parsed,
            ParsedMessage::Transcript {
                is_final: true,
                text: "hello world".into(),
                audio_duration: Some(1.23),
            }
        );
    }

    #[test]
    fn transcript_message_partial_is_marked_not_final() {
        let json = r#"{
            "type": "transcript",
            "data": { "is_final": false, "utterance": { "text": "hel" } }
        }"#;
        let parsed = parse_message(json).expect("parses");
        match parsed {
            ParsedMessage::Transcript { is_final, .. } => assert!(!is_final),
            other => panic!("expected Transcript, got {other:?}"),
        }
    }

    #[test]
    fn final_transcript_envelope_type_implies_is_final() {
        // Some doc samples use `type: "final_transcript"` without
        // re-emitting `is_final: true` in the data block.
        let json = r#"{
            "type": "final_transcript",
            "data": { "utterance": { "text": "ok" } }
        }"#;
        let parsed = parse_message(json).expect("parses");
        match parsed {
            ParsedMessage::Transcript { is_final, text, .. } => {
                assert!(is_final);
                assert_eq!(text, "ok");
            }
            other => panic!("expected Transcript, got {other:?}"),
        }
    }

    #[test]
    fn post_final_transcript_envelope_yields_session_ended() {
        let json = r#"{
            "type": "post_final_transcript",
            "data": { "audio_duration": 4.5 }
        }"#;
        let parsed = parse_message(json).expect("parses");
        assert_eq!(
            parsed,
            ParsedMessage::SessionEnded {
                audio_duration: Some(4.5)
            }
        );
    }

    /// Real-world payload shape confirmed via trace dogfood
    /// 2026-05-12 against the live `/v2/live` endpoint: billing fields
    /// nest under `data.metadata`. Pre-fix this returned None and the
    /// recorder used the press-duration fallback (`status =
    /// ok_fallback_press_duration` in the api_calls table). The
    /// `billing_time` field is the dashboard-parity number Gladia
    /// invoices on (floored to whole seconds), so we prefer it over
    /// `audio_duration` when both are present.
    #[test]
    fn post_final_transcript_reads_metadata_billing_time() {
        let json = r#"{
            "type": "post_final_transcript",
            "data": {
                "metadata": {
                    "audio_duration": 2,
                    "billing_time": 2,
                    "number_of_distinct_channels": 1,
                    "transcription_time": 5.57
                },
                "transcription": { "full_transcript": "x" }
            }
        }"#;
        assert_eq!(
            parse_message(json),
            Some(ParsedMessage::SessionEnded {
                audio_duration: Some(2.0)
            })
        );
    }

    #[test]
    fn post_final_transcript_prefers_billing_time_over_audio_duration() {
        // If Gladia ever diverges (audio submitted vs what they bill —
        // floor rounding), the column we record must match the
        // dashboard. `billing_time` wins.
        let json = r#"{
            "type": "post_final_transcript",
            "data": {
                "metadata": {
                    "audio_duration": 2.539,
                    "billing_time": 2
                }
            }
        }"#;
        assert_eq!(
            parse_message(json),
            Some(ParsedMessage::SessionEnded {
                audio_duration: Some(2.0)
            })
        );
    }

    #[test]
    fn post_final_transcript_falls_back_to_metadata_audio_duration() {
        // If `billing_time` ever disappears from the schema, the next
        // best signal is `audio_duration` in the metadata block.
        let json = r#"{
            "type": "post_final_transcript",
            "data": {
                "metadata": {
                    "audio_duration": 3.7
                }
            }
        }"#;
        assert_eq!(
            parse_message(json),
            Some(ParsedMessage::SessionEnded {
                audio_duration: Some(3.7)
            })
        );
    }

    #[test]
    fn error_envelope_extracts_message_or_falls_back() {
        let with = r#"{"type":"error","message":"rate limited"}"#;
        assert_eq!(
            parse_message(with),
            Some(ParsedMessage::ServerError("rate limited".into()))
        );
        let without = r#"{"type":"error"}"#;
        assert_eq!(
            parse_message(without),
            Some(ParsedMessage::ServerError("Unknown Gladia error".into()))
        );
    }

    #[test]
    fn lifecycle_envelopes_are_ignored() {
        for json in [
            r#"{"type":"speech_event"}"#,
            r#"{"type":"start_session"}"#,
            r#"{"type":"audio_chunk_acknowledged"}"#,
            r#"{"type":"speech_start"}"#,
            r#"{"type":"speech_end"}"#,
            r#"{"type":"stop_recording"}"#,
            r#"{"type":"post_transcript"}"#,
        ] {
            assert_eq!(parse_message(json), Some(ParsedMessage::Ignored));
        }
    }

    #[test]
    fn audio_chunk_envelope_parses_as_audio_chunk_ack() {
        // The server ack frame for each binary audio frame we sent.
        // Distinguished from generic lifecycle ignores (backlog 0019)
        // so a later finalize timeout can be classified as "stall
        // after audio was flowing" rather than "no traffic at all".
        let json = r#"{
            "type": "audio_chunk",
            "data": {
                "byte_range": [0, 1024],
                "time_range": [0.0, 0.95]
            }
        }"#;
        assert_eq!(parse_message(json), Some(ParsedMessage::AudioChunkAck));
    }

    #[tokio::test]
    async fn audio_chunk_ack_sets_state_flag() {
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: String::new(),
            finalize_tx: None,
            last_metadata_duration: None,
            received_audio_chunk_ack: false,
        }));
        apply_message(ParsedMessage::AudioChunkAck, &state).await;
        assert!(state.lock().await.received_audio_chunk_ack);
    }

    #[test]
    fn unknown_type_is_ignored_not_dropped() {
        assert_eq!(
            parse_message(r#"{"type":"future_event"}"#),
            Some(ParsedMessage::Ignored)
        );
    }

    #[test]
    fn invalid_json_returns_none() {
        assert!(parse_message("not json").is_none());
    }

    #[test]
    fn rescue_finalize_budget_scales_and_caps() {
        // Base 3s with no audio.
        assert_eq!(rescue_finalize_budget(0.0), Duration::from_secs(3));
        // 3s + 250ms * 8s = 5s.
        assert_eq!(rescue_finalize_budget(8.0), Duration::from_secs(5));
        // Capped at 15s for very long replays (3 + 0.25*60 = 18 → 15).
        assert_eq!(rescue_finalize_budget(60.0), Duration::from_secs(15));
        // Negative/garbage clamps to the base.
        assert_eq!(rescue_finalize_budget(-5.0), Duration::from_secs(3));
    }

    #[tokio::test]
    async fn apply_message_only_accumulates_final_transcripts() {
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: String::new(),
            finalize_tx: None,
            last_metadata_duration: None,
            received_audio_chunk_ack: false,
        }));

        // Partial should be dropped even if it carries text.
        apply_message(
            ParsedMessage::Transcript {
                is_final: false,
                text: "hel".into(),
                audio_duration: None,
            },
            &state,
        )
        .await;

        // First final.
        apply_message(
            ParsedMessage::Transcript {
                is_final: true,
                text: "hello".into(),
                audio_duration: Some(0.5),
            },
            &state,
        )
        .await;

        // Second final — joined with a single space.
        apply_message(
            ParsedMessage::Transcript {
                is_final: true,
                text: "world".into(),
                audio_duration: Some(1.0),
            },
            &state,
        )
        .await;

        let s = state.lock().await;
        assert_eq!(s.final_transcript, "hello world");
        assert_eq!(s.last_metadata_duration, Some(1.0));
    }

    #[tokio::test]
    async fn session_ended_resolves_pending_finalize_with_accumulated_transcript() {
        let (tx, rx) = oneshot::channel();
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: "the quick fox".into(),
            finalize_tx: Some(tx),
            last_metadata_duration: None,
            received_audio_chunk_ack: false,
        }));
        apply_message(
            ParsedMessage::SessionEnded {
                audio_duration: Some(2.5),
            },
            &state,
        )
        .await;
        let result = rx.await.expect("oneshot delivers");
        assert_eq!(result.unwrap(), "the quick fox");
        let s = state.lock().await;
        assert!(s.final_transcript.is_empty());
        assert!(s.finalize_tx.is_none());
        assert_eq!(s.last_metadata_duration, Some(2.5));
    }

    #[tokio::test]
    async fn apply_server_error_resolves_pending_finalize_with_typed_error() {
        let (tx, rx) = oneshot::channel();
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: String::new(),
            finalize_tx: Some(tx),
            last_metadata_duration: None,
            received_audio_chunk_ack: false,
        }));
        apply_message(ParsedMessage::ServerError("rate limited".into()), &state).await;
        let err = rx.await.expect("oneshot delivers").unwrap_err();
        match err {
            MuniError::GladiaServerError { message } => assert_eq!(message, "rate limited"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn disconnect_with_partial_transcript_returns_partial_not_error() {
        let (tx, rx) = oneshot::channel();
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: "hello".into(),
            finalize_tx: Some(tx),
            last_metadata_duration: None,
            received_audio_chunk_ack: false,
        }));
        resolve_finalize_on_disconnect(&state, "io error".into()).await;
        let result = rx.await.expect("oneshot delivers").unwrap();
        assert_eq!(result, "hello");
    }

    #[tokio::test]
    async fn disconnect_with_no_transcript_returns_connection_failed() {
        let (tx, rx) = oneshot::channel();
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: String::new(),
            finalize_tx: Some(tx),
            last_metadata_duration: None,
            received_audio_chunk_ack: false,
        }));
        resolve_finalize_on_disconnect(&state, "io error".into()).await;
        let err = rx.await.expect("oneshot delivers").unwrap_err();
        assert!(matches!(err, MuniError::GladiaConnectionFailed { .. }));
    }

    #[tokio::test]
    async fn open_with_empty_api_key_returns_missing_error() {
        match GladiaClient::open("").await {
            Err(MuniError::GladiaMissingApiKey) => {}
            Err(other) => panic!("expected GladiaMissingApiKey, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }
}
