//! Integration tests for feature 019 — confidence-triggered mid-press
//! LID re-pass.
//!
//! These tests drive [`DictationSession::spawn_confidence_trigger_task`]
//! directly with mocked confidence + audio channels, a wiremock Groq
//! Whisper endpoint, and an in-process mock [`TextLidClassifier`].
//! That keeps the assertions tight on the trigger's loop semantics
//! without standing up the full press → audio → release pipeline:
//! the unit tests in `session.rs` cover the helpers
//! (`override_decision_deepgram_to_whisper`, `RollingBuffer`,
//! `load_confidence_trigger_config`) and the trigger task here gets
//! end-to-end coverage of:
//!
//! 1. Pure-English / always-high-confidence → trigger never fires.
//! 2. Low-confidence run → re-pass non-English → override flips
//!    `Deepgram → Whisper` and Deepgram WS is closed.
//! 3. Low-confidence run → re-pass English → no flip, trigger exits.
//! 4. Release wins the race against an in-flight re-pass →
//!    `committed` blocks the override → decision stays `Deepgram`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::json;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, watch, Mutex as TokioMutex, Notify};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use muni_lib::deepgram::{ChunkConfidence, DeepgramClient};
use muni_lib::error::MuniError;
use muni_lib::groq_whisper::GroqWhisperClient;
use muni_lib::session::{
    load_confidence_trigger_config, DictationSession, RouterDecision,
    MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE_ENV, MUNI_LID_CONFIDENCE_TRIGGER_ENV,
    MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD_ENV,
};
use muni_lib::text_lid::{LidLabel, LidTokenUsage, TextLidClassifier};

// --- mock helpers -----------------------------------------------------------

/// LID classifier mock that returns a caller-supplied label and counts
/// calls. Replaces `groq_lid` / `gemini_lid` for these tests so the
/// trigger's classify branch is deterministic.
struct MockLidClassifier {
    label: LidLabel,
    calls: AtomicUsize,
}

impl MockLidClassifier {
    fn new(label: LidLabel) -> Self {
        Self {
            label,
            calls: AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TextLidClassifier for MockLidClassifier {
    async fn classify(&self, _text: &str) -> Result<(LidLabel, Option<LidTokenUsage>), MuniError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok((
            self.label.clone(),
            Some(LidTokenUsage {
                input_tokens: 10,
                output_tokens: 1,
            }),
        ))
    }

    fn provider_label(&self) -> &str {
        "mock:test"
    }
}

/// LID classifier mock that blocks until a oneshot fires — for the
/// release-race test (Task 22). Returns the supplied label once
/// unblocked.
struct BlockingLidClassifier {
    label: LidLabel,
    gate: TokioMutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl BlockingLidClassifier {
    fn new(label: LidLabel) -> (Arc<Self>, tokio::sync::oneshot::Sender<()>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let me = Arc::new(Self {
            label,
            gate: TokioMutex::new(Some(rx)),
        });
        (me, tx)
    }
}

#[async_trait]
impl TextLidClassifier for BlockingLidClassifier {
    async fn classify(&self, _text: &str) -> Result<(LidLabel, Option<LidTokenUsage>), MuniError> {
        let rx = self.gate.lock().await.take();
        if let Some(rx) = rx {
            let _ = rx.await;
        }
        Ok((
            self.label.clone(),
            Some(LidTokenUsage {
                input_tokens: 10,
                output_tokens: 1,
            }),
        ))
    }

    fn provider_label(&self) -> &str {
        "mock:blocking"
    }
}

/// Spawn a Deepgram-like mock WS that just accepts frames and never
/// emits anything. The trigger task in these tests provides confidence
/// events via the direct mpsc channel; the WS is here only because
/// `DictationSession::spawn_confidence_trigger_task` takes an
/// `Arc<DeepgramClient>` to call `close()` on. We pass a real client
/// pointing at this no-op mock so the close path actually exercises
/// the WS lifecycle.
async fn start_noop_deepgram_mock() -> (String, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind noop dg mock");
    let addr = listener.local_addr().expect("local addr");
    let url = format!("ws://{addr}/v1/listen");
    let closed = Arc::new(AtomicBool::new(false));
    let closed_for_task = closed.clone();
    tokio::spawn(async move {
        if let Ok((stream, _peer)) = listener.accept().await {
            tokio::spawn(drain_until_close(stream, closed_for_task));
        }
    });
    (url, closed)
}

async fn drain_until_close(stream: TcpStream, closed: Arc<AtomicBool>) {
    let Ok(mut ws) = accept_async(stream).await else {
        return;
    };
    while let Some(msg) = ws.next().await {
        match msg {
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        }
    }
    let _ = ws.close(None).await;
    closed.store(true, Ordering::SeqCst);
}

/// Spin up a wiremock Whisper endpoint that always returns the
/// supplied transcript. Sufficient for tests that don't care about
/// per-call differentiation (the LID mock controls the routing).
async fn start_whisper_mock(text: &'static str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(json!({ "text": text }).to_string()),
        )
        .mount(&server)
        .await;
    server
}

/// Guard for the env-var mutations these tests perform — serialises
/// access so parallel `cargo test --test confidence_trigger` runs
/// don't trample each other's reads/writes.
fn env_lock() -> &'static std::sync::Mutex<()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

struct EnvGuard<'a> {
    _lock: std::sync::MutexGuard<'a, ()>,
}

impl<'a> EnvGuard<'a> {
    fn lock() -> Self {
        Self {
            _lock: env_lock().lock().unwrap_or_else(|p| p.into_inner()),
        }
    }
}

impl Drop for EnvGuard<'_> {
    fn drop(&mut self) {
        std::env::remove_var(MUNI_LID_CONFIDENCE_TRIGGER_ENV);
        std::env::remove_var(MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD_ENV);
        std::env::remove_var(MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE_ENV);
    }
}

