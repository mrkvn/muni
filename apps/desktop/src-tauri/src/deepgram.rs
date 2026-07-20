//! Deepgram Nova-3 streaming ASR client.
//!
//! Mirrors Swift v1's `DeepgramClient` (`muni/Core/DeepgramClient.swift`):
//! a single-shot push-to-talk WebSocket session that opens, accepts binary
//! 16 kHz / mono / linear16 audio frames, then on `finalize()` sends a
//! `CloseStream` control message and returns the accumulated `is_final`
//! transcript when the server replies with `Metadata`.
//!
//! Wire format (must stay byte-identical with Swift v1, with the addition of
//! Nova-3 Keyterm Prompting on top — see [`default_endpoint`]):
//! - URL: see [`BASE_ENDPOINT`] / [`default_endpoint`].
//! - Auth: `Authorization: Token <KEY>` (NOT `Bearer`).
//! - Audio: `Message::Binary` of little-endian i16 samples.
//! - Finalize: `Message::Text("{\"type\":\"CloseStream\"}")`.
//! - Server messages JSON-tagged via top-level `"type"`: `Results`,
//!   `Metadata`, `Error`, plus `UtteranceEnd` / `SpeechStarted` (ignored).
//!
//! Concurrency: the WebSocket is `split()` so `send`/`finalize` (writers)
//! and the spawned receive task (reader) can interleave without re-entry.
//! Receive-task ↔ `finalize()` synchronization uses a `oneshot` channel
//! installed by `finalize()` and consumed when `Metadata`, `Error`, or a
//! transport error arrives.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot, Mutex as TokioMutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::asr_stream::{self, WsRead, WsSink};
use crate::error::MuniError;

// `FinalizeOutcome` now lives in the shared streaming-ASR core so both
// providers speak the same finalize-recovery contract. Re-exported here so
// existing `crate::deepgram::FinalizeOutcome` / `muni_lib::deepgram::FinalizeOutcome`
// import paths (session.rs, tests/deepgram_mock.rs) stay valid.
pub use crate::asr_stream::FinalizeOutcome;

/// Maximum time `finalize()` will wait for Deepgram's `Metadata` reply
/// before giving up. A live socket replies within ~50–200 ms; anything
/// past 3 s indicates the WebSocket is in a half-open state (network
/// dropped after the press began) and we'd otherwise hang the press
/// pipeline indefinitely. QA repro: WiFi off mid-press → finalize
/// blocked the orchestrator for 6 s before the receive task noticed
/// the socket was gone.
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(3);

/// Maximum time the WebSocket handshake (`connect_async`) is allowed to
/// take. Tungstenite has no native operation timeouts; on a half-open
/// or unreachable network the handshake hangs for the OS TCP timeout
/// (~120 s on macOS), which freezes the warmer's `inflight` flag and
/// blocks every subsequent press from getting a fresh socket. Live
/// handshakes complete in 200–500 ms; 5 s leaves ample headroom for
/// flaky cellular / hotspot transitions without letting a true outage
/// pin a thread for minutes.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum time a single audio frame send is allowed to take. Audio
/// chunks arrive at ~60 Hz so a healthy send completes in tens of
/// milliseconds; anything past 1 s indicates the kernel TCP write
/// buffer has filled up because the network is gone. QA repro: WiFi
/// off mid-press → send blocked for 2 m 13 s before macOS finally
/// gave up the connection. The bounded send returns
/// `DeepgramConnectionFailed` so the forwarder's
/// `MAX_CONSECUTIVE_SEND_FAILURES` cap fires almost immediately and
/// the press unblocks.
const SEND_TIMEOUT: Duration = Duration::from_secs(1);

/// Maximum time `close()` will wait while sending the WebSocket close
/// frame. `close()` is best-effort by design (it tears down whether
/// the server cooperates or not); without a cap it can hang the press
/// teardown for the same OS TCP window as `send`.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(500);

/// Streaming endpoint without keyterm biasing applied.
///
/// Mirrors Swift v1's `DeepgramClient.defaultEndpoint`. Parameters chosen to
/// match the v1 contract: Nova-3 model, English, 16 kHz mono linear16,
/// no punctuation, no interim results, 300 ms endpointing window.
///
/// Production callers should use [`default_endpoint`] (which appends the
/// bundled [`DEFAULT_KEYTERMS`] vocabulary). [`BASE_ENDPOINT`] is exposed
/// directly so integration tests can pin a deterministic URL without the
/// keyterm tail.
pub const BASE_ENDPOINT: &str = "wss://api.deepgram.com/v1/listen?model=nova-3&language=en&encoding=linear16&sample_rate=16000&channels=1&punctuate=false&interim_results=false&endpointing=300";

/// Bundled keyterm vocabulary applied to every production session via
/// [`default_endpoint`]. Intentionally minimal: this list is *defaults*,
/// not user-curated vocabulary. Anything specific to one user's failures
/// or workflow belongs in the per-user vocabulary feature (parked as
/// backlog 0006), not here.
///
/// Curation policy for entries that DO live in the default list:
/// - Must be defensible without reference to any specific user's bug
///   report. The only entries that survive this bar today are the app's
///   own proper nouns — terms intrinsic to the product, not reactively
///   added against an observed mishear.
/// - Avoid single common words ("the", "and", "is") — Deepgram warns
///   these bias the model in unhelpful ways.
/// - Total budget: stay well under Deepgram's 500-token-per-request
///   ceiling. Deepgram recommends 20–50 high-signal terms over a long
///   list of marginal ones; the user-vocabulary feature (backlog 0006)
///   is where that headroom should be spent.
const DEFAULT_KEYTERMS: &[&str] = &[
    // The app's own brand. Proper nouns are ASR's classic blind spot
    // and users will dictate the app's name when talking about it.
    // This is the only default that's defensible without a user-specific
    // mishear report behind it.
    "Muni",
];

/// Streaming endpoint with the bundled [`DEFAULT_KEYTERMS`] applied.
///
/// Returns an owned `String` (rather than a `&'static str`) because keyterm
/// percent-encoding is computed at runtime — the encoded length depends on
/// the contents of the term list, which we want free to evolve without
/// re-measuring the const.
pub fn default_endpoint() -> String {
    build_endpoint(BASE_ENDPOINT, DEFAULT_KEYTERMS)
}

