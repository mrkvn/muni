//! Shared streaming-ASR client core (plan 039 slice 3).
//!
//! `deepgram.rs` and `gladia.rs` are two push-to-talk WebSocket clients with
//! the same single-shot shape: open → stream binary PCM frames → `finalize()`
//! sends a control frame and awaits a terminal envelope, recovering whatever
//! `is_final` text accumulated if the handshake fails mid-flight. Rather than
//! keep the finalize-recovery policy, the PCM encoder, the timed-send wrapper
//! and the log-truncation helper duplicated (and drifting) across both files
//! plus `parakeet.rs` / `groq*.rs`, the genuinely shared primitives live here.
//!
//! Deliberately *not* here (yet): a single `State` struct or a codec trait
//! that turns both clients into thin wrappers. The two providers still carry
//! provider-specific state (Deepgram's per-chunk confidence continuation,
//! Gladia's audio-ack flag) and control-frame shapes, so forcing a shared
//! `State` would trade real coupling for cosmetic dedup. The behavioural
//! contract (`FinalizeOutcome`, disconnect resolution) *is* unified, which is
//! what the finalize-recovery fixes in the same slice depend on.

use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::SinkExt;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::MuniError;

/// Concrete WebSocket stream type shared by every streaming ASR client.
pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
/// Write half of a [`WsStream`] after `split()`.
pub type WsSink = SplitSink<WsStream, Message>;
/// Read half of a [`WsStream`] after `split()`.
pub type WsRead = SplitStream<WsStream>;

/// Outcome of a successful streaming-ASR `finalize()` call.
///
/// `Complete` is the happy path: the server acked the finalize control frame
/// with its terminal envelope, so the accumulated transcript is the full
/// press. `Partial` is a degraded success: the finalize handshake failed
/// (control-frame send error, timeout, or mid-flight disconnect) but the
/// provider had already streamed `is_final` fragments during the press, so we
/// recover that accumulated text rather than discard the whole press. The
/// session layer pastes a `Partial` without auto-submitting and tags it for
/// history/telemetry as a degraded success.
#[derive(Debug, Clone, PartialEq)]
pub enum FinalizeOutcome {
    Complete(String),
    Partial(String),
}

impl FinalizeOutcome {
    /// Borrow the recovered/complete transcript, regardless of degraded-ness.
    pub fn text(&self) -> &str {
        match self {
            FinalizeOutcome::Complete(s) | FinalizeOutcome::Partial(s) => s,
        }
    }

    /// Consume into the owned transcript string, regardless of degraded-ness.
    /// The rescue caller only wants the text — it decides degraded handling
    /// from the variant separately via [`FinalizeOutcome::is_partial`].
    pub fn into_text(self) -> String {
        match self {
            FinalizeOutcome::Complete(s) | FinalizeOutcome::Partial(s) => s,
        }
    }

    /// `true` when the transcript was recovered from a failed finalize
    /// handshake (possibly truncated) rather than a clean terminal ack.
    pub fn is_partial(&self) -> bool {
        matches!(self, FinalizeOutcome::Partial(_))
    }
}

/// Ergonomic comparison against an expected transcript string. Lets tests and
/// callers assert on the text without unwrapping the variant when the
/// degraded flag is not what's under test.
impl PartialEq<&str> for FinalizeOutcome {
    fn eq(&self, other: &&str) -> bool {
        self.text() == *other
    }
}

/// Sender half installed by `finalize()` and consumed by the receive loop when
/// the terminal envelope / error / disconnect arrives. Shared type so the
/// disconnect + recovery helpers below can be written once for both clients.
pub type FinalizeSender = oneshot::Sender<Result<FinalizeOutcome, MuniError>>;
/// Receiver half returned from `install_finalize_continuation`.
pub type FinalizeReceiver = oneshot::Receiver<Result<FinalizeOutcome, MuniError>>;