// --- tests ------------------------------------------------------------------

/// Scenario 1 — pure English press. All confidence events arrive
/// above threshold, so the counter never reaches `consecutive`. The
/// trigger task is idle for the press's duration and then exits on
/// release. The decision cell stays at the initial `Some(Deepgram)`;
/// no override fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pure_english_high_confidence_never_fires_trigger() {
    let _guard = EnvGuard::lock();
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_ENV, "true");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD_ENV, "0.7");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE_ENV, "3");
    let cfg = load_confidence_trigger_config();
    assert!(cfg.enabled);

    let (dg_url, _dg_closed) = start_noop_deepgram_mock().await;
    let dg_client = Arc::new(
        DeepgramClient::open_at("test-token", &dg_url)
            .await
            .expect("dg open"),
    );

    let whisper_server = start_whisper_mock("english tail").await;
    let whisper_client = Arc::new(
        GroqWhisperClient::with_endpoint(format!(
            "{}/v1/audio/transcriptions",
            whisper_server.uri()
        ))
        .expect("whisper client"),
    );

    let lid: Arc<dyn TextLidClassifier> = Arc::new(MockLidClassifier::new(LidLabel::English));

    let (conf_tx, conf_rx) = mpsc::channel::<ChunkConfidence>(64);
    let (audio_tx, audio_rx) = broadcast::channel::<Vec<i16>>(64);
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));
    let notify = Arc::new(Notify::new());
    let committed = Arc::new(AtomicBool::new(false));
    let release_tx = watch::channel(false).0;
    let aborted = Arc::new(AtomicBool::new(false));

    let handle = DictationSession::spawn_confidence_trigger_task(
        conf_rx,
        audio_rx,
        decision.clone(),
        notify.clone(),
        committed.clone(),
        release_tx.clone(),
        aborted.clone(),
        dg_client.clone(),
        whisper_client.clone(),
        lid.clone(),
        "test-key".to_string(),
        None,
        cfg,
        Arc::new(AtomicBool::new(false)),
    );

    // Stream high-confidence events; if they keep coming the counter
    // stays at zero and we never fire.
    for _ in 0..10 {
        let _ = audio_tx.send(vec![0_i16; 1600]);
        conf_tx
            .send(ChunkConfidence {
                confidence: 0.95,
                words_in_chunk: 3,
                is_final: true,
            })
            .await
            .expect("send conf event");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Simulate the orchestrator releasing the press.
    let _ = release_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;

    // Trigger never flipped the decision.
    assert_eq!(
        *decision.lock().await,
        Some(RouterDecision::Deepgram),
        "high-confidence chunks must not flip the route"
    );
    assert!(
        !aborted.load(Ordering::SeqCst),
        "forwarder must not have been aborted"
    );

    dg_client.close().await;
}

