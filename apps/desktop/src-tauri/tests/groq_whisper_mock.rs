//! Failure-shape coverage for `GroqWhisperClient::transcribe` (plan 039 slice
//! 3, task 8). These typed errors drive the cross-provider rescue trigger, so
//! each must map to the right `MuniError` variant rather than a panic or a
//! misclassified kind.
//!
//! Covered:
//! - non-2xx status → `GroqServerError { status, body }`
//! - 2xx with malformed JSON body → `GroqInvalidResponse`
//! - transport timeout → `GroqConnectionFailed`

use std::time::Duration;

use muni_lib::error::MuniError;
use muni_lib::groq_whisper::GroqWhisperClient;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_KEY: &str = "gsk-test-****ABCD";
const SAMPLES: [i16; 16] = [0; 16];

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
