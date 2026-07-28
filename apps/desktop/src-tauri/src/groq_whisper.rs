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
//! - Form fields: `file=<audio.flac|audio.wav>`, `model=<DEFAULT_MODEL>`,
//!   `response_format=json`. Language is intentionally omitted so Whisper
//!   does its native auto-detection (the right behaviour for Taglish
//!   code-switching).
//! - Response: `{"text": "..."}`.
//!
//! ## FLAC-first upload (plan 041, tasks 9-10)
//!
//! The audio container is a lossless encoding choice, not a transcription
//! one: FLAC round-trips to bit-identical PCM, so swapping it in changes
//! zero bytes of what Groq actually hears. Every press tries
//! [`encode_flac`] first (roughly halves upload bytes, biggest win on
//! long presses); any encode error falls back to the original WAV part
//! unchanged, so the worst case is today's behaviour. `model`,
//! `response_format`, and the absent `language` field never change.
//! Disable via [`FLAC_UPLOAD_ENV`].

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

/// Env var that disables the FLAC-first upload at the `transcribe` call
/// site. Default (unset / any other value) is "FLAC enabled." Opt-out
/// idiom mirrors `groq_warmup::is_enabled` / `groq_keepalive::is_enabled`.
pub const FLAC_UPLOAD_ENV: &str = "MUNI_FLAC_UPLOAD";