/// Scenario 2 — low-confidence run after pass#2 commits Deepgram,
/// and the re-pass classifies non-English. The override must flip
/// the cell to `Some(Whisper)`, set `aborted=true` on the forwarder,
/// and close the Deepgram WS.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn low_confidence_run_with_tagalog_repass_flips_to_whisper() {
    let _guard = EnvGuard::lock();
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_ENV, "true");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD_ENV, "0.7");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE_ENV, "3");
    let cfg = load_confidence_trigger_config();

    let (dg_url, dg_closed) = start_noop_deepgram_mock().await;
    let dg_client = Arc::new(
        DeepgramClient::open_at("test-token", &dg_url)
            .await
            .expect("dg open"),
    );

    let whisper_server = start_whisper_mock("hindi natin alam yan").await;
    let whisper_client = Arc::new(
        GroqWhisperClient::with_endpoint(format!(
            "{}/v1/audio/transcriptions",
            whisper_server.uri()
        ))
        .expect("whisper client"),
    );

    let lid_mock = Arc::new(MockLidClassifier::new(LidLabel::Tagalog));
    let lid: Arc<dyn TextLidClassifier> = lid_mock.clone();

    let (conf_tx, conf_rx) = mpsc::channel::<ChunkConfidence>(64);
    let (audio_tx, audio_rx) = broadcast::channel::<Vec<i16>>(64);
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));
    let notify = Arc::new(Notify::new());
    let committed = Arc::new(AtomicBool::new(false));
    let release_tx = watch::channel(false).0;
    let aborted = Arc::new(AtomicBool::new(false));

    let handle = DictationSession::spawn_confidence_trigger_task(
        conf_rx,
        audio_rx,
        decision.clone(),
        notify.clone(),
        committed.clone(),
        release_tx.clone(),
        aborted.clone(),
        dg_client.clone(),
        whisper_client.clone(),
        lid.clone(),
        "test-key".to_string(),
        None,
        cfg,
        Arc::new(AtomicBool::new(false)),
    );

    // Feed enough audio that the rolling buffer crosses the
    // 1.0 s minimum (16 000 samples) before the re-pass fires.
    for _ in 0..15 {
        // 1600 samples = 100 ms each → 15 * 100 ms = 1.5 s of audio.
        let _ = audio_tx.send(vec![0_i16; 1600]);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Three consecutive low-confidence chunks — meets the default
    // `consecutive=3` threshold.
    for _ in 0..3 {
        conf_tx
            .send(ChunkConfidence {
                confidence: 0.3,
                words_in_chunk: 2,
                is_final: true,
            })
            .await
            .expect("send low-conf");
    }

    // Wait for the trigger task to finish (it exits after one fire).
    let exit = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(exit.is_ok(), "trigger task should exit within 5 s");

    assert_eq!(
        lid_mock.call_count(),
        1,
        "LID classifier called exactly once"
    );
    assert_eq!(
        *decision.lock().await,
        Some(RouterDecision::Whisper),
        "non-English re-pass must flip the route"
    );
    assert!(
        aborted.load(Ordering::SeqCst),
        "forwarder must be aborted after the flip"
    );

    // The Deepgram client's close() ran — the mock observed the close.
    for _ in 0..50 {
        if dg_closed.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        dg_closed.load(Ordering::SeqCst),
        "Deepgram WS should be closed after the flip"
    );
}

