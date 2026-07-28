//! Failure-shape coverage for `GroqWhisperClient::transcribe` (plan 039 slice
//! 3, task 8), plus the FLAC-first upload wire coverage (plan 041, tasks
//! 9-10). These typed errors drive the cross-provider rescue trigger, so
//! each must map to the right `MuniError` variant rather than a panic or a
//! misclassified kind.
//!
//! Covered:
//! - non-2xx status → `GroqServerError { status, body }`
//! - 2xx with malformed JSON body → `GroqInvalidResponse`
//! - transport timeout → `GroqConnectionFailed`
//! - FLAC-first upload: `MUNI_FLAC_UPLOAD` default-enabled → multipart body
//!   carries `filename="audio.flac"` + `Content-Type: audio/flac`;
//!   `MUNI_FLAC_UPLOAD=false` → `audio.wav` still lands; a forced encode
//!   error falls back to `audio.wav` even with the flag on.

use std::time::Duration;

use muni_lib::error::MuniError;
use muni_lib::groq_whisper::{force_encode_error_for_test, GroqWhisperClient, FLAC_UPLOAD_ENV};
use tokio::sync::Mutex;
use wiremock::matchers::method;
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

const TEST_KEY: &str = "gsk-test-****ABCD";
const SAMPLES: [i16; 16] = [0; 16];

/// Serializes tests that mutate `MUNI_FLAC_UPLOAD` (process-global) and
/// the `force_encode_error_for_test` thread-local seam — `cargo test`
/// parallelizes by default. `tokio::sync::Mutex` (not `std::sync::Mutex`)
/// because these tests `.await` across the lock; mirrors
/// `tests/groq_warmup_mock.rs::ENV_LOCK`.
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

/// Custom `wiremock` matcher scanning the raw multipart request body for
/// a byte substring. No built-in wiremock matcher inspects multipart
/// part headers (`filename=`, `Content-Type:` inside the body, not the
/// outer request), so this reads `request.body` directly.
struct BodyContains(&'static str);

impl Match for BodyContains {
    fn matches(&self, request: &Request) -> bool {
        String::from_utf8_lossy(&request.body).contains(self.0)
    }
}

#[tokio::test]
async fn non_2xx_status_maps_to_server_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream boom"))
        .mount(&server)
        .await;

    let client = GroqWhisperClient::with_endpoint(server.uri()).expect("client builds");
    match client.transcribe(&SAMPLES, TEST_KEY).await {
        Err(MuniError::GroqServerError { status, body }) => {
            assert_eq!(status, 500);
            assert!(
                body.contains("upstream boom"),
                "body captured, got {body:?}"
            );
        }
        other => panic!("expected GroqServerError, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_json_body_maps_to_invalid_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        // 2xx but the body is not the expected `{"text": "..."}` shape.
        .respond_with(ResponseTemplate::new(200).set_body_string("{not valid json"))
        .mount(&server)
        .await;

    let client = GroqWhisperClient::with_endpoint(server.uri()).expect("client builds");
    match client.transcribe(&SAMPLES, TEST_KEY).await {
        Err(MuniError::GroqInvalidResponse) => {}
        other => panic!("expected GroqInvalidResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn transport_timeout_maps_to_connection_failed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        // Delay the response well past the client's (shortened) timeout so the
        // reqwest per-request timeout fires.
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"text":"too late"}"#)
                .set_delay(Duration::from_secs(2)),
        )
        .mount(&server)
        .await;

    let mut client = GroqWhisperClient::with_endpoint(server.uri()).expect("client builds");
    client.set_timeout(Duration::from_millis(150));
    match client.transcribe(&SAMPLES, TEST_KEY).await {
        Err(MuniError::GroqConnectionFailed { .. }) => {}
        other => panic!("expected GroqConnectionFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn flac_enabled_by_default_uploads_audio_flac_part() {
    let _guard = ENV_LOCK.lock().await;
    std::env::remove_var(FLAC_UPLOAD_ENV);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(BodyContains("filename=\"audio.flac\""))
        .and(BodyContains("Content-Type: audio/flac"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"text":"ok"}"#))
        .expect(1)
        .mount(&server)
        .await;

    let client = GroqWhisperClient::with_endpoint(server.uri()).expect("client builds");
    let text = client
        .transcribe(&SAMPLES, TEST_KEY)
        .await
        .expect("transcribe succeeds");
    assert_eq!(text, "ok");
}

#[tokio::test]
async fn flac_upload_false_still_uploads_audio_wav_part() {
    let _guard = ENV_LOCK.lock().await;
    std::env::set_var(FLAC_UPLOAD_ENV, "false");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(BodyContains("filename=\"audio.wav\""))
        .and(BodyContains("Content-Type: audio/wav"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"text":"ok"}"#))
        .expect(1)
        .mount(&server)
        .await;

    let client = GroqWhisperClient::with_endpoint(server.uri()).expect("client builds");
    let text = client
        .transcribe(&SAMPLES, TEST_KEY)
        .await
        .expect("transcribe succeeds");
    assert_eq!(text, "ok");

    std::env::remove_var(FLAC_UPLOAD_ENV);
}

#[tokio::test]
async fn forced_encode_error_falls_back_to_audio_wav_part() {
    let _guard = ENV_LOCK.lock().await;
    // Flag ON (default) — the fallback must fire from the encode error,
    // not from the flag being off.
    std::env::remove_var(FLAC_UPLOAD_ENV);
    force_encode_error_for_test(true);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(BodyContains("filename=\"audio.wav\""))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"text":"ok"}"#))
        .expect(1)
        .mount(&server)
        .await;

    let client = GroqWhisperClient::with_endpoint(server.uri()).expect("client builds");
    let result = client.transcribe(&SAMPLES, TEST_KEY).await;
    force_encode_error_for_test(false);

    assert_eq!(
        result.expect("transcribe still succeeds via wav fallback"),
        "ok"
    );
}
