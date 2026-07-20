//! Groq Whisper batch transcription client.
//!
//! Wraps a press's accumulated 16 kHz / mono / linear16 PCM into a WAV
//! container and POSTs it to Groq's OpenAI-compatible
//! `/v1/audio/transcriptions` endpoint. Used by:
//! - The auto-detect Whisper branch (LID picked Tagalog/Taglish).
//! - The text-LID path itself, which uses Whisper to transcribe each
//!   slice and feeds the *text* to a [`crate::text_lid::TextLidClassifier`]
//!   for language identification (feature 003 — replaces the old
//!   audio-LID head that misclassified accented English).
//!
//! Wire contract:
//! - URL: see [`DEFAULT_ENDPOINT`].
//! - Method: `POST`, `multipart/form-data`.
//! - Headers: `Authorization: Bearer <KEY>`.
//! - Form fields: `file=<audio.wav>`, `model=<DEFAULT_MODEL>`,
//!   `response_format=json`. Language is intentionally omitted so Whisper
//!   does its native auto-detection (the right behaviour for Taglish
//!   code-switching).
//! - Response: `{"text": "..."}`.

use std::time::Duration;

use serde::Deserialize;

use crate::asr_stream::truncate_for_log;
use crate::error::MuniError;

/// Production Groq audio-transcriptions endpoint.
pub const DEFAULT_ENDPOINT: &str = "https://api.groq.com/openai/v1/audio/transcriptions";

/// Dev-only override for the Whisper endpoint. Set to a deliberately
/// unreachable URL (e.g. `http://127.0.0.1:1/...`) to force the
/// Groq Whisper path to fail and exercise the cross-provider Gladia
/// fallback (`session.rs::attempt_gladia_fallback_transcribe`). Unset
/// in production. Intentionally narrow in scope: only this client
/// reads it — cleanup / LID Groq calls keep using their production
/// endpoints, so cleanup of the Gladia-rescued transcript still works.
pub const ENDPOINT_OVERRIDE_ENV: &str = "MUNI_GROQ_WHISPER_ENDPOINT";

/// Whisper model to drive on Groq. `whisper-large-v3-turbo` matched Wispr
/// Flow's reference output essentially verbatim in the spike at ~500 ms
/// per request — see `audio_sample/transcripts_whisper.md`. Same model
/// is used by the feature-021 audio-LID hybrid path: non-turbo
/// `whisper-large-v3` was tried in 2026-05-18 round 6 dogfood and the
/// ~2× latency cost (~700 ms median vs ~300 ms) pushed the classify
/// result past release on short presses, cancelling out its
/// transcription-accuracy gain. Keep turbo, accept occasional
/// short-slice truncation, lean on the release-trigger wait instead.
pub const DEFAULT_MODEL: &str = "whisper-large-v3-turbo";

/// PCM sample rate. Audio capture upstream already resamples to this rate
/// (see `audio::TARGET_SAMPLE_RATE`); we encode the WAV container with the
/// matching field so Groq doesn't have to guess.
pub const PCM_SAMPLE_RATE: u32 = 16_000;

/// PCM channel count — mono. Matches the audio-capture pipeline.
pub const PCM_CHANNELS: u16 = 1;

/// Bits per sample for linear16 PCM.
pub const PCM_BITS_PER_SAMPLE: u16 = 16;

/// Total request timeout. Groq Whisper turbo replies in ~400–600 ms
/// for short clips and ~1.5–2 s for the longest plausible press
/// (~30 s of audio). 5 s leaves ~2–3× headroom on the slowest
/// happy-path call without pinning the user-perceived release
/// latency to 10 s on a network outage.
///
/// Lowered from 10 s → 5 s after the 2026-05-18 round-8 dogfood
/// review. Worst-case scenario from
/// `docs/architecture/press_scenarios.md`: a Whisper outage with
/// the previous 10 s timeout produced a 10–13 s release-to-paste
/// window before the Gladia fallback even started. Cutting to 5 s
/// caps that user-visible delay at ~5–8 s and lets Gladia rescue
/// the press sooner. False positives (a legitimately slow Whisper
/// call that exceeds 5 s) trigger the Gladia fallback — same
/// transcript path, slightly higher cost on that single press, no
/// user-visible failure.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of bytes captured from a non-2xx response body before
/// truncation. Mirrors `groq.rs::MAX_ERROR_BODY_BYTES` so log volume
/// stays bounded when the server replies with a verbose HTML error page.
const MAX_ERROR_BODY_BYTES: usize = 4_096;