/// Env var that overrides the Deepgram streaming endpoint. Mirrors
/// [`crate::groq::GROQ_ENDPOINT_ENV`]. Intended for local-mock and dogfood
/// sessions — e.g. point it at an unreachable host
/// (`MUNI_DEEPGRAM_ENDPOINT=wss://127.0.0.1:1/v1/listen`) to simulate a network
/// drop and exercise the [`MuniError::DeepgramConnectionFailed`] → HUD-notice
/// path without touching real Wi-Fi.
pub const DEEPGRAM_ENDPOINT_ENV: &str = "MUNI_DEEPGRAM_ENDPOINT";

/// Resolve the Deepgram streaming endpoint from the env. Returns
/// [`default_endpoint`] when [`DEEPGRAM_ENDPOINT_ENV`] is unset, empty, or
/// whitespace-only. Pure-function counterpart lives in
/// [`parse_deepgram_endpoint`] for tests.
pub fn resolve_deepgram_endpoint() -> String {
    parse_deepgram_endpoint(std::env::var(DEEPGRAM_ENDPOINT_ENV).ok())
}

/// Pure-function counterpart to [`resolve_deepgram_endpoint`]. An override is
/// used verbatim (no keyterms appended): the caller is pointing at a mock or
/// dead host where keyterm biasing is irrelevant.
pub(crate) fn parse_deepgram_endpoint(raw: Option<String>) -> String {
    raw.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(default_endpoint)
}

/// Append `&keyterm=<encoded>` parameters to a base endpoint URL.
///
/// Each keyterm is percent-encoded per RFC 3986's unreserved set
/// (`A-Z` / `a-z` / `0-9` / `-` / `_` / `.` / `~` pass through; everything
/// else is `%`-escaped). Empty or whitespace-only entries are skipped so
/// callers can splice user-provided lists without sanitising upstream.
///
/// The base URL is assumed to already contain a query string (it does in
/// practice — see [`BASE_ENDPOINT`]) so we always join with `&`.
pub fn build_endpoint(base: &str, keyterms: &[&str]) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(base.len() + keyterms.len() * 24);
    out.push_str(base);
    for term in keyterms {
        let trimmed = term.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push_str("&keyterm=");
        for b in trimmed.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char);
                }
                _ => {
                    // write! to a String is infallible — the only failure
                    // mode of fmt::Write on String is allocation, which
                    // panics rather than returning Err.
                    let _ = write!(out, "%{b:02X}");
                }
            }
        }
    }
    out
}

/// `CloseStream` control message — flushes the server's transcription buffer
/// and triggers a final `Metadata` reply carrying the request summary.
const CLOSE_STREAM_PAYLOAD: &str = r#"{"type":"CloseStream"}"#;

/// `Finalize` control message — forces Deepgram to emit any currently-buffered
/// audio as a final `Results` message *without* closing the WebSocket. Used by
/// the feature-019 release path to drain pending low-confidence finals before
/// committing the route, so a Tagalog tail that Deepgram is holding (waiting
/// for an `endpointing=300` silence gap that never came) reaches the trigger
/// task in time to fire.
const FINALIZE_PAYLOAD: &str = r#"{"type":"Finalize"}"#;

/// Per-chunk confidence event forwarded out of the Deepgram receive
/// loop on every `is_final=true` `Results` message. Consumed by the
/// session-layer confidence-trigger task (feature 019) to decide
/// whether to fire a mid-press LID re-pass. Crosses the
/// `deepgram` ↔ `session` module boundary so the field types are
/// kept simple and copyable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChunkConfidence {
    /// `channel.alternatives[0].confidence` — the alternative-level
    /// score, in `[0.0, 1.0]`.
    pub confidence: f32,
    /// `channel.alternatives[0].words.len()` — count only; the words
    /// array contents aren't needed for the v1 trigger. `0` when the
    /// field is absent.
    pub words_in_chunk: usize,
    /// Always `true` for v1 — only `is_final` Results are forwarded —
    /// but carried explicitly so future per-interim work doesn't have
    /// to widen the channel type.
    pub is_final: bool,
}

/// Mutable state shared between the public API and the receive task.
///
/// `final_transcript` accumulates `is_final` `Results` fragments separated by
/// single spaces (matching Swift). `finalize_tx` is `Some` only between a
/// caller invoking `finalize()` and the receive task seeing the corresponding
/// `Metadata` (or an error/disconnect). `last_metadata_duration` captures
/// the Metadata frame's `duration` field (seconds, float) when present —
/// the cost-tracking path (feature 005) reads this after `finalize()`
/// returns to record `audio_seconds` for the Deepgram billing line.
/// `confidence_tx` is `Some` only when the session has installed a
/// continuation via [`DeepgramClient::install_confidence_continuation`] —
/// the receive loop forwards every `is_final=true` Results message's
/// per-chunk confidence to it via `try_send`.
struct State {
    final_transcript: String,
    finalize_tx: Option<oneshot::Sender<Result<FinalizeOutcome, MuniError>>>,
    last_metadata_duration: Option<f64>,
    confidence_tx: Option<mpsc::Sender<ChunkConfidence>>,
}

/// Push-to-talk Deepgram WebSocket client.
///
/// Construct via [`DeepgramClient::open`]. Lifetime: one client = one PTT
/// hold. After [`finalize`](Self::finalize) returns, the connection is dead;
/// open a new client for the next press.
pub struct DeepgramClient {
    sink: Arc<TokioMutex<Option<WsSink>>>,
    state: Arc<TokioMutex<State>>,
    /// Held in a sync mutex so [`Drop`] can `.abort()` the receive task
    /// without an async context.
    receive_handle: StdMutex<Option<JoinHandle<()>>>,
    /// How long [`finalize`](Self::finalize) waits for the `Metadata` ack
    /// before recovering the accumulated partial. Defaults to
    /// [`FINALIZE_TIMEOUT`] in production; overridable in tests via
    /// [`set_finalize_timeout`](Self::set_finalize_timeout) so the
    /// timeout→partial path can be exercised in well under a second.
    finalize_timeout: Duration,
    /// Artificial delay injected at the very top of [`close`](Self::close).
    /// Defaults to [`Duration::ZERO`] in production (a single `is_zero()`
    /// branch on the teardown path). Tests set it via
    /// [`set_close_grace`](Self::set_close_grace) to mimic a wedged half-open
    /// socket's `CLOSE_TIMEOUT` stall and prove that callers which discard a
    /// client (e.g. [`crate::session::DeepgramPool::take`]) fire-and-forget the
    /// teardown instead of blocking the press-start path on it.
    close_grace: Duration,
}