/// Scenario 3 — low-confidence run after pass#2, but the re-pass
/// classifies English. The trigger logs "stayed English (reset)"
/// and **continues monitoring** (no flip, no exit). Decision
/// remains `Deepgram`.
///
/// 2026-05-15 design change: the trigger used to exit after a single
/// fire (fire-once-per-press), but dogfood showed that a brief
/// disfluency (cough, "uh") fires pass#3 on surrounding-English audio,
/// returns English, and would burn the trigger before a real
/// code-switch could be detected. Multi-fire-on-English keeps the
/// trigger alive past these false fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn low_confidence_run_with_english_repass_does_not_flip() {
    let _guard = EnvGuard::lock();
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_ENV, "true");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD_ENV, "0.7");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE_ENV, "3");
    let cfg = load_confidence_trigger_config();

    let (dg_url, dg_closed) = start_noop_deepgram_mock().await;
    let dg_client = Arc::new(
        DeepgramClient::open_at("test-token", &dg_url)
            .await
            .expect("dg open"),
    );

    let whisper_server = start_whisper_mock("hello there").await;
    let whisper_client = Arc::new(
        GroqWhisperClient::with_endpoint(format!(
            "{}/v1/audio/transcriptions",
            whisper_server.uri()
        ))
        .expect("whisper client"),
    );

    let lid: Arc<dyn TextLidClassifier> = Arc::new(MockLidClassifier::new(LidLabel::English));

    let (conf_tx, conf_rx) = mpsc::channel::<ChunkConfidence>(64);
    let (audio_tx, audio_rx) = broadcast::channel::<Vec<i16>>(64);
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));
    let notify = Arc::new(Notify::new());
    let committed = Arc::new(AtomicBool::new(false));
    let release_tx = watch::channel(false).0;
    let aborted = Arc::new(AtomicBool::new(false));

    let handle = DictationSession::spawn_confidence_trigger_task(
        conf_rx,
        audio_rx,
        decision.clone(),
        notify.clone(),
        committed.clone(),
        release_tx.clone(),
        aborted.clone(),
        dg_client.clone(),
        whisper_client.clone(),
        lid.clone(),
        "test-key".to_string(),
        None,
        cfg,
        Arc::new(AtomicBool::new(false)),
    );

    for _ in 0..15 {
        let _ = audio_tx.send(vec![0_i16; 1600]);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Drive a single low-conf event past the threshold (K=3, so we
    // send 3 lows in a row), then give the trigger time to transcribe,
    // classify English, and reset its counter.
    for _ in 0..3 {
        conf_tx
            .send(ChunkConfidence {
                confidence: 0.3,
                words_in_chunk: 2,
                is_final: true,
            })
            .await
            .expect("send low-conf");
    }
    // Give the trigger time to process the events and run pass#3
    // (mock Whisper + mock LID both return immediately, so a few
    // hundred ms is plenty).
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        *decision.lock().await,
        Some(RouterDecision::Deepgram),
        "English re-pass must leave the route alone"
    );
    assert!(
        !aborted.load(Ordering::SeqCst),
        "forwarder must not be aborted when re-pass says English"
    );
    assert!(
        !dg_closed.load(Ordering::SeqCst),
        "Deepgram WS must not be closed on English verdict"
    );

    // Drop the senders so the trigger's `recv()` returns
    // `Closed`/`None` and the task exits cleanly. The multi-fire
    // change means the trigger no longer exits on its own after
    // an English verdict — the test owns lifecycle here.
    drop(conf_tx);
    drop(audio_tx);
    let exit = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert!(
        exit.is_ok(),
        "trigger task should exit within 2 s after senders drop"
    );

    dg_client.close().await;
}