/// Resolve a pending `finalize()` when the receive loop observes the socket
/// disconnect (clean EOF or transport error). Mirrors Swift v1: prefer
/// returning whatever the user actually said (as a [`FinalizeOutcome::Partial`])
/// over a bare network error when a transcript is already in hand; only
/// surface `empty_err` when nothing was transcribed.
///
/// Sync (the caller already holds the state lock and passes the two fields by
/// mutable ref) so the same body serves both providers despite their otherwise
/// divergent `State` layouts. No-op when no `finalize()` is pending.
pub fn resolve_disconnect(
    finalize_tx: &mut Option<FinalizeSender>,
    final_transcript: &mut String,
    empty_err: impl FnOnce() -> MuniError,
) {
    let Some(tx) = finalize_tx.take() else {
        return;
    };
    let transcript = std::mem::take(final_transcript);
    let result = if transcript.is_empty() {
        Err(empty_err())
    } else {
        Ok(FinalizeOutcome::Partial(transcript))
    };
    let _ = tx.send(result);
}

/// Recover the accumulated transcript on a finalize *failure* (control-frame
/// send error or timeout) from inside `finalize()` itself, where no oneshot
/// send is needed because the caller returns the value directly.
///
/// Always clears `finalize_tx` (a continuation that will now never resolve
/// must not be left installed — it would refuse the next press's finalize).
/// Returns [`FinalizeOutcome::Partial`] when finals accumulated, else the
/// caller-supplied `empty_err`.
pub fn recover_partial(
    finalize_tx: &mut Option<FinalizeSender>,
    final_transcript: &mut String,
    empty_err: impl FnOnce() -> MuniError,
) -> Result<FinalizeOutcome, MuniError> {
    let _ = finalize_tx.take();
    let transcript = std::mem::take(final_transcript);
    if transcript.is_empty() {
        Err(empty_err())
    } else {
        Ok(FinalizeOutcome::Partial(transcript))
    }
}

/// Send one WebSocket frame with a bounded write.
///
/// Tungstenite has no native operation timeout; a half-open socket fills the
/// kernel TCP write buffer and blocks `sink.send` for the OS TCP window
/// (~2 min on macOS). The bounded write turns that into a fast typed error so
/// the forwarder's consecutive-failure cap fires and the press unblocks.
///
/// `map_err` builds the provider-specific error kind from a reason string;
/// `label` names the frame in the timeout reason (e.g. `"CloseStream send"` →
/// `"CloseStream send timed out after 1s"`).
pub async fn send_frame_timed(
    sink: &mut WsSink,
    message: Message,
    timeout: Duration,
    label: &str,
    map_err: impl Fn(String) -> MuniError,
) -> Result<(), MuniError> {
    match tokio::time::timeout(timeout, sink.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(map_err(e.to_string())),
        Err(_) => Err(map_err(format!("{label} timed out after {timeout:?}"))),
    }
}