impl DeepgramClient {
    /// Connect to Deepgram and start the receive task.
    ///
    /// Returns [`MuniError::DeepgramMissingApiKey`] if `api_key` is empty
    /// (mirrors Swift v1's pre-flight check) and
    /// [`MuniError::DeepgramConnectionFailed`] for any other handshake or
    /// header-construction failure.
    pub async fn open(api_key: &str) -> Result<Self, MuniError> {
        Self::open_at(api_key, &default_endpoint()).await
    }

    /// Variant of [`open`](Self::open) that accepts a custom endpoint URL.
    /// Used by `tests/deepgram_mock.rs` to point the client at a local
    /// hand-rolled tungstenite server.
    pub async fn open_at(api_key: &str, endpoint: &str) -> Result<Self, MuniError> {
        if api_key.is_empty() {
            return Err(MuniError::DeepgramMissingApiKey);
        }

        let mut request =
            endpoint
                .into_client_request()
                .map_err(|e| MuniError::DeepgramConnectionFailed {
                    reason: e.to_string(),
                })?;
        let auth_value = format!("Token {api_key}").parse().map_err(|_| {
            // Empty was caught above; any other parse failure is a programmer
            // error (control characters in the key) but we surface it cleanly.
            MuniError::DeepgramConnectionFailed {
                reason: "invalid Authorization header value".into(),
            }
        })?;
        request.headers_mut().insert("Authorization", auth_value);

        // Bounded handshake: tungstenite has no native timeout, so a
        // half-open network would otherwise hang the warmer thread
        // for ~120 s of OS TCP timeout. See `OPEN_TIMEOUT` rationale.
        let connect = tokio::time::timeout(OPEN_TIMEOUT, connect_async(request));
        let (ws, _resp) = match connect.await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                // A deactivated/revoked key fails the WebSocket upgrade with
                // an HTTP 401/403 response. Split that out into the actionable
                // `DeepgramKeyRejected` kind (silent notification → API Keys)
                // so the user gets a "your key was rejected" prompt instead of
                // a generic "connection dropped" HUD chip. Any other handshake
                // failure (network, TLS, timeout) stays `ConnectionFailed`.
                if let tokio_tungstenite::tungstenite::Error::Http(resp) = &e {
                    let status = resp.status().as_u16();
                    if status == 401 || status == 403 {
                        log::warn!(
                            target: "deepgram",
                            "Deepgram handshake rejected with HTTP {status} — treating as rejected API key"
                        );
                        return Err(MuniError::DeepgramKeyRejected);
                    }
                }
                return Err(MuniError::DeepgramConnectionFailed {
                    reason: e.to_string(),
                });
            }
            Err(_) => {
                return Err(MuniError::DeepgramConnectionFailed {
                    reason: format!("handshake timed out after {:?}", OPEN_TIMEOUT),
                })
            }
        };

        let (sink, stream) = ws.split();
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: String::new(),
            finalize_tx: None,
            last_metadata_duration: None,
            confidence_tx: None,
        }));
        let sink = Arc::new(TokioMutex::new(Some(sink)));
        let receive_handle = tokio::spawn(receive_loop(stream, state.clone()));

        // Logged at info so the dev shell shows it without enabling debug
        // verbosity. The auth token rides in the Authorization header (set
        // above) — never in the URL — so emitting the URL here cannot leak
        // the API key. Useful for confirming `keyterm=` params land in the
        // request after a config change.
        log::info!(target: "deepgram", "Deepgram WebSocket opened: {endpoint}");

        Ok(Self {
            sink,
            state,
            receive_handle: StdMutex::new(Some(receive_handle)),
            finalize_timeout: FINALIZE_TIMEOUT,
            close_grace: Duration::ZERO,
        })
    }

    /// Build a client with no live socket (`sink = None`) for unit tests.
    ///
    /// [`send_keepalive`](Self::send_keepalive) / [`send`](Self::send) fail fast
    /// with "stream closed" (so a pool liveness probe reads it as dead), while
    /// [`close`](Self::close) still honours `close_grace` — letting a test model
    /// a discarded, wedged parked socket whose teardown is slow without standing
    /// up a real TCP peer.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn disconnected_for_test(close_grace: Duration) -> Self {
        Self {
            sink: Arc::new(TokioMutex::new(None)),
            state: Arc::new(TokioMutex::new(State {
                final_transcript: String::new(),
                finalize_tx: None,
                last_metadata_duration: None,
                confidence_tx: None,
            })),
            receive_handle: StdMutex::new(None),
            finalize_timeout: FINALIZE_TIMEOUT,
            close_grace,
        }
    }

    /// Send one chunk of 16 kHz mono linear16 PCM audio.
    ///
    /// Samples are reinterpreted as little-endian bytes — matching the
    /// `encoding=linear16` query param in [`BASE_ENDPOINT`]. Returns
    /// [`MuniError::DeepgramConnectionFailed`] if the socket has been closed
    /// or if the underlying write fails.
    pub async fn send(&self, audio: &[i16]) -> Result<(), MuniError> {
        let bytes = asr_stream::i16_slice_to_le_bytes(audio);
        let mut guard = self.sink.lock().await;
        let sink = guard
            .as_mut()
            .ok_or_else(|| MuniError::DeepgramConnectionFailed {
                reason: "stream closed".into(),
            })?;
        // Bounded write: a half-open socket fills the kernel TCP
        // buffer and blocks `sink.send` for the OS TCP timeout. See
        // `SEND_TIMEOUT` rationale.
        asr_stream::send_frame_timed(
            sink,
            Message::Binary(bytes),
            SEND_TIMEOUT,
            "send",
            deepgram_connection_failed,
        )
        .await
    }

    /// Send a `KeepAlive` control message — resets Deepgram's idle timeout
    /// without finalizing or modifying the transcript. Used by the
    /// orchestrator's pre-warming pool to keep an unused parked socket alive
    /// past Deepgram's ~10–15 s idle close.
    ///
    /// Wire format mirrors the documented Deepgram API:
    /// `{"type":"KeepAlive"}` as a Text frame.
    pub async fn send_keepalive(&self) -> Result<(), MuniError> {
        let mut guard = self.sink.lock().await;
        let sink = guard
            .as_mut()
            .ok_or_else(|| MuniError::DeepgramConnectionFailed {
                reason: "stream closed".into(),
            })?;
        // Same bounded-write rationale as `send`: a hung kernel
        // buffer would otherwise pin the pool's liveness probe.
        asr_stream::send_frame_timed(
            sink,
            Message::Text(r#"{"type":"KeepAlive"}"#.to_string()),
            SEND_TIMEOUT,
            "keepalive",
            deepgram_connection_failed,
        )
        .await
    }

    /// Send `Finalize` — forces Deepgram to emit any buffered audio as a
    /// final `Results` message immediately, without closing the WebSocket.
    ///
    /// Used by the feature-019 release path
    /// ([`crate::session::DictationSession::finalize_auto_detect`]) to drain
    /// pending Tagalog finals before aborting the confidence-trigger task.
    /// Without this, audio held by Deepgram waiting for an
    /// `endpointing=300` silence gap that never came (e.g. the user
    /// released the hotkey immediately after their last syllable) would
    /// only flush when `CloseStream` lands later in the finalize path —
    /// by which point the trigger is already aborted and the route is
    /// committed.
    ///
    /// Bounded by [`SEND_TIMEOUT`] for the same half-open-socket reasons
    /// as [`Self::send`].
    pub async fn flush(&self) -> Result<(), MuniError> {
        let mut guard = self.sink.lock().await;
        let sink = guard
            .as_mut()
            .ok_or_else(|| MuniError::DeepgramConnectionFailed {
                reason: "stream closed".into(),
            })?;
        asr_stream::send_frame_timed(
            sink,
            Message::Text(FINALIZE_PAYLOAD.to_string()),
            SEND_TIMEOUT,
            "Finalize send",
            deepgram_connection_failed,
        )
        .await
    }

    /// Send `CloseStream` and await the final transcript.
    ///
    /// Resolves when the receive task observes the corresponding `Metadata`
    /// reply, an `Error` message, or the socket disconnects mid-flight. On a
    /// mid-flight disconnect with non-empty accumulated transcript, mirrors
    /// Swift v1 by returning the partial transcript instead of an error.
    ///
    /// Bounded by [`FINALIZE_TIMEOUT`] — a half-open socket (network
    /// dropped after the press began) would otherwise leave the
    /// receive task waiting on a Metadata frame that will never
    /// arrive, hanging the orchestrator's `handle_hotkey_released`.
    pub async fn finalize(&self) -> Result<FinalizeOutcome, MuniError> {
        let rx = self.install_finalize_continuation().await?;
        // Send `CloseStream`. On send FAILURE (wedged sink / dead socket)
        // do NOT propagate with `?` — that would leave our continuation
        // installed (refusing the next finalize) and discard finals
        // Deepgram already streamed. Instead fall through to the same
        // recovery the timeout branch uses: recover the accumulated
        // partial, else surface the send error. Always clears `finalize_tx`.
        if let Err(send_err) = self.send_close_stream().await {
            log::warn!(
                target: "deepgram",
                "CloseStream send failed ({}) — recovering accumulated finals",
                send_err.user_message()
            );
            return self.recover_partial_on_finalize_failure(send_err).await;
        }
        match tokio::time::timeout(self.finalize_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(MuniError::DeepgramInvalidResponse),
            Err(_) => {
                // Timeout: the `Metadata` ack never arrived. Rather than
                // discard the whole press, recover whatever `is_final`
                // Results Deepgram already streamed — same empty-vs-nonempty
                // policy as `resolve_finalize_on_disconnect`.
                //
                // Race acceptance: if Metadata/disconnect resolved in the
                // gap between the timer firing and us locking state, the
                // transcript is already drained → recovery returns the
                // timeout Err below — identical to today.
                self.recover_partial_on_finalize_failure(MuniError::DeepgramConnectionFailed {
                    reason: format!("finalize timed out after {:?}", self.finalize_timeout),
                })
                .await
            }
        }
    }

    /// Recover the accumulated transcript after a finalize handshake failure
    /// (CloseStream send error or Metadata-ack timeout). Always clears
    /// `finalize_tx` (a continuation that will never resolve must not linger)
    /// and returns [`FinalizeOutcome::Partial`] when finals accumulated, else
    /// `fallback_err`.
    async fn recover_partial_on_finalize_failure(
        &self,
        fallback_err: MuniError,
    ) -> Result<FinalizeOutcome, MuniError> {
        let mut s = self.state.lock().await;
        let s = &mut *s;
        asr_stream::recover_partial(&mut s.finalize_tx, &mut s.final_transcript, || fallback_err)
    }

    /// Override the finalize timeout. Test-only: lets the integration
    /// suite drive the timeout→partial path in tens of milliseconds
    /// instead of the production 3 s. Gated behind `test-fixtures` (the
    /// feature the `deepgram_mock` integration test is built with) so it
    /// is never compiled into release bundles.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn set_finalize_timeout(&mut self, d: Duration) {
        self.finalize_timeout = d;
    }

    /// Inject an artificial [`close`](Self::close) delay so tests can model a
    /// wedged half-open socket's `CLOSE_TIMEOUT` stall (see `close_grace`).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn set_close_grace(&mut self, d: Duration) {
        self.close_grace = d;
    }

    /// Return the most recent `Metadata.duration` value reported by
    /// the server (in seconds), or `None` if no Metadata frame has
    /// arrived (early-error close, mid-flight disconnect, etc.).
    ///
    /// Cost-tracking (feature 005) calls this after `finalize()`
    /// returns Ok to record exact billed audio seconds for the
    /// Deepgram line. When the value is missing the caller falls
    /// back to forwarder-tracked sample count and tags the row
    /// `status="partial"`.
    pub async fn last_metadata_duration(&self) -> Option<f64> {
        self.state.lock().await.last_metadata_duration
    }

    /// Tear down the WebSocket. Idempotent.
    ///
    /// After `close()`, `send`/`finalize` will return
    /// [`MuniError::DeepgramConnectionFailed`].
    pub async fn close(&self) {
        // Test seam only (production `close_grace` is ZERO): models the
        // `CLOSE_TIMEOUT` stall of a wedged half-open socket so callers can
        // prove they don't block on a discarded client's teardown.
        if !self.close_grace.is_zero() {
            tokio::time::sleep(self.close_grace).await;
        }
        let sink = self.sink.lock().await.take();
        if let Some(mut s) = sink {
            // Bounded close: we don't care if the close frame
            // actually lands — we're tearing down anyway. Capping
            // the wait at `CLOSE_TIMEOUT` keeps a half-open socket
            // from stalling press teardown.
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
            let _ = tx.send(Err(MuniError::DeepgramConnectionFailed {
                reason: "client closed".into(),
            }));
        }
        // Drop the confidence sender so a feature-019 trigger task
        // blocked on `recv()` exits cleanly when its press tears down.
        state.confidence_tx.take();
        log::info!(target: "deepgram", "Deepgram WebSocket closed");
    }

    async fn install_finalize_continuation(
        &self,
    ) -> Result<oneshot::Receiver<Result<FinalizeOutcome, MuniError>>, MuniError> {
        let (tx, rx) = oneshot::channel();
        let mut state = self.state.lock().await;
        if state.finalize_tx.is_some() {
            // Two concurrent finalize() calls — the second is the bug. Refuse
            // it cleanly so the first call's continuation is preserved.
            return Err(MuniError::DeepgramInvalidResponse);
        }
        state.finalize_tx = Some(tx);
        Ok(rx)
    }

    /// Install a per-press confidence continuation. The receive loop will
    /// forward every `is_final=true` `Results` message's
    /// `channel.alternatives[0].confidence` to `tx` via `try_send`
    /// (non-blocking; drops on full).
    ///
    /// Idempotent — re-installing replaces the previous sender. Mirrors
    /// the shape of [`install_finalize_continuation`] but for the
    /// feature-019 confidence-trigger task's lifecycle.
    pub async fn install_confidence_continuation(&self, tx: mpsc::Sender<ChunkConfidence>) {
        let mut state = self.state.lock().await;
        state.confidence_tx = Some(tx);
    }

    async fn send_close_stream(&self) -> Result<(), MuniError> {
        let mut guard = self.sink.lock().await;
        let sink = guard
            .as_mut()
            .ok_or_else(|| MuniError::DeepgramConnectionFailed {
                reason: "stream closed".into(),
            })?;
        asr_stream::send_frame_timed(
            sink,
            Message::Text(CLOSE_STREAM_PAYLOAD.to_string()),
            SEND_TIMEOUT,
            "CloseStream send",
            deepgram_connection_failed,
        )
        .await
    }
}