/// Read [`FLAC_UPLOAD_ENV`] and return `false` only when it is explicitly
/// set to `"false"` (case-insensitive, trim-ws). Default = enabled — the
/// safe default for a latency/bandwidth win is "on"; the user opts out
/// for A/B comparison, or if a Groq-side FLAC regression ever surfaces.
pub fn flac_upload_enabled() -> bool {
    !std::env::var(FLAC_UPLOAD_ENV)
        .map(|v| v.trim().eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

// Test-only seam: force the next `encode_flac` call on the CURRENT
// thread to return `Err`, exercising the WAV-fallback path without
// depending on flacenc actually rejecting some input (empirically it
// doesn't reject empty samples or a zero sample rate — both encode
// successfully). Thread-local so parallel `cargo test` runs never leak
// this into an unrelated test.
#[cfg(any(test, feature = "test-fixtures"))]
thread_local! {
    static FORCE_FLAC_ENCODE_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test-only: force the next [`encode_flac`] call on this thread to
/// return `Err`, so the WAV-fallback path in `build_audio_part` /
/// `transcribe` can be exercised without a real flacenc failure. Gated
/// so it never compiles into release bundles (mirrors
/// `GroqWhisperClient::set_timeout`).
#[cfg(any(test, feature = "test-fixtures"))]
pub fn force_encode_error_for_test(force: bool) {
    FORCE_FLAC_ENCODE_ERROR.with(|f| f.set(force));
}

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
        Self::with_endpoint(Self::resolve_endpoint())
    }

    /// Resolve the production endpoint, honouring [`ENDPOINT_OVERRIDE_ENV`]
    /// (dev-only escape hatch). Pulled out (plan 041 task 6) so `lib.rs`
    /// can resolve the endpoint and inject the shared `reqwest::Client`
    /// via [`Self::with_http_client`] without duplicating the override
    /// logic — and so the override warning still fires exactly once on
    /// the boot path.
    pub fn resolve_endpoint() -> String {
        std::env::var(ENDPOINT_OVERRIDE_ENV)
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
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string())
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
        Ok(Self::with_http_client(http, endpoint))
    }

    /// Construct around an already-built `reqwest::Client`. Plan 041
    /// (task 6) — lets `lib.rs` hand every Groq client a clone of the
    /// one [`crate::groq::shared_groq_http`] pool. The per-request
    /// timeout is applied at the `transcribe` call site, never on the
    /// injected client, so sharing the pool across clients is safe.
    pub fn with_http_client(http: reqwest::Client, endpoint: String) -> Self {
        Self {
            endpoint,
            http,
            timeout: DEFAULT_TIMEOUT,
        }
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

        let part = build_audio_part(samples)?;
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

/// Build the multipart `file` part for one press's audio.
///
/// Prefers FLAC ([`encode_flac`]) — lossless, so this can never change
/// what Whisper hears, only how many bytes it takes to get there. Falls
/// back to the original WAV part when [`FLAC_UPLOAD_ENV`] disables FLAC
/// or `encode_flac` errors: a press can never fail because of FLAC (plan
/// 041 invariant). This is the ONLY place either container is built —
/// both the final Tagalog-press upload and the mid-press LID slices
/// route through `transcribe`, so this one change covers both.
fn build_audio_part(samples: &[i16]) -> Result<reqwest::multipart::Part, MuniError> {
    if flac_upload_enabled() {
        match encode_flac(samples, PCM_SAMPLE_RATE) {
            Ok(flac) => {
                log::debug!(
                    target: "groq_whisper",
                    "audio codec=flac bytes={}",
                    flac.len()
                );
                return reqwest::multipart::Part::bytes(flac)
                    .file_name("audio.flac")
                    .mime_str("audio/flac")
                    .map_err(|e| MuniError::GroqConnectionFailed {
                        reason: format!("multipart mime: {e}"),
                    });
            }
            Err(err) => {
                log::warn!(
                    target: "groq_whisper",
                    "flac encode failed, falling back to wav: {err}"
                );
            }
        }
    }

    let wav = encode_wav(samples);
    log::debug!(target: "groq_whisper", "audio codec=wav bytes={}", wav.len());
    reqwest::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| MuniError::GroqConnectionFailed {
            reason: format!("multipart mime: {e}"),
        })
}

/// Encode `samples` (mono, 16-bit, `sample_rate` Hz) as a FLAC byte
/// buffer. Lossless — the decoded PCM is bit-identical to `samples` — so
/// this is purely a container/bandwidth optimization, never a
/// transcription-affecting one (see the module-level FLAC-first note).
///
/// Uses `flacenc`'s default (fast) encoder config: our 16 kHz mono
/// presses don't need the size-vs-speed tradeoff a slower config would
/// buy, and the default already runs orders of magnitude faster than
/// real-time (single-digit milliseconds for a full press).
///
/// Returns `Err` on any config/encode/serialize failure. Callers MUST
/// treat that as "fall back to [`encode_wav`]," never as a hard
/// failure — a press can never fail because of FLAC (plan 041
/// invariant).
fn encode_flac(samples: &[i16], sample_rate: u32) -> Result<Vec<u8>, String> {
    #[cfg(any(test, feature = "test-fixtures"))]
    if FORCE_FLAC_ENCODE_ERROR.with(|f| f.get()) {
        return Err("forced encode error (test seam)".to_string());
    }

    use flacenc::bitsink::ByteSink;
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;
    use flacenc::source::MemSource;

    // flacenc's MemSource wants `&[i32]`; our capture pipeline is i16,
    // so widen losslessly (values already fit in i16 range — the widen
    // can't overflow or clip).
    let widened: Vec<i32> = samples.iter().map(|&s| i32::from(s)).collect();

    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, e)| format!("flac config verify failed: {e:?}"))?;

    let source = MemSource::from_samples(&widened, 1, 16, sample_rate as usize);

    let stream = flacenc::encode_with_fixed_block_size(
        &config,
        source,
        flacenc::constant::DEFAULT_BLOCK_SIZE,
    )
    .map_err(|e| format!("flac encode failed: {e:?}"))?;

    let mut sink = ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| format!("flac serialize failed: {e:?}"))?;

    Ok(sink.into_inner())
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

    /// Serializes tests that mutate the process-global `MUNI_FLAC_UPLOAD`
    /// env var or the thread-local force-error seam. `std::sync::Mutex`
    /// is sufficient (no `.await` held across the guard) — mirrors
    /// `secrets::tests::ENV_VAR_TEST_LOCK`.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    /// One second of a 440 Hz sine at [`PCM_SAMPLE_RATE`], scaled well
    /// inside i16 range — a speech-like (non-silent, non-clipping)
    /// fixture rather than all-zero samples, which FLAC would compress
    /// to a degenerate near-empty size that wouldn't exercise a
    /// realistic size comparison against WAV.
    fn sine_fixture() -> Vec<i16> {
        let seconds = 1.0_f64;
        let freq_hz = 440.0_f64;
        let n = (PCM_SAMPLE_RATE as f64 * seconds) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / PCM_SAMPLE_RATE as f64;
                let amp = (2.0 * std::f64::consts::PI * freq_hz * t).sin();
                (amp * f64::from(i16::MAX) * 0.8) as i16
            })
            .collect()
    }

    #[test]
    fn encode_flac_round_trips_bit_identically_and_beats_wav_size() {
        let samples = sine_fixture();
        let flac = encode_flac(&samples, PCM_SAMPLE_RATE).expect("flac encodes");

        assert_eq!(
            &flac[0..4],
            b"fLaC",
            "flac stream must start with fLaC magic"
        );

        let wav = encode_wav(&samples);
        assert!(
            flac.len() < wav.len(),
            "flac ({} bytes) should beat wav ({} bytes) on a speech-like fixture",
            flac.len(),
            wav.len()
        );

        let mut reader = claxon::FlacReader::new(std::io::Cursor::new(flac)).expect("flac decodes");
        let decoded: Vec<i32> = reader
            .samples()
            .collect::<Result<Vec<i32>, _>>()
            .expect("samples decode cleanly");
        let expected: Vec<i32> = samples.iter().map(|&s| i32::from(s)).collect();
        assert_eq!(decoded, expected, "decoded PCM must be bit-identical");
    }

    #[test]
    fn flac_upload_enabled_defaults_true_and_respects_explicit_false() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(FLAC_UPLOAD_ENV);
        assert!(flac_upload_enabled(), "default = enabled");
        for value in ["false", "FALSE", "  false  ", "fAlSe"] {
            std::env::set_var(FLAC_UPLOAD_ENV, value);
            assert!(
                !flac_upload_enabled(),
                "{FLAC_UPLOAD_ENV}={value:?} disables"
            );
        }
        for value in ["true", "1", "", "yes", "garbage"] {
            std::env::set_var(FLAC_UPLOAD_ENV, value);
            assert!(flac_upload_enabled(), "only explicit `false` disables");
        }
        std::env::remove_var(FLAC_UPLOAD_ENV);
    }

    #[test]
    fn build_audio_part_falls_back_to_wav_on_forced_encode_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(FLAC_UPLOAD_ENV);
        force_encode_error_for_test(true);

        // `build_audio_part` must still succeed via the WAV fallback
        // when `encode_flac` errors — a press can never fail because of
        // FLAC. `reqwest::multipart::Part` doesn't expose its file name
        // for inspection from a unit test, so the `audio.wav`-on-the-wire
        // assertion lives in `tests/groq_whisper_mock.rs` (drives a real
        // wiremock server and inspects the POSTed multipart body); this
        // test's job is narrower — prove the fallback path returns `Ok`
        // instead of propagating the forced encode error.
        let result = build_audio_part(&[0i16; 16]);
        force_encode_error_for_test(false);
        assert!(
            result.is_ok(),
            "wav fallback must succeed even when flac encoding errors, got {result:?}"
        );
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