/// Multi-fire regression — when the trigger's pass#3 returns English,
/// the trigger keeps running and a *second* burst of low-conf chunks
/// fires pass#3 again. Models the cough-then-Tagalog dogfood case
/// (2026-05-15): a first false fire on a disfluency must not burn
/// the trigger before a real code-switch arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn english_repass_does_not_burn_trigger_for_later_fires() {
    let _guard = EnvGuard::lock();
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_ENV, "true");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD_ENV, "0.7");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE_ENV, "1");
    let cfg = load_confidence_trigger_config();

    let (dg_url, _dg_closed) = start_noop_deepgram_mock().await;
    let dg_client = Arc::new(
        DeepgramClient::open_at("test-token", &dg_url)
            .await
            .expect("dg open"),
    );

    let whisper_server = start_whisper_mock("Ahem.").await;
    let whisper_client = Arc::new(
        GroqWhisperClient::with_endpoint(format!(
            "{}/v1/audio/transcriptions",
            whisper_server.uri()
        ))
        .expect("whisper client"),
    );

    // LID classifier that returns English on the first call, then
    // Tagalog on the second — emulating "cough first, real Tagalog
    // second."
    struct SwitchingLid {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl TextLidClassifier for SwitchingLid {
        async fn classify(
            &self,
            _text: &str,
        ) -> Result<(LidLabel, Option<LidTokenUsage>), MuniError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let label = if n == 0 {
                LidLabel::English
            } else {
                LidLabel::Tagalog
            };
            Ok((
                label,
                Some(LidTokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                }),
            ))
        }
        fn provider_label(&self) -> &str {
            "mock:switching"
        }
    }
    let switching = Arc::new(SwitchingLid {
        calls: AtomicUsize::new(0),
    });
    let lid: Arc<dyn TextLidClassifier> = switching.clone();

    let (conf_tx, conf_rx) = mpsc::channel::<ChunkConfidence>(64);
    let (audio_tx, audio_rx) = broadcast::channel::<Vec<i16>>(64);
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));
    let notify = Arc::new(Notify::new());
    let committed = Arc::new(AtomicBool::new(false));
    let release_tx = watch::channel(false).0;
    let aborted = Arc::new(AtomicBool::new(false));

    let handle = DictationSession::spawn_confidence_trigger_task(
        conf_rx,
        audio_rx,
        decision.clone(),
        notify.clone(),
        committed.clone(),
        release_tx.clone(),
        aborted.clone(),
        dg_client.clone(),
        whisper_client.clone(),
        lid.clone(),
        "test-key".to_string(),
        None,
        cfg,
        Arc::new(AtomicBool::new(false)),
    );

    // Buffer audio.
    for _ in 0..20 {
        let _ = audio_tx.send(vec![0_i16; 1600]);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // First low-conf chunk: triggers fire #1 (cough → English →
    // reset, keep running).
    conf_tx
        .send(ChunkConfidence {
            confidence: 0.0,
            words_in_chunk: 0,
            is_final: true,
        })
        .await
        .expect("send low-conf 1");

    // Give the trigger time to run pass#3 fire #1 fully.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // After fire #1, decision unchanged.
    assert_eq!(
        *decision.lock().await,
        Some(RouterDecision::Deepgram),
        "after English verdict, decision must stay Deepgram"
    );
    assert_eq!(
        switching.calls.load(Ordering::SeqCst),
        1,
        "exactly one pass#3 fire by this point"
    );

    // Second low-conf chunk: triggers fire #2 (Tagalog → flip).
    conf_tx
        .send(ChunkConfidence {
            confidence: 0.0,
            words_in_chunk: 0,
            is_final: true,
        })
        .await
        .expect("send low-conf 2");

    // Wait for the trigger task to flip and exit.
    let exit = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(exit.is_ok(), "trigger should exit within 5 s after flip");

    assert_eq!(
        switching.calls.load(Ordering::SeqCst),
        2,
        "second pass#3 fire must have happened (trigger was not burned by first)"
    );
    assert_eq!(
        *decision.lock().await,
        Some(RouterDecision::Whisper),
        "Tagalog verdict on second fire must flip route to Whisper"
    );
    assert!(
        aborted.load(Ordering::SeqCst),
        "forwarder must be aborted after the flip"
    );
}

/// Scenario 4 / Task 22 — release wins the race. The re-pass blocks
/// on a oneshot we hold from the test; while it's in flight we set
/// `committed=true` (mimicking `finalize_auto_detect`) and unblock
/// the LID classifier. When the verdict (Tagalog) arrives back at
/// the override helper, the `committed` check rejects the late
/// write and the decision stays `Deepgram`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_during_repass_no_late_override() {
    let _guard = EnvGuard::lock();
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_ENV, "true");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD_ENV, "0.7");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE_ENV, "3");
    let cfg = load_confidence_trigger_config();

    let (dg_url, _dg_closed) = start_noop_deepgram_mock().await;
    let dg_client = Arc::new(
        DeepgramClient::open_at("test-token", &dg_url)
            .await
            .expect("dg open"),
    );

    let whisper_server = start_whisper_mock("masyado mabilis").await;
    let whisper_client = Arc::new(
        GroqWhisperClient::with_endpoint(format!(
            "{}/v1/audio/transcriptions",
            whisper_server.uri()
        ))
        .expect("whisper client"),
    );

    let (lid_blocking, unblock_tx) = BlockingLidClassifier::new(LidLabel::Tagalog);
    let lid: Arc<dyn TextLidClassifier> = lid_blocking;

    let (conf_tx, conf_rx) = mpsc::channel::<ChunkConfidence>(64);
    let (audio_tx, audio_rx) = broadcast::channel::<Vec<i16>>(64);
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));
    let notify = Arc::new(Notify::new());
    let committed = Arc::new(AtomicBool::new(false));
    let release_tx = watch::channel(false).0;
    let aborted = Arc::new(AtomicBool::new(false));

    let handle = DictationSession::spawn_confidence_trigger_task(
        conf_rx,
        audio_rx,
        decision.clone(),
        notify.clone(),
        committed.clone(),
        release_tx.clone(),
        aborted.clone(),
        dg_client.clone(),
        whisper_client.clone(),
        lid.clone(),
        "test-key".to_string(),
        None,
        cfg,
        Arc::new(AtomicBool::new(false)),
    );

    // Buffer audio + trip the threshold.
    for _ in 0..15 {
        let _ = audio_tx.send(vec![0_i16; 1600]);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for _ in 0..3 {
        conf_tx
            .send(ChunkConfidence {
                confidence: 0.3,
                words_in_chunk: 2,
                is_final: true,
            })
            .await
            .expect("send low-conf");
    }

    // Give the trigger a moment to enter the re-pass (it'll be stuck
    // on the BlockingLidClassifier's gate). 200 ms is enough for the
    // mock Whisper transcribe (wiremock returns immediately) and the
    // classify to enter the await.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Mimic finalize_auto_detect: commit BEFORE the trigger's
    // override has a chance to run. Order is load-bearing — see
    // `finalize_auto_detect`'s `committed.store(true)` placement.
    committed.store(true, Ordering::SeqCst);

    // Unblock the LID mock so the re-pass returns Tagalog.
    let _ = unblock_tx.send(());

    let exit = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(exit.is_ok(), "trigger task should exit within 5 s");

    // Override must have been rejected by the committed check.
    assert_eq!(
        *decision.lock().await,
        Some(RouterDecision::Deepgram),
        "committed must block late override"
    );
    assert!(
        !aborted.load(Ordering::SeqCst),
        "forwarder must not be aborted when committed wins the race"
    );
    dg_client.close().await;
}