/// Build a [`MuniError::DeepgramConnectionFailed`] from a reason string —
/// the `map_err` hook handed to [`asr_stream::send_frame_timed`].
fn deepgram_connection_failed(reason: String) -> MuniError {
    MuniError::DeepgramConnectionFailed { reason }
}

impl Drop for DeepgramClient {
    fn drop(&mut self) {
        // Best-effort: abort the receive task so it doesn't outlive the
        // client. Async work (graceful close) belongs in `close()` and is the
        // caller's responsibility.
        let handle = match self.receive_handle.lock() {
            Ok(mut g) => g.take(),
            Err(p) => p.into_inner().take(),
        };
        if let Some(h) = handle {
            h.abort();
        }
    }
}

// -- receive loop -----------------------------------------------------------

async fn receive_loop(mut stream: WsRead, state: Arc<TokioMutex<State>>) {
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                if let Some(parsed) = parse_message(&text) {
                    apply_message(parsed, &state).await;
                }
            }
            Ok(Message::Binary(bytes)) => {
                // Defensive: Deepgram sends JSON as text frames in practice,
                // but the Swift client decoded both. Treat as UTF-8 text.
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    if let Some(parsed) = parse_message(text) {
                        apply_message(parsed, &state).await;
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => continue, // Ping/Pong/Frame are fine to ignore.
            Err(e) => {
                resolve_finalize_on_disconnect(&state, e.to_string()).await;
                return;
            }
        }
    }
    // Clean EOF (server closed without a `Metadata` we hadn't already
    // delivered): treat the same as a transport error so a stuck `finalize()`
    // doesn't hang.
    resolve_finalize_on_disconnect(&state, "stream ended".to_string()).await;
}