/// Convert `[i16]` PCM to a little-endian `Vec<u8>`.
///
/// Every streaming ASR provider we drive (Deepgram `encoding=linear16`, Gladia
/// `wav/pcm` bit_depth 16, the Parakeet sidecar wire protocol) expects the
/// same raw little-endian i16 byte layout. `i16::to_le_bytes()` guarantees the
/// endianness regardless of host architecture.
pub fn i16_slice_to_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Truncate to at most `max_bytes` bytes WITHOUT splitting a UTF-8 codepoint.
///
/// Used to bound the volume of a logged error-response body (a server can
/// reply with a verbose HTML page). Shared by the Groq chat and Groq Whisper
/// clients, which both cap their non-2xx body logs.
pub fn truncate_for_log(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i16_to_le_bytes_handles_endianness() {
        let samples: [i16; 4] = [0, 1, -1, i16::MIN];
        let bytes = i16_slice_to_le_bytes(&samples);
        assert_eq!(bytes, vec![0, 0, 1, 0, 0xFF, 0xFF, 0x00, 0x80]);
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        let s = "abcd🚀";
        let truncated = truncate_for_log(s, 6);
        assert!(truncated.is_char_boundary(truncated.len()));
        assert!(truncated.starts_with("abcd"));
        // The 4-byte rocket can't fit in the 2 bytes left after "abcd", so it
        // is dropped whole rather than split into invalid UTF-8.
        assert_eq!(truncated, "abcd");
    }

    #[test]
    fn truncate_passes_through_short_strings() {
        assert_eq!(truncate_for_log("hello", 100), "hello");
    }

    #[test]
    fn finalize_outcome_text_accessors_ignore_variant() {
        let complete = FinalizeOutcome::Complete("done".into());
        let partial = FinalizeOutcome::Partial("cut".into());
        assert_eq!(complete.text(), "done");
        assert_eq!(partial.text(), "cut");
        assert!(!complete.is_partial());
        assert!(partial.is_partial());
        assert_eq!(complete.into_text(), "done");
        assert_eq!(partial.into_text(), "cut");
    }

    #[test]
    fn finalize_outcome_compares_to_str_by_text() {
        assert_eq!(FinalizeOutcome::Complete("hi".into()), "hi");
        assert_eq!(FinalizeOutcome::Partial("hi".into()), "hi");
        assert!(FinalizeOutcome::Complete("hi".into()) != "bye");
    }

    #[test]
    fn recover_partial_returns_partial_when_finals_accumulated() {
        let (tx, _rx) = oneshot::channel();
        let mut finalize_tx = Some(tx);
        let mut transcript = String::from("hello world");
        let out = recover_partial(&mut finalize_tx, &mut transcript, || {
            MuniError::DeepgramInvalidResponse
        });
        assert_eq!(out.unwrap(), FinalizeOutcome::Partial("hello world".into()));
        // finalize_tx cleared, transcript drained.
        assert!(finalize_tx.is_none());
        assert!(transcript.is_empty());
    }

    #[test]
    fn recover_partial_returns_empty_err_when_nothing_accumulated() {
        let (tx, _rx) = oneshot::channel();
        let mut finalize_tx = Some(tx);
        let mut transcript = String::new();
        let out = recover_partial(&mut finalize_tx, &mut transcript, || {
            MuniError::DeepgramInvalidResponse
        });
        assert!(matches!(out, Err(MuniError::DeepgramInvalidResponse)));
        assert!(finalize_tx.is_none());
    }

    #[tokio::test]
    async fn resolve_disconnect_sends_partial_when_finals_present() {
        let (tx, rx) = oneshot::channel();
        let mut finalize_tx = Some(tx);
        let mut transcript = String::from("hello");
        resolve_disconnect(&mut finalize_tx, &mut transcript, || {
            MuniError::GladiaConnectionFailed {
                reason: "io".into(),
            }
        });
        assert_eq!(
            rx.await.unwrap().unwrap(),
            FinalizeOutcome::Partial("hello".into())
        );
    }

    #[tokio::test]
    async fn resolve_disconnect_sends_empty_err_when_no_finals() {
        let (tx, rx) = oneshot::channel();
        let mut finalize_tx = Some(tx);
        let mut transcript = String::new();
        resolve_disconnect(&mut finalize_tx, &mut transcript, || {
            MuniError::GladiaConnectionFailed {
                reason: "io".into(),
            }
        });
        assert!(matches!(
            rx.await.unwrap(),
            Err(MuniError::GladiaConnectionFailed { .. })
        ));
    }

    #[test]
    fn resolve_disconnect_is_noop_without_pending_finalize() {
        let mut finalize_tx: Option<FinalizeSender> = None;
        let mut transcript = String::from("kept");
        resolve_disconnect(&mut finalize_tx, &mut transcript, || {
            MuniError::DeepgramInvalidResponse
        });
        // Transcript preserved for a later finalize when nobody is waiting.
        assert_eq!(transcript, "kept");
    }
}
