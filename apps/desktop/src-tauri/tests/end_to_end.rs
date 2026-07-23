//! End-to-end integration test for `session::DictationSession`.
//!
//! Wires a real `DeepgramPool` (pointing at a tokio-tungstenite mock), a
//! real `GroqClient` (pointing at a `wiremock` SSE server), a real
//! `CleanupPrompt` (loaded from a temp file), and a `MockInjector` into a
//! single press → release flow. Asserts the load-bearing contract of
//! plan §002 Task 17: cleaned text from Groq lands on the injector.
//!
//! The mocks are intentionally minimal — happy-path Deepgram returns
//! "hello world" on `Metadata`, happy-path Groq returns "Hello, world."
//! as the cleaned-up text. No real network, no AppHandle.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use muni_lib::error::MuniError;
use muni_lib::groq::GroqClient;
use muni_lib::injection::PlatformInjector;
use muni_lib::prompt::CleanupPrompt;
use muni_lib::session::{
    fixed_deepgram_key, noop_state_notifier, DeepgramPool, DictationSession, EventEmitter,
    HotkeyMode, SessionDeps, EVENT_TRANSCRIPT_ERROR, EVENT_TRANSCRIPT_FINAL, EVENT_TRANSCRIPT_RAW,
    GROQ_API_KEY_ENV,
};

/// Installs the Groq API key into the process environment exactly once for
/// the lifetime of this test binary.
///
/// `cargo test` runs every test in this binary as a thread inside a single
/// process, so `GROQ_API_KEY_ENV` is process-global shared state. The
/// delivery pipeline resolves the key lazily inside a detached task
/// (`run_groq_cleanup`), which can run long after the test that started it —
/// e.g. behind a mocked Groq delay. If any sibling test tore the key back
/// down with `remove_var` while that task was mid-flight, the in-flight press
/// would see `GroqApiKeyMissing` and paste the raw fallback instead of the
/// cleaned text. Every test uses the identical key value, so setting it once
/// and never removing it is both sufficient and race-free: the environment is
/// mutated exactly once, before any resolver can read it.
fn ensure_groq_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        std::env::set_var(GROQ_API_KEY_ENV, "test-groq-key");
    });
}

/// Records every paste so the test can assert end-to-end that whatever
/// cleaned text the orchestrator produced is the text that reached the
/// injector. Mirrors `tests/injector_mock.rs::MockInjector`.
struct CapturingInjector {
    pasted: Mutex<Vec<String>>,
}

impl CapturingInjector {
    fn new() -> Self {
        Self {
            pasted: Mutex::new(Vec::new()),
        }
    }

    fn captured(&self) -> Vec<String> {
        self.pasted.lock().expect("poisoned").clone()
    }
}

#[async_trait]
impl PlatformInjector for CapturingInjector {
    async fn paste(&self, text: &str) -> Result<(), MuniError> {
        if text.is_empty() {
            return Err(MuniError::NothingToPaste);
        }
        self.pasted.lock().expect("poisoned").push(text.to_string());
        Ok(())
    }
}

type RecordedEvents = Arc<Mutex<Vec<(String, String)>>>;

fn recording_emitter() -> (EventEmitter, RecordedEvents) {
    let store: RecordedEvents = Arc::new(Mutex::new(Vec::new()));
    let store_clone = store.clone();
    let emitter: EventEmitter = Arc::new(move |event, payload| {
        store_clone
            .lock()
            .expect("recording emitter poisoned")
            .push((event.to_string(), payload));
    });
    (emitter, store)
}

/// Spawn a hand-rolled tungstenite server that mirrors Deepgram's happy
/// path: on receiving the first binary audio frame, send back two final
/// `Results` fragments; on receiving `CloseStream`, send `Metadata` and
/// hang up.
async fn start_deepgram_mock(transcript: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind deepgram mock");
    let addr = listener.local_addr().expect("local addr");
    let url = format!("ws://{addr}/v1/listen");

    tokio::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                break;
            };
            let transcript = transcript.to_string();
            tokio::spawn(handle_deepgram(stream, transcript));
        }
    });

    url
}