/// Bonus regression — when the confidence channel closes mid-press
/// (Deepgram client called `close()`), the trigger task exits
/// cleanly without firing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confidence_channel_close_exits_trigger_task_cleanly() {
    let _guard = EnvGuard::lock();
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_ENV, "true");
    let cfg = load_confidence_trigger_config();

    let (dg_url, _dg_closed) = start_noop_deepgram_mock().await;
    let dg_client = Arc::new(
        DeepgramClient::open_at("test-token", &dg_url)
            .await
            .expect("dg open"),
    );
    let whisper_server = start_whisper_mock("noop").await;
    let whisper_client = Arc::new(
        GroqWhisperClient::with_endpoint(format!(
            "{}/v1/audio/transcriptions",
            whisper_server.uri()
        ))
        .expect("whisper client"),
    );
    let lid: Arc<dyn TextLidClassifier> = Arc::new(MockLidClassifier::new(LidLabel::English));

    let (conf_tx, conf_rx) = mpsc::channel::<ChunkConfidence>(64);
    let (_audio_tx, audio_rx) = broadcast::channel::<Vec<i16>>(64);
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));
    let notify = Arc::new(Notify::new());
    let committed = Arc::new(AtomicBool::new(false));
    let release_tx = watch::channel(false).0;
    let aborted = Arc::new(AtomicBool::new(false));

    let handle = DictationSession::spawn_confidence_trigger_task(
        conf_rx,
        audio_rx,
        decision.clone(),
        notify.clone(),
        committed.clone(),
        release_tx.clone(),
        aborted.clone(),
        dg_client.clone(),
        whisper_client.clone(),
        lid.clone(),
        "test-key".to_string(),
        None,
        cfg,
        Arc::new(AtomicBool::new(false)),
    );

    // Drop the confidence sender — the trigger task should observe
    // `recv() == None` and exit.
    drop(conf_tx);

    let exit = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert!(
        exit.is_ok(),
        "trigger should exit on confidence-channel close"
    );
    assert_eq!(*decision.lock().await, Some(RouterDecision::Deepgram));
    dg_client.close().await;
}