async fn apply_message(parsed: ParsedMessage, state: &Arc<TokioMutex<State>>) {
    match parsed {
        ParsedMessage::Results {
            is_final,
            transcript,
            confidence,
            words_in_chunk,
        } => {
            // Forward per-chunk confidence (feature 019) to the trigger
            // task before the transcript-empty short-circuit. Empty
            // finals still carry a confidence signal worth observing,
            // but only when the field is present (schema drift defense)
            // and `is_final` (interim chunks would corrupt the trigger's
            // consecutive-low counter).
            if is_final {
                // Per-final confidence visibility for ongoing
                // observability. DEBUG-level so it's silent in the
                // default INFO output but available via
                // `RUST_LOG=muni_lib::deepgram=debug` when
                // troubleshooting confidence-trigger behavior. Was
                // briefly at INFO during 2026-05-15 K/drain tuning;
                // down-levelled once tuning landed (the
                // `confidence trigger counter` line in session.rs is
                // similarly down-levelled).
                log::debug!(
                    target: "deepgram",
                    "is_final chunk: confidence={} words={} transcript={:?}",
                    confidence
                        .map(|c| format!("{c:.3}"))
                        .unwrap_or_else(|| "<absent>".to_string()),
                    words_in_chunk,
                    transcript
                );
                if let Some(conf) = confidence {
                    forward_chunk_confidence(state, conf, words_in_chunk).await;
                }
            }
            if !is_final || transcript.is_empty() {
                return;
            }
            let mut s = state.lock().await;
            if !s.final_transcript.is_empty() {
                s.final_transcript.push(' ');
            }
            s.final_transcript.push_str(&transcript);
            log::debug!(target: "deepgram", "accumulated final transcript ({} chars)", s.final_transcript.len());
        }
        ParsedMessage::Metadata { duration } => {
            let mut s = state.lock().await;
            // Always cache the duration (even on a fresh press where
            // no caller is awaiting finalize yet) so any later reader
            // — `last_metadata_duration()` — sees the most recent
            // server-reported value rather than a stale `None`.
            if duration.is_some() {
                s.last_metadata_duration = duration;
            }
            // Only drain the accumulated transcript when a `finalize()` is
            // actually waiting for it. A server-initiated Metadata frame
            // mid-press (before the user releases) would otherwise `mem::take`
            // the finals and throw them away, so the eventual finalize would
            // recover nothing. With the guard, mid-press Metadata just caches
            // the duration and leaves the finals intact for the real finalize
            // (or the disconnect-recovery path).
            if let Some(tx) = s.finalize_tx.take() {
                let transcript = std::mem::take(&mut s.final_transcript);
                let _ = tx.send(Ok(FinalizeOutcome::Complete(transcript)));
            }
        }
        ParsedMessage::ServerError(message) => {
            let mut s = state.lock().await;
            if let Some(tx) = s.finalize_tx.take() {
                let _ = tx.send(Err(MuniError::DeepgramServerError { message }));
            }
        }
        ParsedMessage::Ignored => {}
    }
}