/// Streaming Groq Whisper transcription client.
///
/// Owns a `reqwest::Client` so the underlying TCP/TLS connection is
/// reused across presses — opening a fresh client every press would add
/// ~200 ms of TLS handshake on top of the model latency we already pay.
#[derive(Debug, Clone)]
pub struct GroqWhisperClient {
    endpoint: String,
    http: reqwest::Client,
    timeout: Duration,
}

impl GroqWhisperClient {
    /// Construct a client targeting the production Groq endpoint, unless
    /// [`ENDPOINT_OVERRIDE_ENV`] is set (dev-only escape hatch for
    /// fallback-path verification).
    pub fn new() -> Result<Self, MuniError> {
        let endpoint = std::env::var(ENDPOINT_OVERRIDE_ENV)
            .ok()
            .filter(|v| !v.is_empty())
            .inspect(|v| {
                log::warn!(
                    target: "groq_whisper",
                    "endpoint overridden via {}={} — production calls will NOT hit Groq",
                    ENDPOINT_OVERRIDE_ENV,
                    v
                );
            })
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
        Self::with_endpoint(endpoint)
    }

    /// Construct a client targeting a custom endpoint. Used by tests to
    /// point at a `wiremock` server without leaving the binary.
    pub fn with_endpoint(endpoint: String) -> Result<Self, MuniError> {
        let http =
            reqwest::Client::builder()
                .build()
                .map_err(|e| MuniError::GroqConnectionFailed {
                    reason: e.to_string(),
                })?;
        Ok(Self {
            endpoint,
            http,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Override the whole-request timeout. Test-only: lets the wiremock
    /// failure-shape suite exercise the transport-timeout path in a few
    /// hundred ms instead of the production 5 s. Gated behind `test-fixtures`
    /// so it never compiles into release bundles (mirrors
    /// `DeepgramClient::set_finalize_timeout`).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Transcribe a 16 kHz mono linear16 PCM buffer.
    ///
    /// Wraps the samples in a minimal WAV container, POSTs as
    /// `multipart/form-data`, and returns the trimmed `text` field of the
    /// JSON response. Returns an empty string when Groq replies 2xx with
    /// no content (effectively "Whisper heard silence").
    ///
    /// Errors mirror `groq.rs::GroqClient::complete`:
    /// - [`MuniError::GroqMissingApiKey`] if `api_key` is empty.
    /// - [`MuniError::GroqServerError`] for any non-2xx response.
    /// - [`MuniError::GroqConnectionFailed`] for transport/timeout errors.
    /// - [`MuniError::GroqInvalidResponse`] if the body is not parseable
    ///   as the expected `{"text": "..."}` shape.
    pub async fn transcribe(&self, samples: &[i16], api_key: &str) -> Result<String, MuniError> {
        if api_key.is_empty() {
            return Err(MuniError::GroqMissingApiKey);
        }

        let wav = encode_wav(samples);

        let part = reqwest::multipart::Part::bytes(wav)
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .map_err(|e| MuniError::GroqConnectionFailed {
                reason: format!("multipart mime: {e}"),
            })?;
        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", DEFAULT_MODEL)
            .text("response_format", "json");

        let response = self
            .http
            .post(&self.endpoint)
            .timeout(self.timeout)
            .header("Authorization", format!("Bearer {api_key}"))
            .multipart(form)
            .send()
            .await
            .map_err(|e| MuniError::GroqConnectionFailed {
                reason: e.to_string(),
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|e| format!("(failed to read error body: {e})"));
            let truncated = truncate_for_log(&body, MAX_ERROR_BODY_BYTES);
            log::warn!(
                target: "groq_whisper",
                "non-2xx HTTP {}: {}",
                status.as_u16(),
                truncated
            );
            return Err(MuniError::GroqServerError {
                status: status.as_u16(),
                body: truncated,
            });
        }

        let body = response
            .text()
            .await
            .map_err(|e| MuniError::GroqConnectionFailed {
                reason: e.to_string(),
            })?;
        let parsed: TranscriptionResponse = serde_json::from_str(&body).map_err(|err| {
            log::warn!(
                target: "groq_whisper",
                "response parse failed: {err}; body={}",
                truncate_for_log(&body, MAX_ERROR_BODY_BYTES)
            );
            MuniError::GroqInvalidResponse
        })?;

        Ok(parsed.text.trim().to_string())
    }
}

/// JSON envelope returned by `/v1/audio/transcriptions` with
/// `response_format=json`. Only the `text` field is consumed; everything
/// else is ignored.
#[derive(Debug, Deserialize)]
struct TranscriptionResponse {
    text: String,
}

/// Encode `samples` as a self-contained PCM-WAV byte buffer.
///
/// Header layout per RFC 1521 / WAVE: 44-byte fixed prefix followed by
/// the little-endian sample bytes. We avoid pulling `hound` into this
/// path because the writer's File-handle ergonomics and per-sample
/// `write_sample` indirection cost more than the manual encoder for a
/// shape this fixed.
fn encode_wav(samples: &[i16]) -> Vec<u8> {
    let byte_rate = PCM_SAMPLE_RATE * PCM_CHANNELS as u32 * (PCM_BITS_PER_SAMPLE as u32 / 8);
    let block_align = PCM_CHANNELS * (PCM_BITS_PER_SAMPLE / 8);
    let data_bytes: u32 = (samples.len() * 2) as u32;
    let total_size: u32 = 36 + data_bytes;

    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&total_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    out.extend_from_slice(&PCM_CHANNELS.to_le_bytes());
    out.extend_from_slice(&PCM_SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&PCM_BITS_PER_SAMPLE.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_wav_writes_expected_44_byte_header() {
        let wav = encode_wav(&[]);
        assert_eq!(wav.len(), 44);
        assert_eq!(&wav[0..4], b"RIFF");
        // Total size = 36 + 0 data bytes.
        assert_eq!(&wav[4..8], &36u32.to_le_bytes());
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        // fmt chunk = 16 bytes
        assert_eq!(&wav[16..20], &16u32.to_le_bytes());
        // PCM = 1
        assert_eq!(&wav[20..22], &1u16.to_le_bytes());
        // 1 channel, 16 kHz
        assert_eq!(&wav[22..24], &1u16.to_le_bytes());
        assert_eq!(&wav[24..28], &16_000u32.to_le_bytes());
        // 16 bits per sample
        assert_eq!(&wav[34..36], &16u16.to_le_bytes());
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(&wav[40..44], &0u32.to_le_bytes());
    }

    #[test]
    fn encode_wav_appends_samples_in_little_endian() {
        let samples: [i16; 3] = [0x0102, -1, i16::MIN];
        let wav = encode_wav(&samples);
        assert_eq!(wav.len(), 44 + 6);
        assert_eq!(&wav[44..46], &0x0102i16.to_le_bytes());
        assert_eq!(&wav[46..48], &(-1i16).to_le_bytes());
        assert_eq!(&wav[48..50], &i16::MIN.to_le_bytes());
        // Data length field reflects the sample bytes, not the sample count.
        assert_eq!(&wav[40..44], &6u32.to_le_bytes());
    }

    #[tokio::test]
    async fn transcribe_with_empty_api_key_returns_missing_error() {
        let client = GroqWhisperClient::with_endpoint("http://127.0.0.1:1".into()).unwrap();
        match client.transcribe(&[0; 16], "").await {
            Err(MuniError::GroqMissingApiKey) => {}
            other => panic!("expected GroqMissingApiKey, got {other:?}"),
        }
    }
}