async fn handle_deepgram(stream: TcpStream, transcript: String) {
    let Ok(mut ws) = accept_async(stream).await else {
        return;
    };
    let mut audio_seen = false;
    while let Some(msg) = ws.next().await {
        let Ok(msg) = msg else {
            break;
        };
        match msg {
            Message::Binary(_) if !audio_seen => {
                audio_seen = true;
                let payload = format!(
                    r#"{{"type":"Results","is_final":true,"channel":{{"alternatives":[{{"transcript":"{}","confidence":0.99}}]}}}}"#,
                    transcript
                );
                let _ = ws.send(Message::Text(payload)).await;
            }
            Message::Binary(_) => {
                // Ignore additional audio frames — they don't change the
                // canned response.
            }
            Message::Text(t) if t.contains("CloseStream") => {
                let _ = ws
                    .send(Message::Text(
                        r#"{"type":"Metadata","request_id":"e2e"}"#.to_string(),
                    ))
                    .await;
                break;
            }
            _ => continue,
        }
    }
    let _ = ws.close(None).await;
}

async fn start_groq_mock(cleaned: &str) -> MockServer {
    let server = MockServer::start().await;
    let body = format!(
        "data: {}\ndata: [DONE]\n",
        json!({ "choices": [{ "delta": { "content": cleaned } }] })
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    server
}

/// Groq mock that fails the first POST (status 500) and succeeds on
/// subsequent POSTs with `cleaned`. The two mocks share the same path;
/// `up_to_n_times(1)` exhausts the failure mock after one match, so the
/// retry falls through to the success mock. `.expect(1)` on the failure
/// mock asserts at server-drop time that the retry actually fired —
/// i.e. the test fails loudly if the orchestrator skipped the retry.
async fn start_groq_mock_fail_then_succeed(cleaned: &str) -> MockServer {
    let server = MockServer::start().await;
    let success_body = format!(
        "data: {}\ndata: [DONE]\n",
        json!({ "choices": [{ "delta": { "content": cleaned } }] })
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream blip"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(success_body),
        )
        .expect(1)
        .mount(&server)
        .await;
    server
}

/// Groq mock whose FIRST POST is delayed (`first_delay`) and returns
/// `first_cleaned`, and whose SECOND POST returns `second_cleaned`
/// immediately. Registration order is load-bearing: wiremock matches the
/// earliest-registered non-exhausted mock, so the delayed mock (registered
/// first, `up_to_n_times(1)`) always serves the first request to arrive —
/// press A's cleanup — while the instant mock serves press B's. Both
/// `.expect(1)` so a dropped/skipped call fails the test loudly.
async fn start_groq_mock_slow_then_fast(
    first_cleaned: &str,
    first_delay: Duration,
    second_cleaned: &str,
) -> MockServer {
    let server = MockServer::start().await;
    let first_body = format!(
        "data: {}\ndata: [DONE]\n",
        json!({ "choices": [{ "delta": { "content": first_cleaned } }] })
    );
    let second_body = format!(
        "data: {}\ndata: [DONE]\n",
        json!({ "choices": [{ "delta": { "content": second_cleaned } }] })
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_delay(first_delay)
                .set_body_string(first_body),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(second_body),
        )
        .expect(1)
        .mount(&server)
        .await;
    server
}

fn write_temp_prompt(body: &str) -> tempfile::NamedTempFile {
    let f = tempfile::NamedTempFile::new().expect("tempfile");
    std::fs::write(f.path(), body).expect("write prompt");
    f
}

/// Helper: feed at least one binary audio chunk into the broadcast channel
/// so the mock Deepgram fires its canned `Results`.
async fn feed_one_chunk(tx: &broadcast::Sender<Vec<i16>>) {
    // Wait briefly for the forwarder to subscribe before sending so the
    // chunk doesn't get dropped on the floor.
    for _ in 0..50 {
        if tx.receiver_count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let _ = tx.send(vec![0_i16, 1, -1, 256]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn press_release_pipeline_pastes_cleaned_text() {
    // Mocks. Deepgram returns "hello world"; Groq cleans it to "Hello, world."
    let dg_url = start_deepgram_mock("hello world").await;
    let groq_server = start_groq_mock("Hello, world.").await;

    // Real GroqClient pointing at the wiremock URL. Pin the cleanup
    // model so `MUNI_CLEANUP_MODEL` in the host env can't leak in.
    let groq_client = Arc::new(
        GroqClient::with_endpoint_and_model(
            format!("{}/v1/chat/completions", groq_server.uri()),
            "llama-3.3-70b-versatile".to_string(),
        )
        .expect("groq client"),
    );

    // CleanupPrompt loaded from a temp file (override path doesn't exist).
    let bundle = write_temp_prompt("Cleanup the transcript.");
    let prompt = Arc::new(CleanupPrompt::new(
        bundle.path().to_path_buf(),
        std::env::temp_dir().join("muni-e2e-no-such-override.md"),
    ));

    // Pool pointing at the mock WS URL.
    let pool = DeepgramPool::spawn_with_endpoint(fixed_deepgram_key("test-key"), dg_url);
    // Wait for the initial warmer to land so the press doesn't pay an inline
    // open cost (and so the test exercises the pre-warmed path the way
    // production does).
    for _ in 0..200 {
        if pool.warmer_count() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        pool.warmer_count() >= 1,
        "initial warmer never parked a client"
    );

    let injector = Arc::new(CapturingInjector::new());
    let (emitter, events) = recording_emitter();

    // Set the dev-path Groq env key so resolve_env_key passes. SAFETY: the
    // value is a placeholder, never a real secret.
    ensure_groq_key();

    let deps = SessionDeps {
        deepgram_pool: pool.clone(),
        groq: Some(groq_client),
        prompt: Some(prompt),
        injector: injector.clone() as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier: noop_state_notifier(),
        present_error: muni_lib::error_presenter::noop_presenter(),
        show_repaste_notice: muni_lib::session::noop_repaste_notice(),
        history: None,
        mic_silenced: muni_lib::session::MicSilencedFlag::default(),
        whisper: None,
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: muni_lib::session::EnglishFastModeFlag::default(),
        bilingual_mode: muni_lib::session::BilingualModeFlag::default(),
        usage_tx: None,
        usage_store: None,
        groq_activity: None,
        my_words: std::sync::Arc::new(muni_lib::my_words::MyWords::default()),
        about_me: muni_lib::about_me::AboutMe::empty(),
        vocabulary: muni_lib::vocabulary::Vocabulary::empty(),
        user_prompt: muni_lib::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    // Build the chunk channel ourselves — production wires this from
    // AudioCapture; the orchestrator only requires a `broadcast::Receiver`.
    let (chunk_tx, chunk_rx) = broadcast::channel::<Vec<i16>>(8);

    // Press: takes a warm WS, spawns the forwarder. The forwarder subscribes
    // to chunk_rx and pumps audio into the WS.
    session
        .handle_hotkey_pressed(chunk_rx, HotkeyMode::Ptt)
        .await;

    // Feed a chunk so the mock Deepgram fires its canned "hello world"
    // Results. Wait briefly for the WS to fully establish + the forwarder
    // to deliver it.
    feed_one_chunk(&chunk_tx).await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Release: signals forwarder, finalizes the WS, runs Groq cleanup,
    // pastes the cleaned text.
    // Delivery (cleanup + paste + history) now runs detached (plan 039
    // task 25); await the returned handle so the paste assertions below are
    // deterministic.
    if let Some(delivery) = session.handle_hotkey_released(false).await {
        delivery.await.expect("delivery task panicked");
    }

    // Assert the cleaned text reached the injector.
    let captured = injector.captured();
    assert_eq!(
        captured,
        vec!["Hello, world.".to_string()],
        "expected cleaned text to be pasted; recorded events: {:?}",
        events.lock().unwrap().clone()
    );

    let recorded = events.lock().expect("poisoned").clone();
    assert!(
        recorded
            .iter()
            .any(|(e, p)| e == EVENT_TRANSCRIPT_RAW && p == "hello world"),
        "expected raw transcript event, got {recorded:?}"
    );
    assert!(
        recorded
            .iter()
            .any(|(e, p)| e == EVENT_TRANSCRIPT_FINAL && p == "Hello, world."),
        "expected cleaned final event, got {recorded:?}"
    );
    assert!(
        !recorded.iter().any(|(e, _)| e == EVENT_TRANSCRIPT_ERROR),
        "happy path must not emit an error event, got {recorded:?}"
    );

    // Pre-warming: after the press completes, the pool should have already
    // scheduled the NEXT warmer — assert the count advanced past the
    // initial one.
    for _ in 0..200 {
        if pool.warmer_count() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        pool.warmer_count() >= 2,
        "expected next warmer to have run; warmer_count={}",
        pool.warmer_count()
    );
}

/// Plan 039 task 25 (VALIDATE) — the driver-overlap contract, end to end
/// through the real `handle_hotkey_pressed`/`handle_hotkey_released` wiring
/// (including the `delivery_order_tail` swap). Press A's Groq cleanup is
/// delayed; press B is issued *during* A's Cleaning phase. Asserts:
///   * B records and finishes its press cycle while A's cleanup is still
///     pending (the driver loop is not blocked on A's delivery), and
///   * both pastes land in press order — A ("Alpha.") before B ("Bravo.") —
///     even though B's own cleanup is instant and would otherwise overtake A.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn press_during_prior_cleanup_records_and_pastes_in_order() {
    // Both presses transcribe to the same raw text; the ordered Groq mock is
    // what makes A's and B's cleaned outputs distinguishable (and A slow).
    let dg_url = start_deepgram_mock("hello world").await;
    let groq_server =
        start_groq_mock_slow_then_fast("Alpha.", Duration::from_millis(700), "Bravo.").await;

    let groq_client = Arc::new(
        GroqClient::with_endpoint_and_model(
            format!("{}/v1/chat/completions", groq_server.uri()),
            "llama-3.3-70b-versatile".to_string(),
        )
        .expect("groq client"),
    );

    let bundle = write_temp_prompt("Cleanup the transcript.");
    let prompt = Arc::new(CleanupPrompt::new(
        bundle.path().to_path_buf(),
        std::env::temp_dir().join("muni-e2e-overlap-no-such-override.md"),
    ));

    let pool = DeepgramPool::spawn_with_endpoint(fixed_deepgram_key("test-key"), dg_url);
    for _ in 0..200 {
        if pool.warmer_count() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(pool.warmer_count() >= 1, "initial warmer never parked");

    let injector = Arc::new(CapturingInjector::new());
    let (emitter, events) = recording_emitter();

    ensure_groq_key();

    let deps = SessionDeps {
        deepgram_pool: pool.clone(),
        groq: Some(groq_client),
        prompt: Some(prompt),
        injector: injector.clone() as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier: noop_state_notifier(),
        present_error: muni_lib::error_presenter::noop_presenter(),
        show_repaste_notice: muni_lib::session::noop_repaste_notice(),
        history: None,
        mic_silenced: muni_lib::session::MicSilencedFlag::default(),
        whisper: None,
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: muni_lib::session::EnglishFastModeFlag::default(),
        bilingual_mode: muni_lib::session::BilingualModeFlag::default(),
        usage_tx: None,
        usage_store: None,
        groq_activity: None,
        my_words: std::sync::Arc::new(muni_lib::my_words::MyWords::default()),
        about_me: muni_lib::about_me::AboutMe::empty(),
        vocabulary: muni_lib::vocabulary::Vocabulary::empty(),
        user_prompt: muni_lib::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    // ---- Press A ----
    let (chunk_tx_a, chunk_rx_a) = broadcast::channel::<Vec<i16>>(8);
    session
        .handle_hotkey_pressed(chunk_rx_a, HotkeyMode::Ptt)
        .await;
    feed_one_chunk(&chunk_tx_a).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Release A: finalize runs inline (freeing the capture slot), then the
    // slow cleanup+paste is spawned detached. Hold the handle — do NOT await
    // it — so A stays in its Cleaning phase while we drive press B.
    let delivery_a = session.handle_hotkey_released(false).await;

    // ---- Press B, during A's Cleaning phase ----
    // The capture slot must already be free (A's finalize released it), so
    // this press is accepted and records — proving the driver loop is not
    // blocked behind A's in-flight delivery.
    let (chunk_tx_b, chunk_rx_b) = broadcast::channel::<Vec<i16>>(8);
    session
        .handle_hotkey_pressed(chunk_rx_b, HotkeyMode::Ptt)
        .await;
    feed_one_chunk(&chunk_tx_b).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let delivery_b = session.handle_hotkey_released(false).await;

    // B has finished recording + releasing, yet A's 700 ms cleanup is still
    // pending — so nothing has pasted yet. This is the load-bearing overlap
    // assertion: B recorded audio while A was mid-cleanup.
    assert!(
        injector.captured().is_empty(),
        "A's slow cleanup should still be pending when B finished recording; \
         captured so far: {:?}",
        injector.captured()
    );

    // Let both deliveries finish.
    if let Some(handle) = delivery_a {
        handle.await.expect("A delivery task panicked");
    }
    if let Some(handle) = delivery_b {
        handle.await.expect("B delivery task panicked");
    }

    // Both pastes landed, in press order — B was gated behind A by the
    // real `delivery_order_tail` chain even though B's cleanup was instant.
    assert_eq!(
        injector.captured(),
        vec!["Alpha.".to_string(), "Bravo.".to_string()],
        "pastes must land in press order across overlapping deliveries; \
         recorded events: {:?}",
        events.lock().unwrap().clone()
    );
}

/// Verifies the cleanup retry path: when the primary Groq call returns
/// a 5xx, the orchestrator must retry once and paste the retry's
/// cleaned text — NOT the raw Deepgram transcript. The mock's
/// `expect(1)` calls assert on drop that both the failing and the
/// succeeding endpoints were hit exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cleanup_retries_once_on_groq_failure_and_pastes_retry_result() {
    let dg_url = start_deepgram_mock("hello world").await;
    let groq_server = start_groq_mock_fail_then_succeed("Hello, world.").await;

    let groq_client = Arc::new(
        GroqClient::with_endpoint_and_model(
            format!("{}/v1/chat/completions", groq_server.uri()),
            "llama-3.3-70b-versatile".to_string(),
        )
        .expect("groq client"),
    );

    let bundle = write_temp_prompt("Cleanup the transcript.");
    let prompt = Arc::new(CleanupPrompt::new(
        bundle.path().to_path_buf(),
        std::env::temp_dir().join("muni-e2e-retry-no-such-override.md"),
    ));

    let pool = DeepgramPool::spawn_with_endpoint(fixed_deepgram_key("test-key"), dg_url);
    for _ in 0..200 {
        if pool.warmer_count() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(pool.warmer_count() >= 1, "initial warmer never parked");

    let injector = Arc::new(CapturingInjector::new());
    let (emitter, events) = recording_emitter();

    ensure_groq_key();

    let deps = SessionDeps {
        deepgram_pool: pool.clone(),
        groq: Some(groq_client),
        prompt: Some(prompt),
        injector: injector.clone() as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier: noop_state_notifier(),
        present_error: muni_lib::error_presenter::noop_presenter(),
        show_repaste_notice: muni_lib::session::noop_repaste_notice(),
        history: None,
        mic_silenced: muni_lib::session::MicSilencedFlag::default(),
        whisper: None,
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: muni_lib::session::EnglishFastModeFlag::default(),
        bilingual_mode: muni_lib::session::BilingualModeFlag::default(),
        usage_tx: None,
        usage_store: None,
        groq_activity: None,
        my_words: std::sync::Arc::new(muni_lib::my_words::MyWords::default()),
        about_me: muni_lib::about_me::AboutMe::empty(),
        vocabulary: muni_lib::vocabulary::Vocabulary::empty(),
        user_prompt: muni_lib::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    let (chunk_tx, chunk_rx) = broadcast::channel::<Vec<i16>>(8);
    session
        .handle_hotkey_pressed(chunk_rx, HotkeyMode::Ptt)
        .await;
    feed_one_chunk(&chunk_tx).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Delivery (cleanup + paste + history) now runs detached (plan 039
    // task 25); await the returned handle so the paste assertions below are
    // deterministic.
    if let Some(delivery) = session.handle_hotkey_released(false).await {
        delivery.await.expect("delivery task panicked");
    }

    // The pasted text must be the *retry's* cleaned text, not the raw
    // Deepgram transcript — that's the load-bearing assertion.
    let captured = injector.captured();
    assert_eq!(
        captured,
        vec!["Hello, world.".to_string()],
        "expected retry's cleaned text to land on injector; events: {:?}",
        events.lock().unwrap().clone()
    );

    // The primary attempt's 500 surfaces as a transient `TRANSCRIPT_ERROR`?
    // No — we only emit the *retry* error (the last word from Groq). If the
    // retry succeeded, no error event should reach the user. This is the
    // user-visible contract: a recovered cleanup should not flash an error.
    let recorded = events.lock().expect("poisoned").clone();
    assert!(
        !recorded.iter().any(|(e, _)| e == EVENT_TRANSCRIPT_ERROR),
        "successful retry must not emit error event, got {recorded:?}"
    );
    assert!(
        recorded
            .iter()
            .any(|(e, p)| e == EVENT_TRANSCRIPT_FINAL && p == "Hello, world."),
        "expected final event with retry's cleaned text, got {recorded:?}"
    );
    // Dropping the MockServer verifies the .expect(1) assertions on both
    // the failing and succeeding mocks — i.e. exactly one primary call
    // (which failed) and exactly one retry call (which succeeded).
    drop(groq_server);
}

/// Verifies the raw-fallback path: when BOTH the primary cleanup and
/// the retry fail, the orchestrator must paste the raw Deepgram
/// transcript and emit a user-visible error event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cleanup_falls_back_to_raw_when_both_primary_and_retry_fail() {
    let dg_url = start_deepgram_mock("hello world").await;

    // Mount a single mock that fails every POST — both the primary and
    // the retry will hit it.
    let groq_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("persistent upstream blip"))
        .expect(2)
        .mount(&groq_server)
        .await;

    let groq_client = Arc::new(
        GroqClient::with_endpoint_and_model(
            format!("{}/v1/chat/completions", groq_server.uri()),
            "llama-3.3-70b-versatile".to_string(),
        )
        .expect("groq client"),
    );

    let bundle = write_temp_prompt("Cleanup the transcript.");
    let prompt = Arc::new(CleanupPrompt::new(
        bundle.path().to_path_buf(),
        std::env::temp_dir().join("muni-e2e-double-fail-no-such-override.md"),
    ));

    let pool = DeepgramPool::spawn_with_endpoint(fixed_deepgram_key("test-key"), dg_url);
    for _ in 0..200 {
        if pool.warmer_count() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let injector = Arc::new(CapturingInjector::new());
    let (emitter, events) = recording_emitter();

    ensure_groq_key();

    let deps = SessionDeps {
        deepgram_pool: pool.clone(),
        groq: Some(groq_client),
        prompt: Some(prompt),
        injector: injector.clone() as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier: noop_state_notifier(),
        present_error: muni_lib::error_presenter::noop_presenter(),
        show_repaste_notice: muni_lib::session::noop_repaste_notice(),
        history: None,
        mic_silenced: muni_lib::session::MicSilencedFlag::default(),
        whisper: None,
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: muni_lib::session::EnglishFastModeFlag::default(),
        bilingual_mode: muni_lib::session::BilingualModeFlag::default(),
        usage_tx: None,
        usage_store: None,
        groq_activity: None,
        my_words: std::sync::Arc::new(muni_lib::my_words::MyWords::default()),
        about_me: muni_lib::about_me::AboutMe::empty(),
        vocabulary: muni_lib::vocabulary::Vocabulary::empty(),
        user_prompt: muni_lib::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    let (chunk_tx, chunk_rx) = broadcast::channel::<Vec<i16>>(8);
    session
        .handle_hotkey_pressed(chunk_rx, HotkeyMode::Ptt)
        .await;
    feed_one_chunk(&chunk_tx).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Delivery (cleanup + paste + history) now runs detached (plan 039
    // task 25); await the returned handle so the paste assertions below are
    // deterministic.
    if let Some(delivery) = session.handle_hotkey_released(false).await {
        delivery.await.expect("delivery task panicked");
    }

    // Both Groq calls failed → pasted text is the raw Deepgram transcript.
    let captured = injector.captured();
    assert_eq!(
        captured,
        vec!["hello world".to_string()],
        "expected raw transcript on double-fail; events: {:?}",
        events.lock().unwrap().clone()
    );

    let recorded = events.lock().expect("poisoned").clone();
    assert!(
        recorded.iter().any(|(e, _)| e == EVENT_TRANSCRIPT_ERROR),
        "expected error event on retry failure, got {recorded:?}"
    );
    assert!(
        recorded
            .iter()
            .any(|(e, p)| e == EVENT_TRANSCRIPT_FINAL && p == "hello world"),
        "expected final event with raw fallback text, got {recorded:?}"
    );
    drop(groq_server);
}

/// Plan 039 slice 2 (task 4) — when BOTH the primary cleanup and the
/// retry fail, the raw-fallback must paste the *marker-stripped*
/// transcript (self-correction already ran deterministically before the
/// LLM), NOT the raw Deepgram transcript. Otherwise a failed cleanup
/// re-surfaces a `scratch that` the user already cancelled.
///
/// The Deepgram transcript carries a self-correction marker, so `stripped`
/// differs from `raw` and the assertion is meaningful. Groq returns 500 on
/// every call (`expect(2)` — primary + retry).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cleanup_double_fail_pastes_marker_stripped_not_raw() {
    // "move the meeting to tuesday actually scratch that move it to wednesday"
    // self-corrects to "move it to wednesday" (position-0 overlap on `move`).
    let raw = "move the meeting to tuesday actually scratch that move it to wednesday";
    let stripped = "move it to wednesday";
    let dg_url = start_deepgram_mock(raw).await;

    let groq_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("persistent upstream blip"))
        .expect(2)
        .mount(&groq_server)
        .await;

    let groq_client = Arc::new(
        GroqClient::with_endpoint_and_model(
            format!("{}/v1/chat/completions", groq_server.uri()),
            "llama-3.3-70b-versatile".to_string(),
        )
        .expect("groq client"),
    );

    let bundle = write_temp_prompt("Cleanup the transcript.");
    let prompt = Arc::new(CleanupPrompt::new(
        bundle.path().to_path_buf(),
        std::env::temp_dir().join("muni-e2e-stripped-fallback-no-such-override.md"),
    ));

    let pool = DeepgramPool::spawn_with_endpoint(fixed_deepgram_key("test-key"), dg_url);
    for _ in 0..200 {
        if pool.warmer_count() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let injector = Arc::new(CapturingInjector::new());
    let (emitter, events) = recording_emitter();

    ensure_groq_key();

    let deps = SessionDeps {
        deepgram_pool: pool.clone(),
        groq: Some(groq_client),
        prompt: Some(prompt),
        injector: injector.clone() as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier: noop_state_notifier(),
        present_error: muni_lib::error_presenter::noop_presenter(),
        show_repaste_notice: muni_lib::session::noop_repaste_notice(),
        history: None,
        mic_silenced: muni_lib::session::MicSilencedFlag::default(),
        whisper: None,
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: muni_lib::session::EnglishFastModeFlag::default(),
        bilingual_mode: muni_lib::session::BilingualModeFlag::default(),
        usage_tx: None,
        usage_store: None,
        groq_activity: None,
        my_words: std::sync::Arc::new(muni_lib::my_words::MyWords::default()),
        about_me: muni_lib::about_me::AboutMe::empty(),
        vocabulary: muni_lib::vocabulary::Vocabulary::empty(),
        user_prompt: muni_lib::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    let (chunk_tx, chunk_rx) = broadcast::channel::<Vec<i16>>(8);
    session
        .handle_hotkey_pressed(chunk_rx, HotkeyMode::Ptt)
        .await;
    feed_one_chunk(&chunk_tx).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Delivery (cleanup + paste + history) now runs detached (plan 039
    // task 25); await the returned handle so the paste assertions below are
    // deterministic.
    if let Some(delivery) = session.handle_hotkey_released(false).await {
        delivery.await.expect("delivery task panicked");
    }

    // Load-bearing assertion: the pasted text is the marker-stripped text,
    // not the raw transcript.
    let captured = injector.captured();
    assert_eq!(
        captured,
        vec![stripped.to_string()],
        "expected marker-stripped fallback on double-fail; events: {:?}",
        events.lock().unwrap().clone()
    );

    let recorded = events.lock().expect("poisoned").clone();
    // The raw transcript is still surfaced on the RAW event (history/raw
    // field keeps the verbatim ASR output).
    assert!(
        recorded
            .iter()
            .any(|(e, p)| e == EVENT_TRANSCRIPT_RAW && p == raw),
        "expected raw transcript event to carry the verbatim ASR text, got {recorded:?}"
    );
    // The FINAL event (what actually landed) carries the stripped text.
    assert!(
        recorded
            .iter()
            .any(|(e, p)| e == EVENT_TRANSCRIPT_FINAL && p == stripped),
        "expected final event with marker-stripped text, got {recorded:?}"
    );
    assert!(
        recorded.iter().any(|(e, _)| e == EVENT_TRANSCRIPT_ERROR),
        "expected error event on retry failure, got {recorded:?}"
    );
    drop(groq_server);
}

/// Plan 039 slice 2 (finding 2) — an EMPTY cleanup body (Groq returns 200
/// but no `delta.content`, e.g. reasoning exhausted the completion cap) is
/// the same failure class as a 5xx: the `finalize_cleanup_text` fallback
/// must paste the *marker-stripped* transcript, NOT the raw one, so an
/// empty-cleanup failure never re-surfaces a `scratch that` the user
/// already cancelled. Unlike the double-fail path this is a "successful"
/// HTTP response, so no retry and no error event fire — a single 200 with
/// an empty content delta.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_cleanup_body_pastes_marker_stripped_not_raw() {
    let raw = "move the meeting to tuesday actually scratch that move it to wednesday";
    let stripped = "move it to wednesday";
    let dg_url = start_deepgram_mock(raw).await;

    // Empty content delta → GroqClient accumulates an empty cleaned body →
    // finalize_cleanup_text hits its fallback branch.
    let groq_server = start_groq_mock("").await;

    let groq_client = Arc::new(
        GroqClient::with_endpoint_and_model(
            format!("{}/v1/chat/completions", groq_server.uri()),
            "llama-3.3-70b-versatile".to_string(),
        )
        .expect("groq client"),
    );

    let bundle = write_temp_prompt("Cleanup the transcript.");
    let prompt = Arc::new(CleanupPrompt::new(
        bundle.path().to_path_buf(),
        std::env::temp_dir().join("muni-e2e-empty-cleanup-no-such-override.md"),
    ));

    let pool = DeepgramPool::spawn_with_endpoint(fixed_deepgram_key("test-key"), dg_url);
    for _ in 0..200 {
        if pool.warmer_count() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let injector = Arc::new(CapturingInjector::new());
    let (emitter, events) = recording_emitter();

    ensure_groq_key();

    let deps = SessionDeps {
        deepgram_pool: pool.clone(),
        groq: Some(groq_client),
        prompt: Some(prompt),
        injector: injector.clone() as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier: noop_state_notifier(),
        present_error: muni_lib::error_presenter::noop_presenter(),
        show_repaste_notice: muni_lib::session::noop_repaste_notice(),
        history: None,
        mic_silenced: muni_lib::session::MicSilencedFlag::default(),
        whisper: None,
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: muni_lib::session::EnglishFastModeFlag::default(),
        bilingual_mode: muni_lib::session::BilingualModeFlag::default(),
        usage_tx: None,
        usage_store: None,
        groq_activity: None,
        my_words: std::sync::Arc::new(muni_lib::my_words::MyWords::default()),
        about_me: muni_lib::about_me::AboutMe::empty(),
        vocabulary: muni_lib::vocabulary::Vocabulary::empty(),
        user_prompt: muni_lib::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    let (chunk_tx, chunk_rx) = broadcast::channel::<Vec<i16>>(8);
    session
        .handle_hotkey_pressed(chunk_rx, HotkeyMode::Ptt)
        .await;
    feed_one_chunk(&chunk_tx).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Delivery (cleanup + paste + history) now runs detached (plan 039
    // task 25); await the returned handle so the paste assertions below are
    // deterministic.
    if let Some(delivery) = session.handle_hotkey_released(false).await {
        delivery.await.expect("delivery task panicked");
    }

    // Load-bearing assertion: the empty-cleanup fallback pastes the
    // marker-stripped text, not the raw transcript.
    let captured = injector.captured();
    assert_eq!(
        captured,
        vec![stripped.to_string()],
        "expected marker-stripped fallback on empty cleanup; events: {:?}",
        events.lock().unwrap().clone()
    );

    let recorded = events.lock().expect("poisoned").clone();
    // History/raw field still carries the verbatim ASR text.
    assert!(
        recorded
            .iter()
            .any(|(e, p)| e == EVENT_TRANSCRIPT_RAW && p == raw),
        "expected raw transcript event to carry the verbatim ASR text, got {recorded:?}"
    );
    // The FINAL event (what actually landed) carries the stripped text.
    assert!(
        recorded
            .iter()
            .any(|(e, p)| e == EVENT_TRANSCRIPT_FINAL && p == stripped),
        "expected final event with marker-stripped text, got {recorded:?}"
    );
    drop(groq_server);
}