/// Non-blocking send of a `ChunkConfidence` to whatever continuation
/// the session layer has installed (feature 019). Cheap when no
/// continuation is installed — just a lock acquire and a noop. The
/// sender is cloned out of the lock so the actual `try_send` happens
/// without holding the state mutex (mpsc::Sender is cheap to clone —
/// it bumps a refcount).
async fn forward_chunk_confidence(
    state: &Arc<TokioMutex<State>>,
    confidence: f32,
    words_in_chunk: usize,
) {
    let sender = {
        let s = state.lock().await;
        s.confidence_tx.clone()
    };
    let Some(tx) = sender else { return };
    let event = ChunkConfidence {
        confidence,
        words_in_chunk,
        is_final: true,
    };
    match tx.try_send(event) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            log::warn!(
                target: "deepgram",
                "confidence_tx full — dropping chunk confidence (session is lagging)"
            );
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            // Receiver has been dropped — the trigger task exited
            // cleanly (e.g. finalize already won the race). Silent.
        }
    }
}

async fn resolve_finalize_on_disconnect(state: &Arc<TokioMutex<State>>, reason: String) {
    let mut s = state.lock().await;
    let s = &mut *s;
    asr_stream::resolve_disconnect(&mut s.finalize_tx, &mut s.final_transcript, || {
        MuniError::DeepgramConnectionFailed { reason }
    });
}

// -- pure parser ------------------------------------------------------------

/// Parsed shape of a Deepgram server message — only the fields we actually
/// consume. Returning a typed enum keeps the receive-loop body tiny and lets
/// us unit-test parsing without spinning up a WebSocket.
#[derive(Debug, PartialEq)]
enum ParsedMessage {
    Results {
        is_final: bool,
        transcript: String,
        /// `channel.alternatives[0].confidence` — `None` when the field
        /// is absent (schema drift defense). Forwarded to the feature-019
        /// confidence-trigger task when present and `is_final=true`.
        confidence: Option<f32>,
        /// Number of word objects in `channel.alternatives[0].words`.
        /// Count only — the words themselves are not consumed in v1.
        words_in_chunk: usize,
    },
    /// `duration` is the Deepgram-reported audio seconds for the
    /// session; absent on early-error closes that emit a Metadata frame
    /// without the field. Feature 005's cost path reads this when
    /// recording the `api_calls.audio_seconds` for the Deepgram line.
    Metadata {
        duration: Option<f64>,
    },
    ServerError(String),
    /// `UtteranceEnd`, `SpeechStarted`, or any unknown `"type"` we don't act
    /// on. Distinct from `None` (failed to parse JSON).
    Ignored,
}