/// 2026-05-15 regression lock — when pass#3 returns English (no flip),
/// the `InflightGuard::drop` must fire `decision_notify.notify_waiters()`
/// so any `finalize_auto_detect` inflight-waiter wakes promptly instead
/// of sleeping the full `TRIGGER_REPASS_WAIT_MS` budget.
///
/// Pre-fix behavior: notify was only fired by the `override` path. A
/// waiter on `decision_notify` would never wake on the English-reset
/// path and would run to the 1500 ms timeout, causing a ~1.2 s
/// release-to-paste latency leak on pure-English armed presses with a
/// fire-during-drain (observed in dogfood at 19:39 / 19:40).
///
/// Post-fix: guard's Drop fires notify on every pass#3 completion path
/// (English-reset, flip, transcribe error, classify error). This test
/// asserts a waiter registered on `decision_notify` before the
/// triggering low-conf event wakes within hundreds of milliseconds
/// (not the full timeout) when the verdict is English.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn english_repass_fires_decision_notify_via_inflight_guard_drop() {
    let _guard = EnvGuard::lock();
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_ENV, "true");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD_ENV, "0.7");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE_ENV, "1");
    let cfg = load_confidence_trigger_config();

    let (dg_url, _dg_closed) = start_noop_deepgram_mock().await;
    let dg_client = Arc::new(
        DeepgramClient::open_at("test-token", &dg_url)
            .await
            .expect("dg open"),
    );
    let whisper_server = start_whisper_mock("hello world").await;
    let whisper_client = Arc::new(
        GroqWhisperClient::with_endpoint(format!(
            "{}/v1/audio/transcriptions",
            whisper_server.uri()
        ))
        .expect("whisper client"),
    );
    let lid: Arc<dyn TextLidClassifier> = Arc::new(MockLidClassifier::new(LidLabel::English));

    let (conf_tx, conf_rx) = mpsc::channel::<ChunkConfidence>(64);
    let (audio_tx, audio_rx) = broadcast::channel::<Vec<i16>>(64);
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));
    let decision_notify = Arc::new(Notify::new());
    let committed = Arc::new(AtomicBool::new(false));
    let release_tx = watch::channel(false).0;
    let aborted = Arc::new(AtomicBool::new(false));

    let handle = DictationSession::spawn_confidence_trigger_task(
        conf_rx,
        audio_rx,
        decision.clone(),
        decision_notify.clone(),
        committed.clone(),
        release_tx.clone(),
        aborted.clone(),
        dg_client.clone(),
        whisper_client.clone(),
        lid.clone(),
        "test-key".to_string(),
        None,
        cfg,
        Arc::new(AtomicBool::new(false)),
    );

    // Buffer enough audio that the rolling buffer crosses the
    // CONFIDENCE_TRIGGER_MIN_REPASS_SAMPLES threshold (1 s @ 16 kHz).
    for _ in 0..15 {
        let _ = audio_tx.send(vec![0_i16; 1600]);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Spawn a watcher task that awaits the notify and reports back the
    // moment it wakes. Spawn BEFORE sending the conf event so the
    // waiter is registered when `notify_waiters` fires from the
    // trigger's guard drop. Use a small sleep after spawn to ensure
    // the watcher has actually entered the await before we fire the
    // event.
    let (signal_tx, signal_rx) = tokio::sync::oneshot::channel::<std::time::Instant>();
    let dn_for_watcher = decision_notify.clone();
    let watcher = tokio::spawn(async move {
        dn_for_watcher.notified().await;
        let _ = signal_tx.send(std::time::Instant::now());
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Fire the single low-conf chunk (K=1 → immediate pass#3 fire).
    let fired_at = std::time::Instant::now();
    conf_tx
        .send(ChunkConfidence {
            confidence: 0.0,
            words_in_chunk: 0,
            is_final: true,
        })
        .await
        .expect("send low-conf");

    // The trigger should:
    //   1. Receive the conf event
    //   2. Fire pass#3 (mock Whisper transcribe + mock Groq LID classify, both near-instant)
    //   3. Get back `LidLabel::English`
    //   4. Take the English-reset branch
    //   5. `continue` to the next iteration; `InflightGuard::drop` runs
    //   6. Drop fires `notify_waiters()` → watcher wakes
    //
    // The whole loop should complete well under 1 s with mock providers.
    // Pre-fix would never fire notify and the watcher would hang.
    let woke_at = tokio::time::timeout(Duration::from_secs(3), signal_rx)
        .await
        .expect("notify must fire before timeout — pre-fix this would hang")
        .expect("watcher's oneshot must succeed");

    let elapsed = woke_at.duration_since(fired_at);
    assert!(
        elapsed < Duration::from_millis(1500),
        "notify wake must be prompt — pass#3 completion + small jitter, not full TRIGGER_REPASS_WAIT_MS; got {elapsed:?}"
    );

    // Cleanup — drop the senders so the trigger exits cleanly, await
    // the watcher and trigger.
    drop(conf_tx);
    drop(audio_tx);
    let _ = watcher.await;
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

    // Sanity — decision was not flipped (English verdict, no override).
    assert_eq!(
        *decision.lock().await,
        Some(RouterDecision::Deepgram),
        "English verdict must leave decision cell at Deepgram"
    );
    dg_client.close().await;
}