fn parse_message(text: &str) -> Option<ParsedMessage> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let obj = value.as_object()?;
    let msg_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match msg_type {
        "Results" => {
            let is_final = obj
                .get("is_final")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let first_alt = obj
                .get("channel")
                .and_then(|c| c.get("alternatives"))
                .and_then(|a| a.as_array())
                .and_then(|arr| arr.first());
            let transcript = first_alt
                .and_then(|first| first.get("transcript"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            // Per-chunk confidence (feature 019). `None` when absent so
            // the receive loop knows to skip forwarding — never panic
            // on schema drift.
            let confidence = first_alt
                .and_then(|first| first.get("confidence"))
                .and_then(|v| v.as_f64())
                .map(|f| f as f32);
            let words_in_chunk = first_alt
                .and_then(|first| first.get("words"))
                .and_then(|w| w.as_array())
                .map(|arr| arr.len())
                .unwrap_or(0);
            Some(ParsedMessage::Results {
                is_final,
                transcript,
                confidence,
                words_in_chunk,
            })
        }
        "Metadata" => {
            // Deepgram emits `duration` (seconds, float) at the root
            // of the Metadata envelope. Field is omitted on early
            // error-close metadata; feature 005 falls back to
            // forwarder-tracked sample count in that case.
            let duration = obj.get("duration").and_then(|v| v.as_f64());
            Some(ParsedMessage::Metadata { duration })
        }
        "Error" => {
            let description = obj
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Deepgram error")
                .to_string();
            Some(ParsedMessage::ServerError(description))
        }
        // Known events that carry no payload of interest to us.
        "UtteranceEnd" | "SpeechStarted" => Some(ParsedMessage::Ignored),
        // Unknown but well-formed JSON — let it pass quietly.
        _ => Some(ParsedMessage::Ignored),
    }
}

// -- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_message_is_parsed_with_is_final_and_transcript() {
        let json = r#"{
            "type": "Results",
            "is_final": true,
            "channel": {
                "alternatives": [
                    { "transcript": "hello world", "confidence": 0.99 }
                ]
            }
        }"#;
        let parsed = parse_message(json).expect("parses");
        assert_eq!(
            parsed,
            ParsedMessage::Results {
                is_final: true,
                transcript: "hello world".into(),
                confidence: Some(0.99),
                words_in_chunk: 0,
            }
        );
    }

    #[test]
    fn results_with_missing_alternatives_returns_empty_transcript() {
        let json = r#"{ "type": "Results", "is_final": false, "channel": { "alternatives": [] } }"#;
        let parsed = parse_message(json).expect("parses");
        assert_eq!(
            parsed,
            ParsedMessage::Results {
                is_final: false,
                transcript: String::new(),
                confidence: None,
                words_in_chunk: 0,
            }
        );
    }

    #[test]
    fn results_message_extracts_confidence_and_words_count() {
        let json = r#"{
            "type": "Results",
            "is_final": true,
            "channel": {
                "alternatives": [
                    {
                        "transcript": "hello world",
                        "confidence": 0.42,
                        "words": [
                            {"word": "hello", "start": 0.0, "end": 0.5, "confidence": 0.9},
                            {"word": "world", "start": 0.5, "end": 1.0, "confidence": 0.4}
                        ]
                    }
                ]
            }
        }"#;
        let parsed = parse_message(json).expect("parses");
        assert_eq!(
            parsed,
            ParsedMessage::Results {
                is_final: true,
                transcript: "hello world".into(),
                confidence: Some(0.42),
                words_in_chunk: 2,
            }
        );
    }

    #[test]
    fn results_message_without_confidence_field_returns_none() {
        // Schema-drift defense: never panic when Deepgram omits the
        // field. Receive loop simply skips forwarding to the trigger.
        let json = r#"{
            "type": "Results",
            "is_final": true,
            "channel": { "alternatives": [{ "transcript": "hi" }] }
        }"#;
        let parsed = parse_message(json).expect("parses");
        assert_eq!(
            parsed,
            ParsedMessage::Results {
                is_final: true,
                transcript: "hi".into(),
                confidence: None,
                words_in_chunk: 0,
            }
        );
    }

    #[test]
    fn metadata_message_is_recognized() {
        let json = r#"{ "type": "Metadata", "request_id": "abc" }"#;
        assert_eq!(
            parse_message(json),
            Some(ParsedMessage::Metadata { duration: None })
        );
    }

    #[test]
    fn metadata_message_round_trips_duration() {
        // Deepgram emits the audio seconds as a top-level `duration`
        // float. Cost tracking (feature 005) reads this to bill the
        // Deepgram line.
        let json = r#"{ "type": "Metadata", "duration": 12.34, "request_id": "abc" }"#;
        assert_eq!(
            parse_message(json),
            Some(ParsedMessage::Metadata {
                duration: Some(12.34)
            })
        );
    }

    #[test]
    fn error_message_extracts_description_or_falls_back() {
        let with_desc = r#"{ "type": "Error", "description": "Quota exhausted" }"#;
        assert_eq!(
            parse_message(with_desc),
            Some(ParsedMessage::ServerError("Quota exhausted".into()))
        );
        let without_desc = r#"{ "type": "Error" }"#;
        assert_eq!(
            parse_message(without_desc),
            Some(ParsedMessage::ServerError("Unknown Deepgram error".into()))
        );
    }

    #[test]
    fn utterance_end_and_speech_started_are_ignored() {
        assert_eq!(
            parse_message(r#"{ "type": "UtteranceEnd" }"#),
            Some(ParsedMessage::Ignored)
        );
        assert_eq!(
            parse_message(r#"{ "type": "SpeechStarted" }"#),
            Some(ParsedMessage::Ignored)
        );
    }

    #[test]
    fn unknown_type_is_ignored_not_dropped() {
        assert_eq!(
            parse_message(r#"{ "type": "FutureEvent" }"#),
            Some(ParsedMessage::Ignored)
        );
    }

    #[test]
    fn invalid_json_returns_none() {
        assert!(parse_message("not json").is_none());
    }

    #[tokio::test]
    async fn apply_message_only_accumulates_final_results() {
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: String::new(),
            finalize_tx: None,
            last_metadata_duration: None,
            confidence_tx: None,
        }));

        // Interim should be ignored even with a non-empty transcript.
        apply_message(
            ParsedMessage::Results {
                is_final: false,
                transcript: "interim text".into(),
                confidence: None,
                words_in_chunk: 0,
            },
            &state,
        )
        .await;

        // First final.
        apply_message(
            ParsedMessage::Results {
                is_final: true,
                transcript: "hello".into(),
                confidence: Some(0.9),
                words_in_chunk: 1,
            },
            &state,
        )
        .await;

        // Second final — should be space-separated.
        apply_message(
            ParsedMessage::Results {
                is_final: true,
                transcript: "world".into(),
                confidence: Some(0.9),
                words_in_chunk: 1,
            },
            &state,
        )
        .await;

        assert_eq!(state.lock().await.final_transcript, "hello world");
    }

    #[tokio::test]
    async fn apply_message_forwards_chunk_confidence_when_installed() {
        let (tx, mut rx) = mpsc::channel::<ChunkConfidence>(8);
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: String::new(),
            finalize_tx: None,
            last_metadata_duration: None,
            confidence_tx: Some(tx),
        }));

        apply_message(
            ParsedMessage::Results {
                is_final: true,
                transcript: "hello".into(),
                confidence: Some(0.42),
                words_in_chunk: 2,
            },
            &state,
        )
        .await;

        let event = rx.recv().await.expect("event delivered");
        assert_eq!(event.confidence, 0.42);
        assert_eq!(event.words_in_chunk, 2);
        assert!(event.is_final);
    }

    #[tokio::test]
    async fn apply_message_skips_confidence_forward_when_field_missing() {
        let (tx, mut rx) = mpsc::channel::<ChunkConfidence>(8);
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: String::new(),
            finalize_tx: None,
            last_metadata_duration: None,
            confidence_tx: Some(tx),
        }));

        apply_message(
            ParsedMessage::Results {
                is_final: true,
                transcript: "hi".into(),
                confidence: None,
                words_in_chunk: 0,
            },
            &state,
        )
        .await;

        // No event should be queued.
        assert!(rx.try_recv().is_err(), "no confidence event expected");
    }

    #[tokio::test]
    async fn apply_message_does_not_forward_interim_confidence() {
        // Defensive: today `interim_results=false` is set in BASE_ENDPOINT,
        // but if that ever flips we must not corrupt the trigger counter.
        let (tx, mut rx) = mpsc::channel::<ChunkConfidence>(8);
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: String::new(),
            finalize_tx: None,
            last_metadata_duration: None,
            confidence_tx: Some(tx),
        }));

        apply_message(
            ParsedMessage::Results {
                is_final: false,
                transcript: "interim".into(),
                confidence: Some(0.1),
                words_in_chunk: 1,
            },
            &state,
        )
        .await;

        assert!(rx.try_recv().is_err(), "interim event must not forward");
    }

    #[tokio::test]
    async fn apply_metadata_resolves_pending_finalize_with_accumulated_transcript() {
        let (tx, rx) = oneshot::channel();
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: "the quick fox".into(),
            finalize_tx: Some(tx),
            last_metadata_duration: None,
            confidence_tx: None,
        }));
        apply_message(
            ParsedMessage::Metadata {
                duration: Some(2.5),
            },
            &state,
        )
        .await;
        let result = rx.await.expect("oneshot delivers");
        assert_eq!(
            result.unwrap(),
            FinalizeOutcome::Complete("the quick fox".into())
        );
        // State is reset so a follow-up press starts clean.
        let s = state.lock().await;
        assert!(s.final_transcript.is_empty());
        assert!(s.finalize_tx.is_none());
        // Duration was cached for the cost-tracking accessor.
        assert_eq!(s.last_metadata_duration, Some(2.5));
    }

    #[tokio::test]
    async fn apply_metadata_without_pending_finalize_preserves_finals() {
        // Task 6b: a server-initiated Metadata frame mid-press (no finalize
        // waiting) must cache the duration but NOT drain the accumulated
        // finals — the eventual finalize (or disconnect recovery) still needs
        // them. Pre-fix this `mem::take`d the transcript and lost the press.
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: "kept across metadata".into(),
            finalize_tx: None,
            last_metadata_duration: None,
            confidence_tx: None,
        }));
        apply_message(
            ParsedMessage::Metadata {
                duration: Some(3.0),
            },
            &state,
        )
        .await;
        let s = state.lock().await;
        assert_eq!(s.final_transcript, "kept across metadata");
        assert_eq!(s.last_metadata_duration, Some(3.0));
    }

    #[tokio::test]
    async fn apply_server_error_resolves_pending_finalize_with_typed_error() {
        let (tx, rx) = oneshot::channel();
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: String::new(),
            finalize_tx: Some(tx),
            last_metadata_duration: None,
            confidence_tx: None,
        }));
        apply_message(ParsedMessage::ServerError("rate limited".into()), &state).await;
        let err = rx.await.expect("oneshot delivers").unwrap_err();
        match err {
            MuniError::DeepgramServerError { message } => assert_eq!(message, "rate limited"),
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
            confidence_tx: None,
        }));
        resolve_finalize_on_disconnect(&state, "io error".into()).await;
        let result = rx.await.expect("oneshot delivers").unwrap();
        assert_eq!(result, FinalizeOutcome::Partial("hello".into()));
    }

    #[tokio::test]
    async fn disconnect_with_no_transcript_returns_connection_failed() {
        let (tx, rx) = oneshot::channel();
        let state = Arc::new(TokioMutex::new(State {
            final_transcript: String::new(),
            finalize_tx: Some(tx),
            last_metadata_duration: None,
            confidence_tx: None,
        }));
        resolve_finalize_on_disconnect(&state, "io error".into()).await;
        let err = rx.await.expect("oneshot delivers").unwrap_err();
        assert!(matches!(err, MuniError::DeepgramConnectionFailed { .. }));
    }

    #[tokio::test]
    async fn open_with_empty_api_key_returns_missing_error() {
        match DeepgramClient::open("").await {
            Err(MuniError::DeepgramMissingApiKey) => {}
            Err(other) => panic!("expected DeepgramMissingApiKey, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn build_endpoint_with_empty_keyterms_returns_base_unchanged() {
        let url = build_endpoint(BASE_ENDPOINT, &[]);
        assert_eq!(url, BASE_ENDPOINT);
    }

    #[test]
    fn parse_deepgram_endpoint_falls_back_to_default_when_blank() {
        assert_eq!(parse_deepgram_endpoint(None), default_endpoint());
        assert_eq!(
            parse_deepgram_endpoint(Some(String::new())),
            default_endpoint()
        );
        assert_eq!(
            parse_deepgram_endpoint(Some("   ".into())),
            default_endpoint()
        );
    }

    #[test]
    fn parse_deepgram_endpoint_uses_override_verbatim_trimmed() {
        // An override is taken as-is (no keyterms appended) — the caller is
        // pointing at a mock or, for dogfood, an unreachable host.
        let override_url = "wss://127.0.0.1:1/v1/listen";
        assert_eq!(
            parse_deepgram_endpoint(Some(format!("  {override_url}  "))),
            override_url
        );
    }

    #[test]
    fn build_endpoint_appends_single_word_keyterm_unencoded() {
        let url = build_endpoint("wss://x/?a=1", &["Muni"]);
        assert_eq!(url, "wss://x/?a=1&keyterm=Muni");
    }

    #[test]
    fn build_endpoint_percent_encodes_spaces_in_multi_word_keyterm() {
        // Deepgram accepts both "%20" and "+" for spaces; we standardize on
        // %20 (RFC 3986 percent-encoding) so the URL also round-trips through
        // strict path parsers, not just form-decoders.
        let url = build_endpoint("wss://x/?a=1", &["mic test"]);
        assert_eq!(url, "wss://x/?a=1&keyterm=mic%20test");
    }

    #[test]
    fn build_endpoint_emits_repeated_keyterm_param_for_each_entry() {
        let url = build_endpoint("wss://x/?a=1", &["mic test", "mic check", "Muni"]);
        assert_eq!(
            url,
            "wss://x/?a=1&keyterm=mic%20test&keyterm=mic%20check&keyterm=Muni"
        );
    }

    #[test]
    fn build_endpoint_encodes_punctuation_outside_unreserved_set() {
        // Apostrophes, ampersands, and equals signs must not leak into the
        // URL unescaped — an unescaped `&` would split the keyterm into two
        // bogus query params, an unescaped `=` would corrupt the key/value
        // boundary. Cover them all in one term.
        let url = build_endpoint("wss://x/?a=1", &["it's a&b=c"]);
        assert_eq!(url, "wss://x/?a=1&keyterm=it%27s%20a%26b%3Dc");
    }

    #[test]
    fn build_endpoint_skips_empty_and_whitespace_only_keyterms() {
        // Lets future user-extension lists pass through without a sanitise
        // pass — a stray empty entry would otherwise emit `&keyterm=` which
        // Deepgram would treat as an empty term.
        let url = build_endpoint("wss://x/?a=1", &["", "   ", "Muni", "\t"]);
        assert_eq!(url, "wss://x/?a=1&keyterm=Muni");
    }

    #[test]
    fn default_endpoint_includes_base_and_all_default_keyterms() {
        let url = default_endpoint();
        assert!(
            url.starts_with(BASE_ENDPOINT),
            "default endpoint should extend the base, got: {url}"
        );
        for term in DEFAULT_KEYTERMS {
            // Each default term must appear at least once as a keyterm
            // value — encoded form, since multi-word terms have %20 in them.
            let needle_full = build_endpoint("", &[term]);
            assert!(
                url.contains(&needle_full),
                "default endpoint missing keyterm {term:?}: {url}"
            );
        }
    }

    #[test]
    fn default_endpoint_default_keyterms_are_well_formed() {
        // Guard against accidentally adding an empty/whitespace entry to
        // DEFAULT_KEYTERMS — those would silently no-op (per
        // build_endpoint_skips_empty…) but indicate a list-edit mistake.
        for term in DEFAULT_KEYTERMS {
            let trimmed = term.trim();
            assert!(!trimmed.is_empty(), "empty default keyterm");
            assert_eq!(
                *term, trimmed,
                "default keyterm has stray whitespace: {term:?}"
            );
        }
    }
}
