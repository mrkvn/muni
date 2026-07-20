use super::*;
use std::sync::Mutex;

use async_trait::async_trait;

type RecordedEvents = Arc<Mutex<Vec<(String, String)>>>;
type RecordedStates = Arc<Mutex<Vec<SessionState>>>;

/// Recording event sink for assertions.
fn recording_emitter() -> (EventEmitter, RecordedEvents) {
    let store = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let store_for_closure = store.clone();
    let emitter: EventEmitter = Arc::new(move |event, payload| {
        store_for_closure
            .lock()
            .expect("recording emitter poisoned")
            .push((event.to_string(), payload));
    });
    (emitter, store)
}

fn recording_state_notifier() -> (StateNotifier, RecordedStates) {
    let store = Arc::new(Mutex::new(Vec::<SessionState>::new()));
    let store_for_closure = store.clone();
    let notifier: StateNotifier = Arc::new(move |state| {
        store_for_closure
            .lock()
            .expect("recording state notifier poisoned")
            .push(state);
    });
    (notifier, store)
}

/// Test injector that records every `paste` call and counts every
/// `press_enter` (auto-submit) call. Mirrors the harness used by
/// `tests/injector_mock.rs`.
struct MockInjector {
    pasted: Mutex<Vec<String>>,
    submits: Mutex<u32>,
}

impl MockInjector {
    fn new() -> Self {
        Self {
            pasted: Mutex::new(Vec::new()),
            submits: Mutex::new(0),
        }
    }

    fn captured(&self) -> Vec<String> {
        self.pasted.lock().expect("poisoned").clone()
    }

    fn submit_count(&self) -> u32 {
        *self.submits.lock().expect("poisoned")
    }
}

#[async_trait]
impl PlatformInjector for MockInjector {
    async fn paste(&self, text: &str) -> Result<(), MuniError> {
        if text.is_empty() {
            return Err(MuniError::NothingToPaste);
        }
        self.pasted.lock().expect("poisoned").push(text.to_string());
        Ok(())
    }

    async fn press_enter(&self) -> Result<(), MuniError> {
        *self.submits.lock().expect("poisoned") += 1;
        Ok(())
    }
}

/// Build a minimal session with an unreachable pool — the orchestrator
/// still uses the pool only via its trait; tests inject `groq`/`prompt`
/// to drive the cleanup-path branches without touching the network.
fn unreachable_pool() -> Arc<DeepgramPool> {
    // Bind to 127.0.0.1:1 — guaranteed connection refused so an inline
    // open path would fail loudly. Tests in this module don't actually
    // call `take()` (they exercise `run_groq_cleanup` / `deliver_final`
    // directly), so no real socket is opened.
    DeepgramPool::spawn_with_endpoint(fixed_deepgram_key("test-key"), "ws://127.0.0.1:1".into())
}

fn session_with(
    groq: Option<Arc<GroqClient>>,
    prompt: Option<Arc<CleanupPrompt>>,
    injector: Arc<MockInjector>,
) -> (
    Arc<DictationSession>,
    RecordedEvents,
    RecordedStates,
    Arc<MockInjector>,
) {
    let (emitter, events) = recording_emitter();
    let (state_notifier, states) = recording_state_notifier();
    let deps = SessionDeps {
        deepgram_pool: unreachable_pool(),
        groq,
        prompt,
        injector: injector.clone() as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier,
        present_error: crate::error_presenter::noop_presenter(),
        show_repaste_notice: noop_repaste_notice(),
        history: None,
        mic_silenced: MicSilencedFlag::default(),
        whisper: None,
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: EnglishFastModeFlag::default(),
        bilingual_mode: BilingualModeFlag::default(),
        usage_tx: None,
        my_words: std::sync::Arc::new(crate::my_words::MyWords::default()),
        about_me: crate::about_me::AboutMe::empty(),
        vocabulary: crate::vocabulary::Vocabulary::empty(),
        user_prompt: crate::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    (DictationSession::new(deps), events, states, injector)
}

#[tokio::test]
async fn missing_groq_falls_back_to_raw_with_error_event() {
    let injector = Arc::new(MockInjector::new());
    let (session, events, states, injector) = session_with(None, None, injector);

    session
        .run_groq_cleanup(
            "hello world",
            SERVED_BY_GLADIA_PRIMARY,
            Duration::from_secs(3),
            false,
            DeliveryContext::immediate(),
        )
        .await;

    // Raw fallback: paste called with "hello world".
    assert_eq!(injector.captured(), vec!["hello world".to_string()]);
    // Plain commit (not Enter) → no auto-submit.
    assert_eq!(injector.submit_count(), 0);

    // Final event emitted with raw text.
    let recorded = events.lock().expect("poisoned").clone();
    assert!(
        recorded
            .iter()
            .any(|(e, p)| e == EVENT_TRANSCRIPT_FINAL && p == "hello world"),
        "expected final event with raw text, got {recorded:?}"
    );
    // Error event emitted (CleanupPromptMissing fires before GroqMissing
    // because the prompt is checked first).
    assert!(
        recorded.iter().any(|(e, _)| e == EVENT_TRANSCRIPT_ERROR),
        "expected error event, got {recorded:?}"
    );

    // State trail: the prompt-missing emit_error fires Error, then the
    // raw paste lands and we transition to Idle.
    let state_trail = states.lock().expect("states poisoned").clone();
    assert!(state_trail.contains(&SessionState::Error));
    assert_eq!(
        state_trail.last(),
        Some(&SessionState::Idle),
        "expected Idle after raw fallback, got {state_trail:?}"
    );
}

#[tokio::test]
async fn missing_groq_with_prompt_present_still_falls_back_to_raw() {
    // CleanupPrompt loaded from a temp file so the first gate passes;
    // then the missing GroqClient gate triggers the raw fallback.
    let bundle = tempfile::NamedTempFile::new().expect("tempfile");
    std::fs::write(bundle.path(), "system prompt body").unwrap();
    let prompt = Arc::new(CleanupPrompt::new(
        bundle.path().to_path_buf(),
        // A path that doesn't exist — overrides cascade falls through
        // to the bundle path.
        std::env::temp_dir().join("muni-no-such-override.md"),
    ));

    let injector = Arc::new(MockInjector::new());
    let (session, events, _states, injector) = session_with(None, Some(prompt), injector);

    session
        .run_groq_cleanup(
            "hi there",
            SERVED_BY_GLADIA_PRIMARY,
            Duration::from_secs(3),
            false,
            DeliveryContext::immediate(),
        )
        .await;

    assert_eq!(injector.captured(), vec!["hi there".to_string()]);
    let recorded = events.lock().expect("poisoned").clone();
    assert!(recorded
        .iter()
        .any(|(e, p)| e == EVENT_TRANSCRIPT_FINAL && p == "hi there"));
    assert!(recorded.iter().any(|(e, _)| e == EVENT_TRANSCRIPT_ERROR));
}

/// Plan 039 slice 2 (finding 2) — the cleanup-unavailable guards that run
/// BEFORE the Groq call (here: missing CleanupPrompt) must paste the
/// marker-STRIPPED self-correction output, never the raw transcript with a
/// live `scratch that` the user already cancelled. Same acceptance
/// criterion as the two later cleanup-failure paths; this pins the early
/// guards so a locked-keychain / missing-key user never gets the marker
/// pasted verbatim.
#[tokio::test]
async fn cleanup_prompt_missing_guard_pastes_marker_stripped_text() {
    let injector = Arc::new(MockInjector::new());
    // prompt=None → the CleanupPromptMissing guard fires first, before any
    // Groq/keychain work.
    let (session, events, _states, injector) = session_with(None, None, injector);

    session
        .run_groq_cleanup(
            "let's meet tuesday scratch that let's meet wednesday",
            SERVED_BY_GLADIA_PRIMARY,
            Duration::from_secs(3),
            false,
            DeliveryContext::immediate(),
        )
        .await;

    // The marker and cancelled lead-in are gone — NOT the raw transcript.
    assert_eq!(
        injector.captured(),
        vec!["let's meet wednesday".to_string()]
    );
    let recorded = events.lock().expect("poisoned").clone();
    assert!(recorded
        .iter()
        .any(|(e, p)| e == EVENT_TRANSCRIPT_FINAL && p == "let's meet wednesday"));
    assert!(recorded.iter().any(|(e, _)| e == EVENT_TRANSCRIPT_ERROR));
}

/// Plan 039 task 25 (constraint b) — deliveries run concurrently off the
/// driver loop, but pastes must land in press order. Here press A's paste
/// is slow and press B's is instant, so without the paste-order gate B
/// would overtake A. The `DeliveryContext.order` chain (built exactly as
/// `handle_hotkey_released` builds it) must hold B behind A.
#[tokio::test]
async fn deliveries_paste_in_press_order_when_later_would_overtake() {
    struct SlowFirstInjector {
        pasted: std::sync::Mutex<Vec<String>>,
    }
    #[async_trait]
    impl PlatformInjector for SlowFirstInjector {
        async fn paste(&self, text: &str) -> Result<(), MuniError> {
            // Only press A ("alpha") is slow; B ("bravo") is instant.
            if text == "alpha" {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            self.pasted.lock().expect("poisoned").push(text.to_string());
            Ok(())
        }
    }

    let injector = Arc::new(SlowFirstInjector {
        pasted: std::sync::Mutex::new(Vec::new()),
    });
    let (emitter, _events) = recording_emitter();
    let (state_notifier, _states) = recording_state_notifier();
    let deps = SessionDeps {
        deepgram_pool: unreachable_pool(),
        groq: None,
        prompt: None,
        injector: injector.clone() as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier,
        present_error: crate::error_presenter::noop_presenter(),
        show_repaste_notice: noop_repaste_notice(),
        history: None,
        mic_silenced: MicSilencedFlag::default(),
        whisper: None,
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: EnglishFastModeFlag::default(),
        bilingual_mode: BilingualModeFlag::default(),
        usage_tx: None,
        my_words: std::sync::Arc::new(crate::my_words::MyWords::default()),
        about_me: crate::about_me::AboutMe::empty(),
        vocabulary: crate::vocabulary::Vocabulary::empty(),
        user_prompt: crate::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    // Build the paste-order chain: A installs its completion receiver, B
    // captures it as its predecessor — the same swap `handle_hotkey_released`
    // performs against `delivery_order_tail`.
    let (a_done_tx, a_done_rx) = oneshot::channel::<()>();
    let ctx_a = DeliveryContext {
        order: None,
        epoch: None,
    };
    let ctx_b = DeliveryContext {
        order: Some(a_done_rx),
        epoch: None,
    };

    let sa = session.clone();
    let a = tauri::async_runtime::spawn(async move {
        let _order_done = DeliveryDoneGuard {
            tx: Some(a_done_tx),
            epoch: 1,
        };
        sa.deliver_final(
            "alpha",
            "alpha",
            SERVED_BY_GLADIA_PRIMARY,
            CompletionMetrics::test_default(),
            false,
            ctx_a,
        )
        .await;
    });
    // Let A enter its slow paste; B is now ready and would overtake.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let sb = session.clone();
    let b = tauri::async_runtime::spawn(async move {
        sb.deliver_final(
            "bravo",
            "bravo",
            SERVED_BY_GLADIA_PRIMARY,
            CompletionMetrics::test_default(),
            false,
            ctx_b,
        )
        .await;
    });
    a.await.expect("A delivery");
    b.await.expect("B delivery");

    assert_eq!(
        injector.pasted.lock().expect("poisoned").clone(),
        vec!["alpha".to_string(), "bravo".to_string()],
        "pastes must land in press order even though B was ready first"
    );
}

/// Plan 039 task 25 (constraint c) — "recording wins". A delivery that
/// finishes after a *newer* press has taken the HUD must NOT stomp the
/// new press's Listening pill back to Idle. The paste still lands (press
/// A's text is delivered), but the terminal HUD transition is suppressed.
#[tokio::test]
async fn stale_delivery_hud_transition_is_suppressed_by_newer_press() {
    let injector = Arc::new(MockInjector::new());
    let (session, _events, states, injector) = session_with(None, None, injector);

    // Press A captured epoch 1; a newer press B has since bumped it to 2.
    session.press_epoch.store(2, Ordering::SeqCst);

    session
        .deliver_final(
            "alpha text",
            "alpha text",
            SERVED_BY_GLADIA_PRIMARY,
            CompletionMetrics::test_default(),
            false,
            DeliveryContext {
                order: None,
                epoch: Some(1),
            },
        )
        .await;

    // Press A's text still landed — delivery is never gated by the HUD.
    assert_eq!(injector.captured(), vec!["alpha text".to_string()]);
    // ...but the Idle transition was suppressed so B's HUD survives.
    let trail = states.lock().expect("poisoned").clone();
    assert!(
        !trail.contains(&SessionState::Idle),
        "a superseded delivery must not stomp the HUD to Idle, got {trail:?}"
    );
}

/// Plan 039 task 25 (constraint c) — the positive case: when no newer
/// press has taken over (epoch still current), the delivery's terminal
/// Idle transition fires normally.
#[tokio::test]
async fn current_delivery_transitions_hud_to_idle() {
    let injector = Arc::new(MockInjector::new());
    let (session, _events, states, injector) = session_with(None, None, injector);

    session.press_epoch.store(1, Ordering::SeqCst);
    session
        .deliver_final(
            "alpha text",
            "alpha text",
            SERVED_BY_GLADIA_PRIMARY,
            CompletionMetrics::test_default(),
            false,
            DeliveryContext {
                order: None,
                epoch: Some(1),
            },
        )
        .await;

    assert_eq!(injector.captured(), vec!["alpha text".to_string()]);
    let trail = states.lock().expect("poisoned").clone();
    assert_eq!(
        trail.last(),
        Some(&SessionState::Idle),
        "a current delivery must transition to Idle, got {trail:?}"
    );
}

/// Plan 039 task 25 (constraint d) — cancellation only tears down the
/// in-capture press (via `self.active`); an in-flight delivery is
/// decoupled and must still complete. Here a delivery is running when a
/// stray cancel arrives (no active session) — the paste must still land.
#[tokio::test]
async fn cancel_does_not_abort_in_flight_delivery() {
    let injector = Arc::new(MockInjector::new());
    let (session, _events, _states, injector) = session_with(None, None, injector);

    let sd = session.clone();
    let delivery = tauri::async_runtime::spawn(async move {
        sd.deliver_final(
            "in flight",
            "in flight",
            SERVED_BY_GLADIA_PRIMARY,
            CompletionMetrics::test_default(),
            false,
            DeliveryContext {
                order: None,
                epoch: Some(1),
            },
        )
        .await;
    });
    // A cancel that races the in-flight delivery must not disturb it — it
    // only manipulates `self.active`, which the delivery does not touch.
    session.handle_hotkey_cancelled().await;
    delivery.await.expect("delivery");

    assert_eq!(injector.captured(), vec!["in flight".to_string()]);
}

/// Backlog 0003 — happy path. When the release event arrives within
/// the timeout window the helper resolves cleanly with `Released`
/// and matches the legacy behavior of [`wait_for_release`].
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn wait_for_release_or_recover_returns_released_when_release_arrives() {
    let (release_tx, mut release_rx) = broadcast::channel::<ReleaseKind>(8);

    // Send the release before awaiting; with paused time the helper
    // would otherwise just sit on the timeout.
    release_tx.send(ReleaseKind::Commit).expect("queue release");

    let outcome = wait_for_release_or_recover(&mut release_rx, Duration::from_secs(60)).await;
    assert_eq!(outcome, ReleaseWaitOutcome::Released(ReleaseKind::Commit));
}

/// Plan 030 — a cancel release surfaces through the same helper so
/// the driver loop can dispatch to `handle_hotkey_cancelled`.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn wait_for_release_or_recover_carries_cancel_kind() {
    let (release_tx, mut release_rx) = broadcast::channel::<ReleaseKind>(8);
    release_tx
        .send(ReleaseKind::Cancel)
        .expect("queue cancel release");

    let outcome = wait_for_release_or_recover(&mut release_rx, Duration::from_secs(60)).await;
    assert_eq!(outcome, ReleaseWaitOutcome::Released(ReleaseKind::Cancel));
}

/// Regression — a tap-to-toggle session must use a strictly larger
/// force-recovery cap than PTT. Symptom this guards: a continuous
/// hands-free dictation was force-committed mid-sentence at exactly
/// 3 minutes because toggle reused the 180 s PTT backstop. Toggle
/// has explicit terminators (re-tap / Esc / 60 s silence), so its
/// cap only bounds runaway buffer growth and is sized to the toggle
/// audio ceiling.
#[test]
fn toggle_release_timeout_exceeds_ptt_backstop() {
    assert_eq!(
        release_timeout_for(HotkeyMode::Ptt),
        Duration::from_secs(180),
        "PTT keeps its dropped-modifier-release backstop"
    );
    assert_eq!(
        release_timeout_for(HotkeyMode::ToggleLocked),
        Duration::from_secs(600),
        "toggle cap matches the 10-minute toggle audio ceiling"
    );
    assert!(
        release_timeout_for(HotkeyMode::ToggleLocked) > release_timeout_for(HotkeyMode::Ptt),
        "a long continuous toggle dictation must outlast the PTT cap"
    );
}

/// Backlog 0003 — recovery path. With no release ever arriving the
/// helper must resolve with `TimedOut` after `timeout`, rather than
/// blocking the driver loop forever (the failure mode that strands
/// the HUD in `Listening`).
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn wait_for_release_or_recover_times_out_when_release_never_arrives() {
    // Hold the sender alive so the channel doesn't close and short-
    // circuit `wait_for_release` via `Err(Closed)`.
    let (_release_tx, mut release_rx) = broadcast::channel::<ReleaseKind>(8);

    let outcome = wait_for_release_or_recover(&mut release_rx, Duration::from_secs(180)).await;
    assert_eq!(outcome, ReleaseWaitOutcome::TimedOut);
}

/// Plan 039 task 27 — no owed debt behaves exactly like the plain
/// bounded wait: the first release is honored immediately.
#[tokio::test]
async fn realigning_wait_without_debt_honors_first_release() {
    let (tx, mut rx) = broadcast::channel::<ReleaseKind>(8);
    tx.send(ReleaseKind::CommitAndSubmit)
        .expect("queue release");
    let mut debt = 0u32;
    let outcome =
        wait_for_release_or_recover_realigning(&mut rx, Duration::from_secs(5), &mut debt, None)
            .await;
    assert_eq!(
        outcome,
        ReleaseWaitOutcome::Released(ReleaseKind::CommitAndSubmit)
    );
    assert_eq!(debt, 0);
}

/// Plan 039 task 27 — a stale release already buffered at the boundary
/// (e.g. the toggle force-recovery's synthetic Commit) is discarded by
/// identity, and the press's real release is honored.
#[tokio::test]
async fn realigning_wait_discards_buffered_stale_then_honors_real() {
    let (tx, mut rx) = broadcast::channel::<ReleaseKind>(8);
    // Orphan (owed) release, then this press's real release.
    tx.send(ReleaseKind::Commit).expect("queue stale");
    tx.send(ReleaseKind::CommitAndSubmit).expect("queue real");
    let mut debt = 1u32;
    let deadline = Some(Instant::now() + Duration::from_secs(5));
    let outcome = wait_for_release_or_recover_realigning(
        &mut rx,
        Duration::from_secs(5),
        &mut debt,
        deadline,
    )
    .await;
    assert_eq!(
        outcome,
        ReleaseWaitOutcome::Released(ReleaseKind::CommitAndSubmit),
        "the real release must be honored after the buffered orphan is discarded"
    );
    assert_eq!(debt, 0, "the owed orphan was consumed");
}

/// Plan 039 task 27 — the load-bearing case: the orphaned release is
/// delivered *late*, after the next press's wait has already begun (so
/// the old boundary-only drain would have missed it). The realigning
/// wait still discards it by identity and then honors the real release.
#[tokio::test]
async fn realigning_wait_discards_late_stale_after_boundary() {
    let (tx, mut rx) = broadcast::channel::<ReleaseKind>(8);
    let mut debt = 1u32;
    // Catch-up window well beyond both sends so the orphan is still owed.
    let deadline = Some(Instant::now() + Duration::from_secs(5));
    let waiter = wait_for_release_or_recover_realigning(
        &mut rx,
        Duration::from_secs(5),
        &mut debt,
        deadline,
    );
    let feeder = async {
        // Orphan arrives only after the wait is running (nothing was
        // buffered at the boundary).
        tokio::time::sleep(Duration::from_millis(40)).await;
        tx.send(ReleaseKind::Commit).expect("late stale");
        // The new press's real release lands a beat later.
        tokio::time::sleep(Duration::from_millis(40)).await;
        tx.send(ReleaseKind::CommitAndSubmit).expect("real release");
    };
    let (outcome, ()) = tokio::join!(waiter, feeder);
    assert_eq!(
        outcome,
        ReleaseWaitOutcome::Released(ReleaseKind::CommitAndSubmit),
        "a late-arriving orphan must not collapse the real release"
    );
    assert_eq!(debt, 0);
}

/// Plan 039 task 27 — if only the orphan ever arrives and the real
/// release never does, the wait times out (the driver then
/// force-recovers, shipping the captured audio). The orphan was still
/// consumed, so the debt is cleared.
#[tokio::test]
async fn realigning_wait_times_out_when_only_stale_arrives() {
    let (tx, mut rx) = broadcast::channel::<ReleaseKind>(8);
    let mut debt = 1u32;
    // Catch-up window outlasts the press timeout so the orphan is still
    // owed when it arrives (it's discarded, not mistaken for the real one).
    let deadline = Some(Instant::now() + Duration::from_secs(5));
    let waiter = wait_for_release_or_recover_realigning(
        &mut rx,
        Duration::from_millis(150),
        &mut debt,
        deadline,
    );
    let feeder = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        tx.send(ReleaseKind::Commit).expect("late stale");
        // No real release ever follows.
    };
    let (outcome, ()) = tokio::join!(waiter, feeder);
    assert_eq!(outcome, ReleaseWaitOutcome::TimedOut);
    assert_eq!(
        debt, 0,
        "the orphan was consumed even though the real release never came"
    );
}

/// Plan 039 task 27 (regression — finding: catch-up expiry must burn the
/// buffer first). The toggle force-recovery's synthetic Commit is already
/// buffered, but the next press arrives only *after* the catch-up window
/// has elapsed (e.g. the inline finalize of the recovered audio outlasted
/// it). The expiry must not clear the debt before that buffered orphan is
/// consumed — otherwise the orphan instantly satisfies the new press's
/// wait and collapses it (~0 ms commit, dictation lost).
#[tokio::test]
async fn realigning_wait_burns_buffered_orphan_even_after_catchup_expiry() {
    let (tx, mut rx) = broadcast::channel::<ReleaseKind>(8);
    // Buffered synthetic Commit (the orphan), then this press's real one.
    tx.send(ReleaseKind::Commit).expect("queue buffered orphan");
    tx.send(ReleaseKind::CommitAndSubmit).expect("queue real");
    let mut debt = 1u32;
    // Catch-up window already elapsed — the orphan is nonetheless still
    // sitting in the buffer.
    let deadline = Some(Instant::now() - Duration::from_millis(1));
    let outcome = wait_for_release_or_recover_realigning(
        &mut rx,
        Duration::from_secs(5),
        &mut debt,
        deadline,
    )
    .await;
    assert_eq!(
        outcome,
        ReleaseWaitOutcome::Released(ReleaseKind::CommitAndSubmit),
        "a buffered orphan must be burned before the catch-up expiry clears \
             the debt, so it can't collapse the new press"
    );
    assert_eq!(debt, 0, "the buffered orphan was consumed");
}

/// Plan 039 task 27 (regression — finding: mid-wait discard must honor the
/// catch-up deadline). The orphan is genuinely lost (never arrives) and
/// the user re-presses within the catch-up window, but their REAL release
/// only lands *after* the window closes. It must be honored promptly, not
/// swallowed as the orphan for the whole (multi-minute) press timeout.
#[tokio::test]
async fn realigning_wait_honors_real_release_once_catchup_elapses() {
    let (tx, mut rx) = broadcast::channel::<ReleaseKind>(8);
    let mut debt = 1u32;
    // Short catch-up window; nothing is buffered (orphan is lost).
    let deadline = Some(Instant::now() + Duration::from_millis(80));
    let waiter = wait_for_release_or_recover_realigning(
        &mut rx,
        Duration::from_secs(5),
        &mut debt,
        deadline,
    );
    let feeder = async {
        // The real release lands after the catch-up window has elapsed.
        tokio::time::sleep(Duration::from_millis(200)).await;
        tx.send(ReleaseKind::Commit).expect("real release");
    };
    let (outcome, ()) = tokio::join!(waiter, feeder);
    assert_eq!(
        outcome,
        ReleaseWaitOutcome::Released(ReleaseKind::Commit),
        "once the catch-up window closes, a lost orphan must stop swallowing \
             the real release"
    );
    assert_eq!(
        debt, 0,
        "the lost orphan's debt was cleared at the deadline"
    );
}

#[tokio::test]
async fn deliver_final_paste_failure_other_than_nothing_emits_error() {
    // Custom injector that always errs with a non-NothingToPaste variant.
    struct AlwaysFails;
    #[async_trait]
    impl PlatformInjector for AlwaysFails {
        async fn paste(&self, _text: &str) -> Result<(), MuniError> {
            Err(MuniError::AccessibilityDenied)
        }
    }

    let (emitter, events) = recording_emitter();
    let (state_notifier, states) = recording_state_notifier();
    let deps = SessionDeps {
        deepgram_pool: unreachable_pool(),
        groq: None,
        prompt: None,
        injector: Arc::new(AlwaysFails) as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier,
        present_error: crate::error_presenter::noop_presenter(),
        show_repaste_notice: noop_repaste_notice(),
        history: None,
        mic_silenced: MicSilencedFlag::default(),
        whisper: None,
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: EnglishFastModeFlag::default(),
        bilingual_mode: BilingualModeFlag::default(),
        usage_tx: None,
        my_words: std::sync::Arc::new(crate::my_words::MyWords::default()),
        about_me: crate::about_me::AboutMe::empty(),
        vocabulary: crate::vocabulary::Vocabulary::empty(),
        user_prompt: crate::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    session
        .deliver_final(
            "Final text.",
            "Final text.",
            SERVED_BY_GLADIA_PRIMARY,
            CompletionMetrics::test_default(),
            false,
            DeliveryContext::immediate(),
        )
        .await;

    let recorded = events.lock().expect("poisoned").clone();
    assert!(recorded
        .iter()
        .any(|(e, p)| e == EVENT_TRANSCRIPT_FINAL && p == "Final text."));
    assert!(recorded
        .iter()
        .any(|(e, p)| e == EVENT_TRANSCRIPT_ERROR && p.contains("Accessibility")));
    // Error must be the latest tray-relevant transition — NOT Idle —
    // because a failed paste shouldn't quietly clear the badge.
    let trail = states.lock().expect("poisoned").clone();
    assert_eq!(
        trail.last(),
        Some(&SessionState::Error),
        "paste failure must leave session in Error, got {trail:?}"
    );
}

#[tokio::test]
async fn deliver_final_nothing_to_paste_is_quiet() {
    struct NeverPaste;
    #[async_trait]
    impl PlatformInjector for NeverPaste {
        async fn paste(&self, _text: &str) -> Result<(), MuniError> {
            Err(MuniError::NothingToPaste)
        }
    }

    let (emitter, events) = recording_emitter();
    let (state_notifier, states) = recording_state_notifier();
    let deps = SessionDeps {
        deepgram_pool: unreachable_pool(),
        groq: None,
        prompt: None,
        injector: Arc::new(NeverPaste) as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier,
        present_error: crate::error_presenter::noop_presenter(),
        show_repaste_notice: noop_repaste_notice(),
        history: None,
        mic_silenced: MicSilencedFlag::default(),
        whisper: None,
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: EnglishFastModeFlag::default(),
        bilingual_mode: BilingualModeFlag::default(),
        usage_tx: None,
        my_words: std::sync::Arc::new(crate::my_words::MyWords::default()),
        about_me: crate::about_me::AboutMe::empty(),
        vocabulary: crate::vocabulary::Vocabulary::empty(),
        user_prompt: crate::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    session
        .deliver_final(
            "anything",
            "anything",
            SERVED_BY_GLADIA_PRIMARY,
            CompletionMetrics::test_default(),
            false,
            DeliveryContext::immediate(),
        )
        .await;

    let recorded = events.lock().expect("poisoned").clone();
    // Final event should be emitted regardless.
    assert!(recorded
        .iter()
        .any(|(e, p)| e == EVENT_TRANSCRIPT_FINAL && p == "anything"));
    // No error event for NothingToPaste — it's expected steady state.
    assert!(
        !recorded.iter().any(|(e, _)| e == EVENT_TRANSCRIPT_ERROR),
        "NothingToPaste must not emit an error event, got {recorded:?}"
    );
    // NothingToPaste is treated as a clean cycle close → tray returns
    // to Idle so the user can dictate again without a stale badge.
    let trail = states.lock().expect("poisoned").clone();
    assert_eq!(trail, vec![SessionState::Idle]);
}

#[tokio::test]
async fn deliver_final_no_editable_focus_holds_for_repaste() {
    // Feature 037 — a confident NoEditableField probe must skip the paste,
    // still persist the history row (so the re-paste hotkey has something
    // to reinject), fire the notice closure, and end Idle — never Enter.
    struct NoFieldInjector {
        paste_calls: Mutex<u32>,
    }
    #[async_trait]
    impl PlatformInjector for NoFieldInjector {
        async fn paste(&self, _text: &str) -> Result<(), MuniError> {
            *self.paste_calls.lock().expect("poisoned") += 1;
            Ok(())
        }
        async fn has_editable_focus(&self) -> FocusProbe {
            FocusProbe::NoEditableField
        }
    }

    let injector = Arc::new(NoFieldInjector {
        paste_calls: Mutex::new(0),
    });

    // Real history store so we can assert a row was written on the
    // no-field branch (persist runs on a blocking task — we drain it).
    let dir = tempfile::tempdir().expect("tempdir");
    let history =
        Arc::new(HistoryStore::open(HistoryStore::default_path(dir.path())).expect("open history"));

    // Recording notice closure so we can assert it fired exactly once.
    let notice_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let notice_calls_for_closure = Arc::clone(&notice_calls);
    let show_repaste_notice: RepasteNotice = Arc::new(move || {
        notice_calls_for_closure.fetch_add(1, Ordering::SeqCst);
    });

    // Regression: on the no-editable-field *held* branch, the completion must
    // still be persisted — otherwise the "Press <hotkey> to insert your
    // dictation" notice would promise a re-paste that reinjects a stale/older
    // dictation (or no-ops). The held arm writes through `history`.

    let (emitter, _events) = recording_emitter();
    let (state_notifier, states) = recording_state_notifier();
    let deps = SessionDeps {
        deepgram_pool: unreachable_pool(),
        groq: None,
        prompt: None,
        injector: injector.clone() as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier,
        present_error: crate::error_presenter::noop_presenter(),
        show_repaste_notice,
        history: Some(Arc::clone(&history)),
        mic_silenced: MicSilencedFlag::default(),
        whisper: None,
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: EnglishFastModeFlag::default(),
        bilingual_mode: BilingualModeFlag::default(),
        usage_tx: None,
        my_words: std::sync::Arc::new(crate::my_words::MyWords::default()),
        about_me: crate::about_me::AboutMe::empty(),
        vocabulary: crate::vocabulary::Vocabulary::empty(),
        user_prompt: crate::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    session
        .deliver_final(
            "held thought",
            "raw held thought",
            SERVED_BY_GLADIA_PRIMARY,
            CompletionMetrics::test_default(),
            // auto_submit=true must NOT press Enter on the held branch.
            true,
            DeliveryContext::immediate(),
        )
        .await;

    // No paste fired — the blind Cmd+V into nothing is suppressed.
    assert_eq!(
        *injector.paste_calls.lock().expect("poisoned"),
        0,
        "no-editable-focus must skip the paste"
    );
    // Notice closure fired exactly once.
    assert_eq!(notice_calls.load(Ordering::SeqCst), 1);
    // History row still written (persist runs on a blocking task; give it a
    // moment to land, then assert the newest row carries the cleaned text).
    let latest = tokio::task::spawn_blocking(move || {
        for _ in 0..50 {
            if let Ok(Some(rec)) = history.latest() {
                return Some(rec);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    })
    .await
    .expect("join");
    let latest = latest.expect("a history row must be persisted on the held branch");
    assert_eq!(latest.cleaned_text, "held thought");
    assert_eq!(latest.raw_text, "raw held thought");
    // Cycle closes cleanly at Idle — same as a normal successful delivery.
    let trail = states.lock().expect("poisoned").clone();
    assert_eq!(trail, vec![SessionState::Idle]);
}

// ---- feature 021 round-4: pad_with_trailing_silence -----------------

#[test]
fn pad_with_trailing_silence_appends_zero_samples() {
    let input: Vec<i16> = vec![1, 2, 3, -1, -2, -3];
    let padded = pad_with_trailing_silence(&input, 4);
    assert_eq!(padded.len(), input.len() + 4);
    // Original samples preserved at the start.
    assert_eq!(&padded[..input.len()], input.as_slice());
    // Trailing samples are all zero.
    assert!(padded[input.len()..].iter().all(|&s| s == 0));
}

#[test]
fn pad_with_trailing_silence_zero_padding_is_identity_copy() {
    // Padding of 0 should round-trip the input (but freshly
    // allocated — caller owns the returned Vec).
    let input: Vec<i16> = vec![42, -7, 100];
    let padded = pad_with_trailing_silence(&input, 0);
    assert_eq!(padded, input);
}

#[test]
fn pad_with_trailing_silence_on_empty_input_produces_pure_silence() {
    let padded = pad_with_trailing_silence(&[], 16);
    assert_eq!(padded.len(), 16);
    assert!(padded.iter().all(|&s| s == 0));
}

#[test]
fn is_silent_press_fires_at_threshold_and_floor() {
    // feat/022 — long press + peak at or below the calibrated
    // ceiling counts as silent. Negative peaks (loud-then-quiet
    // tail dipping below zero) must trip the same way as positive
    // peaks at the same magnitude.
    let one_second = Duration::from_secs(1);
    assert!(is_silent_press(0, one_second));
    assert!(is_silent_press(SILENCED_PEAK_THRESHOLD, one_second));
    assert!(is_silent_press(-SILENCED_PEAK_THRESHOLD, one_second));
}

#[test]
fn is_silent_press_false_just_above_peak_threshold() {
    // One i16 unit above the ceiling already counts as audible —
    // legitimate ambient-room presses sit > 200 in practice, so
    // the boundary is conservative.
    let one_second = Duration::from_secs(1);
    assert!(!is_silent_press(SILENCED_PEAK_THRESHOLD + 1, one_second));
    assert!(!is_silent_press(1024, one_second));
    assert!(!is_silent_press(i16::MAX, one_second));
}

#[test]
fn is_silent_press_false_below_duration_floor() {
    // A short press (release < floor) can legitimately produce
    // near-zero peak — cpal's first callback may not have arrived.
    // Floor exists to keep accidental hotkey taps from tripping
    // the gate.
    let short = MIN_PRESS_FOR_SILENCE_DETECTION - Duration::from_millis(1);
    assert!(!is_silent_press(0, short));
    assert!(!is_silent_press(SILENCED_PEAK_THRESHOLD, short));
}

#[test]
fn is_silent_press_fires_at_exact_duration_floor() {
    // Boundary contract: duration exactly equal to the floor is
    // long enough to be considered intentional.
    assert!(is_silent_press(0, MIN_PRESS_FOR_SILENCE_DETECTION));
}

#[test]
fn is_silent_press_loud_peak_with_short_negative_overshoot() {
    // i16::MIN.unsigned_abs() == 32768 — never panics, never trips
    // the gate. Pins the absence of a `i16::abs()` overflow bug.
    let one_second = Duration::from_secs(1);
    assert!(!is_silent_press(i16::MIN, one_second));
}

#[test]
fn is_dead_capture_stream_fires_only_on_digital_silence() {
    // The stale-mark gate: a dead stream delivers digitally-zeroed
    // buffers (peak ~0). Exact zero and a stray-glitch LSB at the
    // ceiling both count; negative peaks of the same magnitude match.
    let one_second = Duration::from_secs(1);
    assert!(is_dead_capture_stream(0, one_second));
    assert!(is_dead_capture_stream(
        DEAD_STREAM_PEAK_THRESHOLD,
        one_second
    ));
    assert!(is_dead_capture_stream(
        -DEAD_STREAM_PEAK_THRESHOLD,
        one_second
    ));
}

#[test]
fn is_dead_capture_stream_false_for_live_but_quiet_press() {
    // The regression this whole fix exists for: a quiet or
    // speechless-but-audible press is a LIVE mic and must never be
    // classified as a dead stream (which would flip the pill to Stale).
    // A press just above the dead-stream ceiling, a press below the
    // whisper-skip threshold (64), and a normal room-tone press all
    // read as alive.
    let one_second = Duration::from_secs(1);
    assert!(!is_dead_capture_stream(
        DEAD_STREAM_PEAK_THRESHOLD + 1,
        one_second
    ));
    assert!(!is_dead_capture_stream(SILENCED_PEAK_THRESHOLD, one_second));
    assert!(!is_dead_capture_stream(250, one_second));
    assert!(!is_dead_capture_stream(i16::MAX, one_second));
}

#[test]
fn is_dead_capture_stream_stricter_than_silent_press() {
    // Pins the invariant that the stale-mark gate is a strict subset of
    // the whisper-skip gate: a press classified silent-but-not-dead
    // (peak between the two thresholds) skips Whisper yet keeps the mic
    // honest as Granted. Guards against anyone collapsing the two gates
    // back together — the exact bug that pinned the pill to Stale.
    let one_second = Duration::from_secs(1);
    let quiet_but_alive = SILENCED_PEAK_THRESHOLD; // 64: silent, not dead
    assert!(is_silent_press(quiet_but_alive, one_second));
    assert!(!is_dead_capture_stream(quiet_but_alive, one_second));
}

#[test]
fn is_dead_capture_stream_false_below_duration_floor() {
    // Shares the duration floor with is_silent_press: a sub-floor press
    // can read near-zero before cpal's first callback lands, so it is
    // never treated as a dead stream.
    let short = MIN_PRESS_FOR_SILENCE_DETECTION - Duration::from_millis(1);
    assert!(!is_dead_capture_stream(0, short));
}

#[test]
fn is_noise_only_transcript_true_for_punctuation_and_whitespace() {
    // feat/022 — post-Whisper widen catches the original 2026-05-07
    // `.` hallucination and its punctuation-only siblings.
    assert!(is_noise_only_transcript("."));
    assert!(is_noise_only_transcript("-"));
    assert!(is_noise_only_transcript("..."));
    assert!(is_noise_only_transcript("   "));
    assert!(is_noise_only_transcript(""));
    assert!(is_noise_only_transcript(". -"));
    assert!(is_noise_only_transcript("…"));
}

#[test]
fn is_silent_slice_true_on_all_zero_slice() {
    // feat/022 Gate 2 — the canonical silent-press case: the
    // hybrid's slice buffer is full of zero-valued i16 samples.
    // Must short-circuit before incrementing the inflight counter.
    let silent: Vec<i16> = vec![0; 4096];
    assert!(is_silent_slice(&silent));
}

#[test]
fn is_silent_slice_true_at_threshold_boundary() {
    // Peak exactly equal to SILENCED_PEAK_THRESHOLD counts as
    // silent (matches the `<=` boundary used by the other gates).
    let mut slice: Vec<i16> = vec![0; 64];
    slice[10] = SILENCED_PEAK_THRESHOLD;
    slice[20] = -SILENCED_PEAK_THRESHOLD;
    assert!(is_silent_slice(&slice));
}

#[test]
fn is_silent_slice_false_when_one_sample_above_threshold() {
    // A single sample one unit above the ceiling is enough to
    // count the slice as audible — the gate must NOT fire and
    // the classify must proceed.
    let mut slice: Vec<i16> = vec![0; 4096];
    slice[2048] = SILENCED_PEAK_THRESHOLD + 1;
    assert!(!is_silent_slice(&slice));
}

#[test]
fn audio_too_short_for_groq_whisper_true_on_empty_buffer() {
    // Backlog 0041 — the canonical failure case: cpal delivered
    // zero callbacks before release, so the buffer is empty. Must
    // short-circuit before reaching the Groq HTTP 400.
    assert!(audio_too_short_for_groq_whisper(&[]));
}

#[test]
fn audio_too_short_for_groq_whisper_true_just_below_min() {
    // 159 samples @ 16 kHz = 9.9375 ms, just under Groq's 0.01 s
    // minimum. The 1-and-the-many-cpal-callback case where a fast
    // press delivered one partial callback below the API floor.
    let buf: Vec<i16> = vec![0; 159];
    assert!(audio_too_short_for_groq_whisper(&buf));
}

#[test]
fn audio_too_short_for_groq_whisper_false_at_min() {
    // 160 samples @ 16 kHz = exactly 0.01 s — the documented Groq
    // minimum. Boundary contract: at the floor the gate must NOT
    // fire so a legitimate just-long-enough press still routes.
    let buf: Vec<i16> = vec![0; 160];
    assert!(!audio_too_short_for_groq_whisper(&buf));
}

#[test]
fn audio_too_short_for_groq_whisper_false_on_typical_press() {
    // 16 000 samples = 1 s — a typical intentional press. The
    // gate must NOT fire on real audio regardless of content
    // (silence vs speech is the next gate's job, not this one's).
    let buf: Vec<i16> = vec![0; 16_000];
    assert!(!audio_too_short_for_groq_whisper(&buf));
}

#[test]
fn is_silent_slice_false_for_loud_slice() {
    // Real audible audio (peak in the thousands) never trips the
    // gate.
    let slice: Vec<i16> = (0..4096).map(|i| ((i % 2048) * 4) as i16).collect();
    assert!(!is_silent_slice(&slice));
}

#[test]
fn is_silent_slice_handles_negative_peak_loud_audio() {
    // i16::MIN.unsigned_abs() == 32768. The slice is loud (peak
    // overflows the positive range) — gate must NOT fire.
    let slice: Vec<i16> = vec![i16::MIN; 4096];
    assert!(!is_silent_slice(&slice));
}

#[test]
fn is_silent_slice_true_on_empty_slice() {
    // Defensive: empty slice → peak treated as 0 → silent. In
    // production the hybrid caller guarantees a non-empty slice,
    // but the predicate must not panic on the empty case.
    let slice: Vec<i16> = Vec::new();
    assert!(is_silent_slice(&slice));
}

#[test]
fn is_noise_only_transcript_false_for_alphanumeric_content() {
    // ASCII single chars + words + digits all count as content.
    assert!(!is_noise_only_transcript("yes"));
    assert!(!is_noise_only_transcript("no"));
    assert!(!is_noise_only_transcript("a"));
    assert!(!is_noise_only_transcript("1"));
    assert!(!is_noise_only_transcript("a1"));
    assert!(!is_noise_only_transcript("Hindi"));
    // Unicode letters count as alphanumeric — Japanese hallucinations
    // fall through to cleanup (treated as content). Filtering those
    // is a separate concern (backlog 0010 follow-up).
    assert!(!is_noise_only_transcript("はい"));
    assert!(!is_noise_only_transcript("Thanks for watching!"));
}

// ---- feature 023 (backlog 0040): hallucination allowlist tests --------

#[test]
fn matches_known_hallucination_true_for_exact_thank_you() {
    // Whisper's signature silent-press hallucination shape — and a
    // sample of the normalization variants we expect to see in the
    // wild (leading/trailing whitespace, punctuation, casing).
    assert!(matches_known_hallucination("Thank you."));
    assert!(matches_known_hallucination(" Thank you."));
    assert!(matches_known_hallucination("thank you"));
    assert!(matches_known_hallucination("THANK YOU!"));
    assert!(matches_known_hallucination("  ...Thank you...  "));
}

#[test]
fn matches_known_hallucination_true_for_all_initial_entries() {
    for entry in [
        "Thank you",
        "Thanks for watching",
        "Thank you for watching",
        "Thanks",
        "Bye",
        "Goodbye",
        "You",
        "はい",
        "ありがとうございました",
    ] {
        assert!(
            matches_known_hallucination(entry),
            "should match: {entry:?}"
        );
    }
}

/// Correctness-critical: real dictations containing the hallucination
/// substring must NOT be gated. Brainstorm 005 § Decision 7a locked
/// the policy to *exact match* for this reason — a future tweak that
/// silently switches to `contains()` will fail these tests first.
#[test]
fn matches_known_hallucination_false_for_substring_in_real_speech() {
    assert!(!matches_known_hallucination(
        "Please thank you for your time"
    ));
    assert!(!matches_known_hallucination(
        "Thank you very much for your help"
    ));
    assert!(!matches_known_hallucination(
        "Goodbye for now, I'll see you later"
    ));
    assert!(!matches_known_hallucination("you and me both"));
}

#[test]
fn matches_known_hallucination_false_for_empty_and_punct_only() {
    // is_noise_only_transcript handles these — the allowlist MUST
    // NOT also fire (would double-count in gate-attribution logs).
    assert!(!matches_known_hallucination(""));
    assert!(!matches_known_hallucination("."));
    assert!(!matches_known_hallucination("..."));
    assert!(!matches_known_hallucination("   "));
}

#[test]
fn matches_known_hallucination_false_for_real_dictation() {
    assert!(!matches_known_hallucination("test one two three"));
    assert!(!matches_known_hallucination("My test 1, 2, 3"));
    assert!(!matches_known_hallucination("hello world this is a test"));
}

#[test]
fn normalize_for_hallucination_match_canonicalizes_whitespace_and_case() {
    assert_eq!(
        normalize_for_hallucination_match("  Thank You.  "),
        "thank you"
    );
    assert_eq!(
        normalize_for_hallucination_match("THANK    YOU"),
        "thank you"
    );
    assert_eq!(
        normalize_for_hallucination_match("...thanks for watching!!!"),
        "thanks for watching"
    );
}

/// Locks the threshold constant at compile time so a future
/// "let's bump it for robustness" tweak that would push it above
/// the room-tone floor (~ 200 on a built-in mic) fails to compile
/// instead of silently making the heuristic fire on quiet rooms.
const _: () = assert!(SILENCED_PEAK_THRESHOLD <= 256);

#[test]
fn av_cache_lying_only_when_authorized() {
    // The QA-driven invariant: "Stale — restart Muni" should only
    // surface when AVFoundation thinks the mic is authorized but
    // the audio stream says otherwise. Anything else (denied, not
    // determined, restricted) means the cache is honest and the
    // standard pill is correct.
    assert!(av_cache_is_lying(MicrophoneStatus::Authorized));
    assert!(!av_cache_is_lying(MicrophoneStatus::Denied));
    assert!(!av_cache_is_lying(MicrophoneStatus::NotDetermined));
    assert!(!av_cache_is_lying(MicrophoneStatus::Restricted));
}

#[tokio::test]
async fn pressed_with_failing_pool_emits_listening_before_error() {
    // UX contract: a hotkey press always shows visual feedback before any
    // failure transition takes the HUD back down. Without this, a missing
    // API key produces a press where the user sees nothing — they can't
    // tell whether the hotkey registered, the app is unresponsive, or
    // their key is bad. This test pins the press → Listening → (pool
    // failure) → Error sequence.

    // Pool with an empty key fails synchronously inside `open_at` —
    // no network attempt — so the assertion is deterministic.
    let pool = DeepgramPool::spawn_with_endpoint(fixed_deepgram_key(""), "ws://127.0.0.1:1".into());
    let (emitter, _events) = recording_emitter();
    let (state_notifier, states) = recording_state_notifier();
    let injector = Arc::new(MockInjector::new());
    let deps = SessionDeps {
        deepgram_pool: pool,
        groq: None,
        prompt: None,
        injector: injector as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier,
        present_error: crate::error_presenter::noop_presenter(),
        show_repaste_notice: noop_repaste_notice(),
        history: None,
        mic_silenced: MicSilencedFlag::default(),
        whisper: None,
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: EnglishFastModeFlag::default(),
        bilingual_mode: BilingualModeFlag::default(),
        usage_tx: None,
        my_words: std::sync::Arc::new(crate::my_words::MyWords::default()),
        about_me: crate::about_me::AboutMe::empty(),
        vocabulary: crate::vocabulary::Vocabulary::empty(),
        user_prompt: crate::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    let (_chunk_tx, chunk_rx) = broadcast::channel::<Vec<i16>>(8);
    session
        .handle_hotkey_pressed(chunk_rx, HotkeyMode::Ptt)
        .await;

    let trail = states.lock().expect("poisoned").clone();
    assert!(!trail.is_empty(), "expected state transitions, got nothing");
    assert_eq!(
        trail.first(),
        Some(&SessionState::Listening),
        "Listening MUST fire before any failure path; got {trail:?}",
    );
    assert!(
        trail.contains(&SessionState::Error),
        "Error MUST follow on pool failure; got {trail:?}",
    );
    // Ordering: Listening at position 0, Error somewhere after.
    let listening_idx = trail
        .iter()
        .position(|s| matches!(s, SessionState::Listening))
        .expect("Listening present");
    let error_idx = trail
        .iter()
        .position(|s| matches!(s, SessionState::Error))
        .expect("Error present");
    assert!(
        error_idx > listening_idx,
        "Error must come AFTER Listening; got {trail:?}",
    );
}

#[tokio::test]
async fn capture_start_failure_surfaces_error_and_returns_idle() {
    // Plan 039 task 32 — a capture-start failure at press start must route
    // the typed error through the presenter and drop the HUD to Idle,
    // instead of the old log-and-drop that left a fake Listening pill over
    // a dead mic. Exercises the surfacing method the driver calls when
    // `AudioCapture::start` returns Err (and, via the same method, the
    // `CAPTURE_START_TIMEOUT` arm added for the wedged-probe case).
    //
    // KNOWN GAP (reviewed, accepted): this pins `present_capture_start_error`
    // — the surfacing contract — but NOT the driver wiring around it (that a
    // real `AudioCapture::start` Err reaches this method, that
    // `handle_hotkey_pressed` is skipped so no Listening fires, and that the
    // press's paired release is still consumed to preserve the 1:1
    // press:release invariant). `spawn_driver` takes a concrete
    // `Arc<AudioCapture>` whose `new` needs a live `AppHandle`, so the
    // failure arm can't be driven without Tauri mock-runtime infra this
    // crate doesn't carry. The driver arm is instead guarded structurally:
    // the `if capture_started { handle_hotkey_pressed }` gate followed by the
    // unconditional `wait_for_release_or_recover` keeps both invariants by
    // construction (see the driver-loop comment above), and the toggle
    // teardown that failure arm now performs is pinned separately by
    // `capture_failure_tears_down_armed_toggle_but_not_ptt`.
    let recorded: Arc<Mutex<Vec<MuniError>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = recorded.clone();
    let present_error: PresentError =
        Arc::new(move |e: &MuniError| sink.lock().expect("poisoned").push(e.clone()));
    let (state_notifier, states) = recording_state_notifier();
    let deps = SessionDeps {
        state_notifier,
        present_error,
        ..minimal_deps_for_trim_test(None)
    };
    let session = DictationSession::new(deps);

    session.present_capture_start_error(&MuniError::NoInputDevice);

    let errs = recorded.lock().expect("poisoned").clone();
    assert_eq!(errs.len(), 1, "capture-start error must be surfaced once");
    assert!(matches!(errs[0], MuniError::NoInputDevice));

    let trail = states.lock().expect("poisoned").clone();
    assert_eq!(
        trail.last(),
        Some(&SessionState::Idle),
        "HUD must return to Idle after a capture-start failure; got {trail:?}"
    );
    assert!(
        !trail.contains(&SessionState::Listening),
        "a press that never started capture must not show Listening; got {trail:?}"
    );
}

#[tokio::test]
async fn late_completing_wedged_capture_start_is_stopped() {
    // Plan 039 round-2 finding (slice 11, privacy) — when a capture-start
    // probe wedges past the timeout the driver abandons its `JoinHandle`,
    // but a merely-slow probe can still finish and OPEN the mic afterwards.
    // The late-stop watcher must stop that orphaned mic so it can't stream
    // audio with no session. Simulated by a `spawn_blocking` probe that
    // "wakes up" (returns Ok) after the driver would have timed out, and a
    // stop closure whose invocation is observable.
    let stopped = Arc::new(AtomicBool::new(false));
    let capture_generation = Arc::new(AtomicU64::new(3));
    let start_task = tauri::async_runtime::spawn_blocking(|| {
        std::thread::sleep(Duration::from_millis(20));
        Ok::<(), MuniError>(())
    });
    let stopped_flag = stopped.clone();
    // Generation is unchanged (== 3): this press is still current, so the
    // late-opened mic is orphaned and must be stopped.
    stop_late_capture_if_orphaned(start_task, capture_generation, 3, move || {
        stopped_flag.store(true, Ordering::SeqCst);
    })
    .await;
    assert!(
        stopped.load(Ordering::SeqCst),
        "a late-completing wedged capture start must have its orphaned mic stopped"
    );
}

#[tokio::test]
async fn late_capture_start_not_stopped_when_newer_press_took_over() {
    // Companion to the above — the generation guard must NOT stop a mic that
    // a newer press legitimately re-opened. If `capture_generation` has
    // advanced past the stamp, the late probe belongs to a superseded
    // attempt and stopping would kill the live press's mic.
    let stopped = Arc::new(AtomicBool::new(false));
    let capture_generation = Arc::new(AtomicU64::new(4));
    let start_task = tauri::async_runtime::spawn_blocking(|| Ok::<(), MuniError>(()));
    let stopped_flag = stopped.clone();
    // Stamp was 3 but the counter is now 4 — a newer press owns capture.
    stop_late_capture_if_orphaned(start_task, capture_generation, 3, move || {
        stopped_flag.store(true, Ordering::SeqCst);
    })
    .await;
    assert!(
        !stopped.load(Ordering::SeqCst),
        "a superseded late probe must not stop a newer press's live mic"
    );
}

#[tokio::test]
async fn errored_late_capture_start_does_not_stop() {
    // A probe that eventually FAILS opened no mic, so the watcher must not
    // fire the stop (it would be a harmless no-op, but asserting it keeps the
    // "only stop when a mic was actually opened" contract honest).
    let stopped = Arc::new(AtomicBool::new(false));
    let capture_generation = Arc::new(AtomicU64::new(1));
    let start_task =
        tauri::async_runtime::spawn_blocking(|| Err::<(), MuniError>(MuniError::NoInputDevice));
    let stopped_flag = stopped.clone();
    stop_late_capture_if_orphaned(start_task, capture_generation, 1, move || {
        stopped_flag.store(true, Ordering::SeqCst);
    })
    .await;
    assert!(
        !stopped.load(Ordering::SeqCst),
        "a failed capture-start probe opened no mic and must not trigger a stop"
    );
}

#[tokio::test]
async fn deliver_final_success_emits_state_changed_event() {
    let injector = Arc::new(MockInjector::new());
    let (session, events, states, injector) = session_with(None, None, injector);

    session
        .deliver_final(
            "good morning",
            "good morning",
            SERVED_BY_GLADIA_PRIMARY,
            CompletionMetrics::test_default(),
            false,
            DeliveryContext::immediate(),
        )
        .await;

    // The state-changed event piggybacks on the same emitter the React
    // layer reads, so the wire payload must match `SessionState::as_wire`.
    let recorded = events.lock().expect("poisoned").clone();
    assert!(
        recorded
            .iter()
            .any(|(e, p)| e == EVENT_SESSION_STATE_CHANGED && p == "idle"),
        "expected state-changed=idle event, got {recorded:?}"
    );
    // And the notifier closure fired with the typed enum.
    let trail = states.lock().expect("poisoned").clone();
    assert_eq!(trail, vec![SessionState::Idle]);
    // `auto_submit = false` (re-tap commit) must NOT press Enter.
    assert_eq!(injector.submit_count(), 0);
}

/// "Press Enter to finish" (`auto_submit = true`) must press Enter once
/// after a successful paste — this is what sends the chat message.
#[tokio::test]
async fn deliver_final_auto_submit_presses_enter_after_paste() {
    let injector = Arc::new(MockInjector::new());
    let (session, _events, _states, injector) = session_with(None, None, injector);

    session
        .deliver_final(
            "send me",
            "send me",
            SERVED_BY_GLADIA_PRIMARY,
            CompletionMetrics::test_default(),
            true,
            DeliveryContext::immediate(),
        )
        .await;

    assert_eq!(injector.captured(), vec!["send me".to_string()]);
    assert_eq!(
        injector.submit_count(),
        1,
        "auto_submit must press Enter exactly once after the paste"
    );
}

/// A recovered Deepgram partial (`served_by = deepgram-partial`) must NOT
/// auto-press Enter even when `auto_submit = true` — the transcript may be
/// truncated mid-thought, so silently submitting it is unacceptable. The
/// paste itself must still land.
#[tokio::test]
async fn deliver_final_partial_suppresses_auto_submit() {
    let injector = Arc::new(MockInjector::new());
    let (session, _events, _states, injector) = session_with(None, None, injector);

    session
        .deliver_final(
            "half a thought",
            "half a thought",
            SERVED_BY_DEEPGRAM_PARTIAL,
            CompletionMetrics::test_default(),
            true,
            DeliveryContext::immediate(),
        )
        .await;

    // Paste still happened...
    assert_eq!(injector.captured(), vec!["half a thought".to_string()]);
    // ...but Enter was NOT pressed despite auto_submit = true.
    assert_eq!(
        injector.submit_count(),
        0,
        "a recovered partial must never auto-submit, even with auto_submit = true"
    );
}

/// Regression (dogfood 2026-06-16): a recovered partial is a degraded
/// *success*, so its runtime signal must be the amber `Recovering` HUD
/// pill — never `SessionState::Error`. The original wiring routed the
/// partial through `emit_error`, which sets `Error`; because the HUD
/// *hides* on `error`, the amber pill flashed for one frame and vanished,
/// shipping the signal invisible. `signal_partial_recovered` must raise
/// `Recovering` and nothing else.
#[tokio::test]
async fn signal_partial_recovered_raises_recovering_never_error() {
    let injector = Arc::new(MockInjector::new());
    let (session, _events, states, _injector) = session_with(None, None, injector);

    session.signal_partial_recovered();

    let trail = states.lock().expect("poisoned").clone();
    assert_eq!(
        trail,
        vec![SessionState::Recovering],
        "a recovered partial must raise exactly the amber Recovering pill"
    );
    assert!(
        !trail.contains(&SessionState::Error),
        "a recovered partial must never set SessionState::Error (it hides the HUD pill)"
    );
}

/// Auto-submit must NOT fire when there's nothing to paste — otherwise a
/// silent/empty press would inject a stray newline (e.g. send an empty
/// chat message). The Enter press lives in the paste-success branch, so
/// `NothingToPaste` short-circuits before it.
#[tokio::test]
async fn deliver_final_auto_submit_skips_enter_when_nothing_to_paste() {
    let injector = Arc::new(MockInjector::new());
    let (session, _events, _states, injector) = session_with(None, None, injector);

    // Empty text makes MockInjector::paste return NothingToPaste.
    session
        .deliver_final(
            "",
            "",
            SERVED_BY_GLADIA_PRIMARY,
            CompletionMetrics::test_default(),
            true,
            DeliveryContext::immediate(),
        )
        .await;

    assert_eq!(
        injector.submit_count(),
        0,
        "no paste landed → no Enter, even with auto_submit"
    );
}

#[test]
fn session_state_as_wire_uses_camel_case() {
    assert_eq!(SessionState::Idle.as_wire(), "idle");
    assert_eq!(SessionState::Listening.as_wire(), "listening");
    assert_eq!(SessionState::ListeningLocked.as_wire(), "listeningLocked");
    assert_eq!(SessionState::Cleaning.as_wire(), "cleaning");
    assert_eq!(SessionState::Recovering.as_wire(), "recovering");
    assert_eq!(SessionState::Error.as_wire(), "error");
}

#[test]
fn session_state_serde_matches_as_wire() {
    for state in [
        SessionState::Idle,
        SessionState::Listening,
        SessionState::ListeningLocked,
        SessionState::Cleaning,
        SessionState::Recovering,
        SessionState::Error,
    ] {
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, format!("\"{}\"", state.as_wire()));
    }
}

#[test]
fn session_state_tracker_defaults_to_idle() {
    // A freshly constructed tracker must report `Idle` so a webview
    // that mounts before any orchestrator transition seeds its UI
    // with the correct neutral state instead of an arbitrary
    // previous variant.
    let tracker = SessionStateTracker::new();
    assert_eq!(tracker.get(), SessionState::Idle);
}

#[test]
fn session_state_tracker_round_trips_every_variant() {
    // Round-trip guard against future variants getting silently
    // mapped to `Idle` by the `from_u8` table. If a new state is
    // added to `SessionState` without updating the tracker's
    // encoding, this test fails — preventing the HUD seed from
    // returning the wrong variant for it.
    let tracker = SessionStateTracker::new();
    for state in [
        SessionState::Idle,
        SessionState::Listening,
        SessionState::ListeningLocked,
        SessionState::Cleaning,
        SessionState::Recovering,
        SessionState::Error,
    ] {
        tracker.set(state);
        assert_eq!(tracker.get(), state, "round trip failed for {state:?}");
    }
}

#[test]
fn session_state_tracker_overwrites_on_set() {
    // Successive transitions must clobber the previous value — the
    // tracker is a snapshot, not a queue. The HUD seed only ever
    // wants the latest state.
    let tracker = SessionStateTracker::new();
    tracker.set(SessionState::Listening);
    tracker.set(SessionState::Cleaning);
    tracker.set(SessionState::Idle);
    assert_eq!(tracker.get(), SessionState::Idle);
}

#[test]
fn resolve_env_key_returns_error_when_unset() {
    // Pick a name that's vanishingly unlikely to be set in any
    // environment we run tests in.
    std::env::remove_var("MUNI_TEST_NO_SUCH_KEY");
    match resolve_env_key("MUNI_TEST_NO_SUCH_KEY", MuniError::DeepgramMissingApiKey) {
        Err(MuniError::DeepgramMissingApiKey) => {}
        other => panic!("expected DeepgramMissingApiKey, got {other:?}"),
    }
}

#[test]
fn resolve_env_key_returns_error_for_empty_value() {
    std::env::set_var("MUNI_TEST_EMPTY_KEY", "");
    match resolve_env_key("MUNI_TEST_EMPTY_KEY", MuniError::GroqMissingApiKey) {
        Err(MuniError::GroqMissingApiKey) => {}
        other => panic!("expected GroqMissingApiKey, got {other:?}"),
    }
    std::env::remove_var("MUNI_TEST_EMPTY_KEY");
}

/// Backlog 0009 regression — positive case. A press whose LID
/// decision lands inside the 500–1000 ms window after release
/// must route to Deepgram. Pre-fix `RELEASE_LID_WAIT = 500 ms`,
/// so a 700 ms decision timed out and forced the press into
/// Whisper batch (the ~3–5 s tax this backlog cuts). Post-fix
/// the grace is 1000 ms — long enough to absorb the
/// Whisper-transcribe-turbo floor (~0.95 s wall after press
/// start) on short English presses ("sure", "sure, go ahead").
///
/// Driven on the paused tokio clock so the test resolves in
/// <100 ms of wall time despite the modeled 700 ms LID lag.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn wait_for_decision_admits_700ms_arrival_under_1000ms_grace() {
    let notify = Arc::new(Notify::new());
    let decision = Arc::new(TokioMutex::new(None::<RouterDecision>));

    let n2 = notify.clone();
    let d2 = decision.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        *d2.lock().await = Some(RouterDecision::Deepgram);
        n2.notify_one();
    });

    let (notified, snapshot) =
        wait_for_decision(&notify, &decision, Duration::from_millis(1000)).await;
    assert!(notified, "1000 ms grace must catch the 700 ms decision");
    assert_eq!(snapshot, Some(RouterDecision::Deepgram));
}

/// Plan 039 task 14 — the hybrid-inflight wait must LOOP: a leading
/// english classify firing `decision_notify` first must not abandon a
/// trailing taglish classify that flips the route ~300 ms later. Real-time
/// (not paused) because `await_hybrid_inflight_flip`'s budget uses a real
/// `Instant`.
#[tokio::test(flavor = "multi_thread")]
async fn hybrid_inflight_wait_flips_on_trailing_taglish_after_leading_english() {
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));
    let notify = Arc::new(Notify::new());
    let inflight = Arc::new(AtomicUsize::new(2));

    // Leading classify: english, no flip; completes first and fires notify.
    let (n, i) = (notify.clone(), inflight.clone());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        i.fetch_sub(1, Ordering::SeqCst);
        n.notify_waiters();
    });
    // Trailing classify: taglish; flips to Whisper ~300 ms after the leading.
    let (d, n, i) = (decision.clone(), notify.clone(), inflight.clone());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(350)).await;
        *d.lock().await = Some(RouterDecision::Whisper);
        i.fetch_sub(1, Ordering::SeqCst);
        n.notify_waiters();
    });

    let flipped = await_hybrid_inflight_flip(
        &decision,
        &notify,
        &inflight,
        Duration::from_millis(TRIGGER_REPASS_WAIT_MS),
    )
    .await;
    assert!(
        flipped,
        "a leading english notify must not abandon the trailing taglish classify"
    );
    assert_eq!(*decision.lock().await, Some(RouterDecision::Whisper));
}

/// Plan 039 task 14 — the loop returns as soon as all inner classifies
/// drain without a flip, rather than blocking the full 1500 ms budget.
#[tokio::test(flavor = "multi_thread")]
async fn hybrid_inflight_wait_returns_early_when_classifies_drain_without_flip() {
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));
    let notify = Arc::new(Notify::new());
    let inflight = Arc::new(AtomicUsize::new(1));

    let (n, i) = (notify.clone(), inflight.clone());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        i.fetch_sub(1, Ordering::SeqCst);
        n.notify_waiters();
    });

    let started = Instant::now();
    let flipped = await_hybrid_inflight_flip(
        &decision,
        &notify,
        &inflight,
        Duration::from_millis(TRIGGER_REPASS_WAIT_MS),
    )
    .await;
    assert!(!flipped, "no classify flipped the route");
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "must return when inflight drains, not wait the full 1500 ms budget"
    );
}

/// Plan 039 task 13 — the `watch`-based release signal wakes EVERY waiter
/// on a single fire (broadcast) and is observed even by a receiver that
/// only checks after the fire (sticky). The old `Notify::notify_one` woke
/// exactly one of the concurrent audio-LID-pass / hybrid waiters, starving
/// the other until the 1 s `RELEASE_LID_WAIT` default.
#[tokio::test(flavor = "multi_thread")]
async fn release_signal_wakes_every_waiter_and_is_sticky() {
    // Mirror production: keep ONLY the sender (the initial receiver is
    // dropped at `watch::channel(false).0`), so every fire below happens
    // with a zero (or dropping-to-zero) receiver count. This is the case
    // that `send` would silently drop and `send_replace` must survive.
    let tx = watch::channel(false).0;
    // Two concurrent waiters (audio-LID pass + hybrid), each its own rx.
    let mut rx_a = tx.subscribe();
    let mut rx_b = tx.subscribe();
    let a = tokio::spawn(async move { released(&mut rx_a).await });
    let b = tokio::spawn(async move { released(&mut rx_b).await });
    // Let both register their awaits.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // A SINGLE fire must wake BOTH — with `notify_one` one would starve.
    tx.send_replace(true);
    tokio::time::timeout(Duration::from_millis(500), async {
        a.await.unwrap();
        b.await.unwrap();
    })
    .await
    .expect("both waiters must observe a single release fire");

    // Sticky under the production zero-receiver pattern: reset to `false`,
    // fire with EVERY receiver dropped, THEN subscribe. `send` would return
    // Err and leave the value `false`, hanging this waiter until timeout;
    // `send_replace` stores `true` so the late subscriber resolves at once.
    tx.send_replace(false);
    tx.send_replace(true);
    let mut late = tx.subscribe();
    tokio::time::timeout(Duration::from_millis(200), released(&mut late))
        .await
        .expect("a late subscriber must still observe the sticky release");
}

/// Backlog 0009 regression — negative case. With the OLD 500 ms
/// grace the same 700 ms decision falls outside the window and
/// the press defaults to Whisper. Pinning this guards against an
/// accidental revert of `RELEASE_LID_WAIT` masquerading as a
/// "tighten the budget" cleanup.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn wait_for_decision_misses_700ms_arrival_under_500ms_grace() {
    let notify = Arc::new(Notify::new());
    let decision = Arc::new(TokioMutex::new(None::<RouterDecision>));

    let n2 = notify.clone();
    let d2 = decision.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        *d2.lock().await = Some(RouterDecision::Deepgram);
        n2.notify_one();
    });

    let (notified, snapshot) =
        wait_for_decision(&notify, &decision, Duration::from_millis(500)).await;
    assert!(
        !notified,
        "500 ms grace must time out before the 700 ms decision lands"
    );
    assert_eq!(snapshot, None);
}

// ---- feature 021 fix 2026-05-18: override_or_commit_to_whisper_via_hybrid

/// Pre-commit case: when `*g == None` (audio-LID hasn't committed
/// yet) and the hybrid lands `taglish`, the new helper must
/// commit Whisper directly — pre-empting whatever audio-LID's
/// next window would have written.
///
/// This is the fix for the 4/7 dogfood retries of "Hindi ko alam
/// exactly..." on 2026-05-18 where the strict override no-op'd
/// (because route was None) and audio-LID's next English window
/// then committed Deepgram, dropping the leading Tagalog.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn override_or_commit_to_whisper_via_hybrid_commits_when_cell_is_none() {
    let notify = Arc::new(Notify::new());
    let decision = Arc::new(TokioMutex::new(None::<RouterDecision>));
    let committed = Arc::new(AtomicBool::new(false));

    let flipped =
        DictationSession::override_or_commit_to_whisper_via_hybrid(&decision, &notify, &committed)
            .await;

    assert!(
        flipped,
        "pre-commit (None) state must accept hybrid taglish and commit Whisper"
    );
    assert_eq!(*decision.lock().await, Some(RouterDecision::Whisper));
}

/// Override case: when `*g == Some(Deepgram)`, the helper must
/// flip to Whisper (same semantics as the strict override used by
/// drift detector and feat/019).
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn override_or_commit_to_whisper_via_hybrid_flips_committed_deepgram() {
    let notify = Arc::new(Notify::new());
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));
    let committed = Arc::new(AtomicBool::new(false));

    let flipped =
        DictationSession::override_or_commit_to_whisper_via_hybrid(&decision, &notify, &committed)
            .await;

    assert!(flipped, "committed Deepgram must flip to Whisper");
    assert_eq!(*decision.lock().await, Some(RouterDecision::Whisper));
}

/// Already-Whisper case: the helper must no-op (and report
/// `false`) — the cell is already on the safe path.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn override_or_commit_to_whisper_via_hybrid_noops_on_already_whisper() {
    let notify = Arc::new(Notify::new());
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Whisper)));
    let committed = Arc::new(AtomicBool::new(false));

    let flipped =
        DictationSession::override_or_commit_to_whisper_via_hybrid(&decision, &notify, &committed)
            .await;

    assert!(!flipped, "already-Whisper cell must report no-op");
    assert_eq!(*decision.lock().await, Some(RouterDecision::Whisper));
}

/// Finalize-raced case: when `committed.load() == true`, the
/// orchestrator's `finalize_auto_detect` has already locked the
/// route. The helper must NOT mutate the cell.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn override_or_commit_to_whisper_via_hybrid_noops_when_finalize_committed() {
    let notify = Arc::new(Notify::new());
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));
    let committed = Arc::new(AtomicBool::new(true));

    let flipped =
        DictationSession::override_or_commit_to_whisper_via_hybrid(&decision, &notify, &committed)
            .await;

    assert!(
        !flipped,
        "committed=true must short-circuit before mutation"
    );
    assert_eq!(
        *decision.lock().await,
        Some(RouterDecision::Deepgram),
        "cell must remain untouched"
    );
}

/// Finalize-raced case with `None` cell: same — the helper must
/// NOT silently commit Whisper after finalize has locked the
/// route. Without this guard a late hybrid taglish verdict could
/// flip the cell after finalize has already started transcribing.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn override_or_commit_to_whisper_via_hybrid_noops_when_finalize_committed_and_cell_none() {
    let notify = Arc::new(Notify::new());
    let decision = Arc::new(TokioMutex::new(None::<RouterDecision>));
    let committed = Arc::new(AtomicBool::new(true));

    let flipped =
        DictationSession::override_or_commit_to_whisper_via_hybrid(&decision, &notify, &committed)
            .await;

    assert!(
        !flipped,
        "committed=true must short-circuit even when cell is None"
    );
    assert_eq!(*decision.lock().await, None);
}

/// Notify-waiters fires only on success. Mirrors the existing
/// `override_decision_deepgram_to_whisper` semantics: a no-op must
/// not wake a waiter on the release path (would race the bounded
/// wait into an early exit with no actual route change).
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn override_or_commit_to_whisper_via_hybrid_notify_only_fires_on_success() {
    let notify = Arc::new(Notify::new());
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Whisper)));
    let committed = Arc::new(AtomicBool::new(false));

    let n2 = notify.clone();
    let waiter = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_millis(50), n2.notified())
            .await
            .is_ok()
    });

    // Yield once so the waiter registers.
    tokio::time::sleep(Duration::from_millis(1)).await;

    let flipped =
        DictationSession::override_or_commit_to_whisper_via_hybrid(&decision, &notify, &committed)
            .await;
    assert!(!flipped);

    let fired = waiter.await.expect("waiter joined");
    assert!(
        !fired,
        "no-op must not wake waiters (would race the release path)"
    );
}

/// Backlog 0011 regression — Bug 2 snapshot fast-path.
///
/// When the LID task commits its decision **before** the
/// orchestrator reaches `wait_for_decision` (typical for short
/// non-English presses where pass#1 fires while the press is
/// still active), `Notify::notify_waiters` finds no registered
/// waiter and the notification is lost. The pre-fix code then
/// blocked the full grace window — wasting ~1 s on every short
/// non-English press. The fast-path snapshots the mutex on entry
/// and returns immediately when the decision is already set.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn wait_for_decision_returns_immediately_when_decision_already_set() {
    let notify = Arc::new(Notify::new());
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));

    let started = tokio::time::Instant::now();
    let (notified, snapshot) =
        wait_for_decision(&notify, &decision, Duration::from_millis(1000)).await;
    let elapsed = started.elapsed();

    assert!(notified, "snapshot fast-path must report success");
    assert_eq!(snapshot, Some(RouterDecision::Deepgram));
    assert!(
        elapsed < Duration::from_millis(1),
        "fast-path must return without waiting; elapsed={:?}",
        elapsed
    );
}

/// Backlog 0012 — happy path. When Groq's pass#2 has committed
/// `Some(Whisper)` and Gemini's parallel classify returns
/// `english`, the override flips the cell to `Some(Deepgram)`,
/// notifies the release path, and reports `true`.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn override_flips_whisper_to_deepgram_when_gemini_lands_first() {
    let notify = Arc::new(Notify::new());
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Whisper)));

    let flipped = DictationSession::override_decision_groq_to_gemini(
        &decision,
        &notify,
        RouterDecision::Deepgram,
    )
    .await;

    assert!(flipped, "override must report flip on Whisper→Deepgram");
    assert_eq!(*decision.lock().await, Some(RouterDecision::Deepgram));
}

/// Backlog 0012 — idempotent guard. If the cell is already
/// `Some(Deepgram)` (e.g. pass#1 committed English directly), the
/// override is a no-op. Returning `false` keeps the caller from
/// double-logging "override applied".
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn override_skips_when_already_deepgram() {
    let notify = Arc::new(Notify::new());
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));

    let flipped = DictationSession::override_decision_groq_to_gemini(
        &decision,
        &notify,
        RouterDecision::Deepgram,
    )
    .await;

    assert!(!flipped, "override on already-Deepgram cell must no-op");
    assert_eq!(*decision.lock().await, Some(RouterDecision::Deepgram));
}

/// Backlog 0012 — race guard. Gemini may classify before Groq has
/// committed pass#2 (e.g. Gemini got served a cached response).
/// In that case the override has no Groq verdict to flip and must
/// no-op — Groq's eventual `set_decision` is what routes the
/// press.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn override_skips_when_decision_not_yet_committed() {
    let notify = Arc::new(Notify::new());
    let decision = Arc::new(TokioMutex::new(None::<RouterDecision>));

    let flipped = DictationSession::override_decision_groq_to_gemini(
        &decision,
        &notify,
        RouterDecision::Deepgram,
    )
    .await;

    assert!(!flipped, "override on uncommitted cell must no-op");
    assert_eq!(*decision.lock().await, None);
}

/// Backlog 0012 — direction matrix. Only `(Some(Whisper),
/// Deepgram)` is allowed to flip. Everything else is a no-op,
/// protecting scenario 6 (long-Taglish) where Gemini cannot be
/// trusted to downgrade Groq's English verdict.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn override_only_flips_whisper_to_deepgram_not_other_directions() {
    // (Some(Deepgram), Whisper) — Gemini is not allowed to
    // downgrade Groq's English verdict.
    let notify = Arc::new(Notify::new());
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));
    let flipped = DictationSession::override_decision_groq_to_gemini(
        &decision,
        &notify,
        RouterDecision::Whisper,
    )
    .await;
    assert!(!flipped, "Deepgram→Whisper override must be rejected");
    assert_eq!(*decision.lock().await, Some(RouterDecision::Deepgram));

    // (Some(Whisper), Whisper) — same-route override is a no-op.
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Whisper)));
    let flipped = DictationSession::override_decision_groq_to_gemini(
        &decision,
        &notify,
        RouterDecision::Whisper,
    )
    .await;
    assert!(!flipped, "Whisper→Whisper override must no-op");
    assert_eq!(*decision.lock().await, Some(RouterDecision::Whisper));

    // (None, Deepgram) — already covered by the dedicated test;
    // included here for matrix-completeness.
    let decision = Arc::new(TokioMutex::new(None::<RouterDecision>));
    let flipped = DictationSession::override_decision_groq_to_gemini(
        &decision,
        &notify,
        RouterDecision::Deepgram,
    )
    .await;
    assert!(!flipped, "None→Deepgram override must no-op");
    assert_eq!(*decision.lock().await, None);
}

// ---- feature 021: Gladia fallback for Whisper failures ----------------
//
// Regression tests for two bugs found in dogfood 2026-05-25:
//   1. The amber `Recovering` HUD pill never showed during the
//      cross-provider rescue — the user got the dim `Cleaning`
//      pill instead, making a 5–7 s rescue indistinguishable from
//      a normal post-release cleanup wait.
//   2. Long presses (>~5 s of audio) failed in `finalize()` with
//      `stop_recording send timed out after 1s` because the whole
//      buffer was pushed as a single ~325 KB binary frame —
//      oversaturating macOS's default ~128 KB SO_SNDBUF and leaving
//      residual bytes draining when the tiny `stop_recording` text
//      frame tried to flush.

/// Spawn a localhost mock Gladia WS server that counts non-empty
/// binary frames received before responding to `stop_recording`
/// with a final transcript. The returned counter lets the test
/// assert chunking behavior — a regression to "one giant frame"
/// would drop this back to 1.
async fn spawn_chunk_counting_mock_ws() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock ws listener");
    let addr = listener.local_addr().expect("local addr");
    let url = format!("ws://{addr}/v2/live/session");
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_inner = Arc::clone(&counter);
    tokio::spawn(async move {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::accept_async;
        use tokio_tungstenite::tungstenite::Message;
        let (stream, _peer) = listener.accept().await.expect("accept");
        let mut ws = accept_async(stream).await.expect("ws handshake");
        while let Some(msg) = ws.next().await {
            match msg.expect("ws read") {
                Message::Binary(b) if !b.is_empty() => {
                    counter_inner.fetch_add(1, Ordering::SeqCst);
                }
                Message::Text(t) if t.contains("stop_recording") => {
                    ws.send(Message::Text(
                        serde_json::json!({
                            "type": "transcript",
                            "data": {
                                "is_final": true,
                                "utterance": {
                                    "text": "fallback transcript",
                                    "language": "tl"
                                },
                                "audio_duration": 3.0
                            }
                        })
                        .to_string(),
                    ))
                    .await
                    .expect("send transcript");
                    break;
                }
                _ => continue,
            }
        }
        let _ = ws.close(None).await;
    });
    (url, counter)
}

#[tokio::test(flavor = "current_thread")]
async fn gladia_fallback_emits_recovering_and_chunks_large_buffer() {
    use std::sync::atomic::Ordering;
    let _g = crate::secrets::env_var_test_lock().lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Mock WS that counts binary frames + replies to stop_recording.
    let (ws_url, frame_count) = spawn_chunk_counting_mock_ws().await;

    // Mock POST /v2/live → returns the WS URL above.
    let post_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/live"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "test-fallback-session",
            "url": ws_url,
        })))
        .mount(&post_server)
        .await;

    // Point `GladiaClient::open` at the mock POST and provide a
    // fake key so `secrets::get(GLADIA_ACCOUNT)` resolves.
    std::env::set_var(
        crate::gladia::POST_ENDPOINT_OVERRIDE_ENV,
        format!("{}/v2/live", post_server.uri()),
    );
    std::env::set_var(crate::secrets::GLADIA_ENV_VAR, "test-gladia-key");

    let injector = Arc::new(MockInjector::new());
    let (session, _events, states, _injector) = session_with(None, None, injector);

    // 50_000 samples = ~3.1 s of audio at 16 kHz, large enough to
    // span ⌈50_000 / FALLBACK_CHUNK_SAMPLES (16_000)⌉ = 4 chunks.
    let samples = vec![0_i16; 50_000];
    let original = MuniError::GroqConnectionFailed {
        reason: "test forced failure".into(),
    };
    let result = session
        .attempt_gladia_fallback_transcribe(&samples, &original)
        .await;

    std::env::remove_var(crate::gladia::POST_ENDPOINT_OVERRIDE_ENV);
    std::env::remove_var(crate::secrets::GLADIA_ENV_VAR);

    assert_eq!(result.as_deref(), Some("fallback transcript"));

    // Regression guard for the HUD fix — Recovering must appear.
    let trail = states.lock().expect("states poisoned").clone();
    assert!(
        trail.contains(&SessionState::Recovering),
        "expected Recovering in state trail, got {trail:?}",
    );

    // Regression guard for the chunking fix — more than one frame.
    let frames = frame_count.load(Ordering::SeqCst);
    assert!(
        frames > 1,
        "expected chunked send (>1 binary frame), got {frames}",
    );
}

/// Regression test for the "Quiet HUD pill → Loud notification"
/// fix: when Groq Whisper fails AND the Gladia rescue can't
/// transcribe either (key missing, POST refused, etc.), the
/// orchestrator must emit `TranscriptionUnavailable` — the
/// backend-agnostic Loud variant that routes to a native macOS
/// notification — not the original Quiet `GroqConnectionFailed`.
/// Without this guard, a flip back to `self.emit_error(&err)` at
/// the call site would silently downgrade the surface and most
/// users would never see the failure (Muni's main window is
/// backgrounded during dictation, so the HUD pill is easy to miss).
#[tokio::test(flavor = "current_thread")]
async fn rescue_via_gladia_emits_transcription_unavailable_when_gladia_open_fails() {
    let _g = crate::secrets::env_var_test_lock().lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Force `GladiaClient::open` to fail: the POST endpoint returns
    // a server error, which `GladiaClient::open` surfaces as
    // `GladiaServerError`, which `attempt_gladia_fallback_transcribe`
    // maps to `None` — exercising the terminal branch.
    let post_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/live"))
        .respond_with(ResponseTemplate::new(500).set_body_string("forced fail"))
        .mount(&post_server)
        .await;
    std::env::set_var(
        crate::gladia::POST_ENDPOINT_OVERRIDE_ENV,
        format!("{}/v2/live", post_server.uri()),
    );
    std::env::set_var(crate::secrets::GLADIA_ENV_VAR, "test-gladia-key");

    let injector = Arc::new(MockInjector::new());
    let (session, events, states, _injector) = session_with(None, None, injector);

    let samples = vec![0_i16; 16_000];
    let original = MuniError::GroqConnectionFailed {
        reason: "test forced Groq failure".into(),
    };
    let result = session
        .rescue_via_gladia_or_emit_terminal(&samples, &original)
        .await;

    std::env::remove_var(crate::gladia::POST_ENDPOINT_OVERRIDE_ENV);
    std::env::remove_var(crate::secrets::GLADIA_ENV_VAR);

    assert!(
        result.is_none(),
        "expected None when Gladia rescue can't transcribe; got {result:?}",
    );

    // The user-facing error event must carry the backend-agnostic
    // TranscriptionUnavailable copy, NOT the raw Groq error.
    let recorded = events.lock().expect("events poisoned").clone();
    let error_events: Vec<&String> = recorded
        .iter()
        .filter_map(|(e, p)| (e == EVENT_TRANSCRIPT_ERROR).then_some(p))
        .collect();
    assert_eq!(
        error_events.len(),
        1,
        "expected exactly one TRANSCRIPT_ERROR event, got {error_events:?}",
    );
    let payload = error_events[0];
    assert!(
        payload.contains("Couldn't transcribe"),
        "expected TranscriptionUnavailable user_message, got {payload:?}",
    );
    // Negative assertion: the raw Groq error copy must NOT appear —
    // a flip back to `emit_error(&err)` would leak it through.
    assert!(
        !payload.contains("Groq"),
        "expected backend-agnostic copy, got {payload:?}",
    );

    let trail = states.lock().expect("states poisoned").clone();
    assert!(
        trail.contains(&SessionState::Error),
        "expected Error in state trail, got {trail:?}",
    );
}

// ---- feature 019: confidence-trigger helpers --------------------------

/// Serialise env-var mutating tests in this module so parallel
/// runs of `cargo test` don't trample each other's reads/writes
/// of `MUNI_LID_CONFIDENCE_TRIGGER*`.
fn confidence_trigger_env_lock() -> &'static std::sync::Mutex<()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

fn clear_confidence_trigger_env_vars() {
    std::env::remove_var(MUNI_LID_CONFIDENCE_TRIGGER_ENV);
    std::env::remove_var(MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD_ENV);
    std::env::remove_var(MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE_ENV);
    std::env::remove_var(MUNI_LID_CONFIDENCE_TRIGGER_SLICE_SECONDS_ENV);
}

#[test]
fn load_confidence_trigger_config_defaults_when_env_unset() {
    let _g = confidence_trigger_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    clear_confidence_trigger_env_vars();

    let cfg = load_confidence_trigger_config();
    assert!(!cfg.enabled, "feature defaults to off");
    assert_eq!(cfg.threshold, DEFAULT_CONFIDENCE_TRIGGER_THRESHOLD);
    assert_eq!(cfg.consecutive, DEFAULT_CONFIDENCE_TRIGGER_CONSECUTIVE);
    assert_eq!(cfg.slice_samples, DEFAULT_CONFIDENCE_TRIGGER_SLICE_SAMPLES);
}

#[test]
fn load_confidence_trigger_config_parses_env_overrides() {
    let _g = confidence_trigger_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    clear_confidence_trigger_env_vars();
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_ENV, "true");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD_ENV, "0.5");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE_ENV, "4");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_SLICE_SECONDS_ENV, "2.0");

    let cfg = load_confidence_trigger_config();
    assert!(cfg.enabled);
    assert_eq!(cfg.threshold, 0.5);
    assert_eq!(cfg.consecutive, 4);
    assert_eq!(
        cfg.slice_samples,
        (2.0 * TARGET_SAMPLE_RATE as f32) as usize
    );

    clear_confidence_trigger_env_vars();
}

#[test]
fn load_confidence_trigger_config_rejects_out_of_range_threshold() {
    let _g = confidence_trigger_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    clear_confidence_trigger_env_vars();
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_ENV, "1");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD_ENV, "1.5");

    let cfg = load_confidence_trigger_config();
    assert!(cfg.enabled, "enabled flag still parsed");
    assert_eq!(
        cfg.threshold, DEFAULT_CONFIDENCE_TRIGGER_THRESHOLD,
        "out-of-range threshold falls back to default"
    );

    clear_confidence_trigger_env_vars();
}

#[test]
fn load_confidence_trigger_config_rejects_zero_consecutive() {
    let _g = confidence_trigger_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    clear_confidence_trigger_env_vars();
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_ENV, "yes");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE_ENV, "0");

    let cfg = load_confidence_trigger_config();
    assert_eq!(
        cfg.consecutive, DEFAULT_CONFIDENCE_TRIGGER_CONSECUTIVE,
        "consecutive=0 must fall back to default (else fires every chunk)"
    );

    clear_confidence_trigger_env_vars();
}

#[test]
fn load_confidence_trigger_config_rejects_out_of_range_slice_seconds() {
    let _g = confidence_trigger_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    clear_confidence_trigger_env_vars();
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_ENV, "true");
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_SLICE_SECONDS_ENV, "0.1"); // below min

    let cfg = load_confidence_trigger_config();
    assert_eq!(
        cfg.slice_samples, DEFAULT_CONFIDENCE_TRIGGER_SLICE_SAMPLES,
        "below-min slice seconds falls back to default"
    );

    clear_confidence_trigger_env_vars();
}

#[test]
fn load_confidence_trigger_config_off_when_env_falsy() {
    let _g = confidence_trigger_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    clear_confidence_trigger_env_vars();
    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_ENV, "false");
    let cfg = load_confidence_trigger_config();
    assert!(!cfg.enabled, "falsy values must disable the feature");

    std::env::set_var(MUNI_LID_CONFIDENCE_TRIGGER_ENV, "0");
    let cfg = load_confidence_trigger_config();
    assert!(!cfg.enabled);

    clear_confidence_trigger_env_vars();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn override_decision_deepgram_to_whisper_flips_when_uncommitted() {
    let notify = Arc::new(Notify::new());
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));
    let committed = Arc::new(AtomicBool::new(false));

    let flipped =
        DictationSession::override_decision_deepgram_to_whisper(&decision, &notify, &committed)
            .await;

    assert!(flipped, "Deepgram→Whisper must flip when uncommitted");
    assert_eq!(*decision.lock().await, Some(RouterDecision::Whisper));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn override_decision_deepgram_to_whisper_noop_when_committed() {
    let notify = Arc::new(Notify::new());
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram)));
    let committed = Arc::new(AtomicBool::new(true));

    let flipped =
        DictationSession::override_decision_deepgram_to_whisper(&decision, &notify, &committed)
            .await;

    assert!(!flipped, "committed cell must not be overwritten");
    assert_eq!(*decision.lock().await, Some(RouterDecision::Deepgram));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn override_decision_deepgram_to_whisper_noop_when_wrong_expected() {
    let notify = Arc::new(Notify::new());
    let committed = Arc::new(AtomicBool::new(false));

    // None — pass#2 never landed; trigger must not coerce a value.
    let decision = Arc::new(TokioMutex::new(None::<RouterDecision>));
    let flipped =
        DictationSession::override_decision_deepgram_to_whisper(&decision, &notify, &committed)
            .await;
    assert!(!flipped, "None must not be coerced into Whisper");
    assert_eq!(*decision.lock().await, None);

    // Already Whisper — idempotent guard (e.g. pass#1 hybrid committed).
    let decision = Arc::new(TokioMutex::new(Some(RouterDecision::Whisper)));
    let flipped =
        DictationSession::override_decision_deepgram_to_whisper(&decision, &notify, &committed)
            .await;
    assert!(!flipped, "already-Whisper cell must report no-op");
    assert_eq!(*decision.lock().await, Some(RouterDecision::Whisper));
}

#[test]
fn rolling_buffer_push_drops_oldest_at_cap() {
    let mut rb = RollingBuffer::new(4);
    rb.push(&[1, 2, 3, 4]);
    assert_eq!(rb.len(), 4);
    rb.push(&[5, 6]);
    // Oldest two samples (1, 2) dropped; tail is [3, 4, 5, 6].
    assert_eq!(rb.len(), 4);
    assert_eq!(rb.snapshot_last_n_samples(4), vec![3, 4, 5, 6]);
}

#[test]
fn rolling_buffer_push_handles_chunk_larger_than_cap() {
    let mut rb = RollingBuffer::new(4);
    rb.push(&[10, 20, 30, 40, 50, 60]);
    // Tail-only retention prevents unbounded growth.
    assert_eq!(rb.len(), 4);
    assert_eq!(rb.snapshot_last_n_samples(4), vec![30, 40, 50, 60]);
}

#[test]
fn rolling_buffer_snapshot_returns_contiguous_vec() {
    let mut rb = RollingBuffer::new(8);
    rb.push(&[1, 2, 3, 4, 5, 6, 7, 8]);
    // After a push that wraps, snapshot must still produce a
    // contiguous Vec — Whisper expects `&[i16]`.
    rb.push(&[9, 10]);
    let snap = rb.snapshot_last_n_samples(6);
    assert_eq!(snap, vec![5, 6, 7, 8, 9, 10]);
}

#[test]
fn rolling_buffer_snapshot_handles_partial_fill() {
    let rb = RollingBuffer::new(8);
    // Empty buffer returns empty vec, not error.
    assert!(rb.snapshot_last_n_samples(4).is_empty());

    let mut rb = RollingBuffer::new(8);
    rb.push(&[1, 2]);
    // Request more than we have — get what's available.
    let snap = rb.snapshot_last_n_samples(4);
    assert_eq!(snap, vec![1, 2]);
}

// ---- feature 020: audio-LID windowing decision logic ----------------

/// Compact helper: build a `LidLabel` from a short alias used by
/// the windowing-decision tests. `en`/`tl`/`tg` (Taglish) map to
/// the three routable labels; anything else maps to
/// `LidLabel::Other`.
fn lbl(alias: &str) -> LidLabel {
    match alias {
        "en" => LidLabel::English,
        "tl" => LidLabel::Tagalog,
        "tg" => LidLabel::Taglish,
        other => LidLabel::Other(other.to_string()),
    }
}

#[test]
fn audio_lid_proposed_route_maps_label_not_top1_lang() {
    // Dogfood 2026-05-18: this is the bug-fix assertion. A Taglish
    // verdict (label=Taglish, top1=en) must route to Whisper, not
    // Deepgram. Earlier code read top1_lang and routed Taglish to
    // Deepgram → Tagalog content dropped.
    assert_eq!(
        audio_lid_proposed_route(&LidLabel::English),
        Some(RouterDecision::Deepgram)
    );
    assert_eq!(
        audio_lid_proposed_route(&LidLabel::Tagalog),
        Some(RouterDecision::Whisper)
    );
    assert_eq!(
        audio_lid_proposed_route(&LidLabel::Taglish),
        Some(RouterDecision::Whisper)
    );
    // Other labels (cough, silence, noisy ko/id verdicts) yield
    // None → "keep checking" pre-commit, "ignore noise" post-commit.
    assert_eq!(
        audio_lid_proposed_route(&LidLabel::Other("ko".into())),
        None
    );
    assert_eq!(audio_lid_proposed_route(&LidLabel::Other("".into())), None);
}

#[test]
fn other_verdict_does_not_arm_drift_veto_so_override_proceeds() {
    // Plan 039 task 20 — a mid-press hybrid `Other` verdict must NOT
    // arm the symmetric drift veto. `Other` is non-English per the
    // `LidLabel::Other` contract (text_lid.rs), so it can't suppress a
    // legitimate audio-LID drift override to Whisper. Arming on the
    // catch-all token wrongly pinned an ambiguous press to Deepgram.
    assert!(
        !hybrid_verdict_arms_drift_veto(&LidLabel::Other("id".into())),
        "an Other verdict must not arm the veto"
    );
    assert!(
        !hybrid_verdict_arms_drift_veto(&LidLabel::Other(String::new())),
        "an empty Other verdict must not arm the veto"
    );
    // Only an explicit English verdict arms it.
    assert!(hybrid_verdict_arms_drift_veto(&LidLabel::English));
    // Tagalog/Taglish take the override path and never reach the arm
    // site, but confirm they don't arm the veto either.
    assert!(!hybrid_verdict_arms_drift_veto(&LidLabel::Tagalog));
    assert!(!hybrid_verdict_arms_drift_veto(&LidLabel::Taglish));

    // End-to-end of the veto: feed the disarmed bit from an `Other`
    // verdict into the consumer. A Deepgram-committed press at the
    // drift threshold seeing a Tagalog window fires the override to
    // Whisper — the `Other` verdict left the veto disarmed.
    let hybrid_recent_english = hybrid_verdict_arms_drift_veto(&LidLabel::Other("id".into()));
    assert_eq!(
        audio_lid_decide_action(
            &LidLabel::Tagalog,
            0.10,
            Some(RouterDecision::Deepgram),
            0,
            1,
            hybrid_recent_english,
        ),
        AudioLidAction::FireOverrideToWhisper,
        "Other left the veto disarmed → drift override proceeds"
    );

    // Contrast: an English verdict arms the veto, downgrading the same
    // fire to Agree (route pinned to Deepgram). This locks the
    // English-only asymmetry the task introduces.
    let hybrid_recent_english = hybrid_verdict_arms_drift_veto(&LidLabel::English);
    assert_eq!(
        audio_lid_decide_action(
            &LidLabel::Tagalog,
            0.10,
            Some(RouterDecision::Deepgram),
            0,
            1,
            hybrid_recent_english,
        ),
        AudioLidAction::Agree,
        "English armed the veto → override downgraded to Agree"
    );
}

#[test]
fn audio_lid_load_drift_consecutive_returns_default_when_unset() {
    // Use a unique env-var name so concurrent tests don't race the
    // process-wide MUNI_AUDIO_LID_DRIFT_CONSECUTIVE setting.
    // SAFETY: env vars are process-global; tests in this module
    // never set this name elsewhere.
    std::env::remove_var(MUNI_AUDIO_LID_DRIFT_CONSECUTIVE_ENV);
    assert_eq!(
        load_audio_lid_drift_consecutive(),
        DEFAULT_AUDIO_LID_DRIFT_CONSECUTIVE
    );
}

#[test]
fn audio_lid_load_release_drift_fire_floor_returns_default_when_unset() {
    // Mirrors the drift-consecutive env-loader test. Backlog 0048.
    // SAFETY: env vars are process-global; tests in this module
    // never set this name elsewhere.
    std::env::remove_var(MUNI_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR_ENV);
    assert_eq!(
        load_audio_lid_release_drift_fire_floor(),
        DEFAULT_AUDIO_LID_RELEASE_DRIFT_FIRE_FLOOR
    );
}

#[test]
fn audio_lid_load_release_other_as_taglish_returns_default_when_unset() {
    // Backlog 0048 v2. SAFETY: env vars are process-global; tests
    // in this module never set this name elsewhere.
    std::env::remove_var(MUNI_AUDIO_LID_RELEASE_OTHER_AS_TAGLISH_ENV);
    assert_eq!(
        load_audio_lid_release_other_as_taglish(),
        DEFAULT_AUDIO_LID_RELEASE_OTHER_AS_TAGLISH
    );
}

/// Default `p_en` for synthetic labels in the decision-machine
/// tests. Clears the `MIN_P_EN_TO_COMMIT_DEEPGRAM` gate so tests
/// that don't care about the confidence floor reach the commit
/// path. Tests that exercise the gate supply their own value via
/// [`drive_decide_sequence_with_p_en`].
const TEST_DEFAULT_P_EN: f32 = 0.80;

/// Drive `audio_lid_decide_action` through a sequence of synthetic
/// labels and return both the final state and the per-step action
/// sequence. Mirrors the runtime windowing state machine but with
/// zero side effects — no Deepgram, no Mutex, no allocations
/// beyond the action vector. Uses [`TEST_DEFAULT_P_EN`] for every
/// window so the gate doesn't fire — see
/// [`drive_decide_sequence_with_p_en`] for gate tests.
fn drive_decide_sequence(
    labels: &[&str],
    drift_threshold: usize,
) -> (Vec<AudioLidAction>, Option<RouterDecision>, usize) {
    let p_ens: Vec<f32> = labels.iter().map(|_| TEST_DEFAULT_P_EN).collect();
    drive_decide_sequence_with_p_en(labels, &p_ens, drift_threshold)
}

/// Same as [`drive_decide_sequence`] but lets each window's `p_en`
/// be supplied — needed for tests of
/// [`MIN_P_EN_TO_COMMIT_DEEPGRAM`].
fn drive_decide_sequence_with_p_en(
    labels: &[&str],
    p_ens: &[f32],
    drift_threshold: usize,
) -> (Vec<AudioLidAction>, Option<RouterDecision>, usize) {
    assert_eq!(labels.len(), p_ens.len(), "labels and p_ens must align");
    let mut committed: Option<RouterDecision> = None;
    let mut drift: usize = 0;
    let mut actions = Vec::with_capacity(labels.len());
    for (&t, &p_en) in labels.iter().zip(p_ens.iter()) {
        let label = lbl(t);
        let action = audio_lid_decide_action(
            &label,
            p_en,
            committed,
            drift,
            drift_threshold,
            false, // hybrid_recent_english — existing tests exercise pre-feat/028 behavior
        );
        match action {
            AudioLidAction::KeepChecking => {}
            AudioLidAction::Commit(r) => {
                committed = Some(r);
                drift = 0;
            }
            AudioLidAction::Agree => {
                drift = 0;
            }
            AudioLidAction::IgnoreNoise => {
                // Critical: drift counter intentionally preserved
                // — the runtime state machine MUST NOT reset on
                // noise windows. This branch documents that.
            }
            AudioLidAction::IncrementDrift { new_count } => {
                drift = new_count;
            }
            AudioLidAction::FireOverrideToWhisper => {
                committed = Some(RouterDecision::Whisper);
                drift = 0;
            }
        }
        actions.push(action);
    }
    (actions, committed, drift)
}

#[test]
fn audio_lid_decide_commit_on_first_en_window() {
    let (actions, committed, drift) = drive_decide_sequence(&["en"], 1);
    assert_eq!(
        actions,
        vec![AudioLidAction::Commit(RouterDecision::Deepgram)]
    );
    assert_eq!(committed, Some(RouterDecision::Deepgram));
    assert_eq!(drift, 0);
}

#[test]
fn audio_lid_decide_commit_on_first_tl_window() {
    let (actions, committed, drift) = drive_decide_sequence(&["tl"], 1);
    assert_eq!(
        actions,
        vec![AudioLidAction::Commit(RouterDecision::Whisper)]
    );
    assert_eq!(committed, Some(RouterDecision::Whisper));
    assert_eq!(drift, 0);
}

#[test]
fn audio_lid_decide_commit_on_first_taglish_window_routes_to_whisper() {
    // Dogfood 2026-05-18 bug-fix: a Taglish label (top1=en,
    // p_tl ≥ 0.10) must commit Whisper, not Deepgram.
    let (actions, committed, drift) = drive_decide_sequence(&["tg"], 1);
    assert_eq!(
        actions,
        vec![AudioLidAction::Commit(RouterDecision::Whisper)]
    );
    assert_eq!(committed, Some(RouterDecision::Whisper));
    assert_eq!(drift, 0);
}

#[test]
fn audio_lid_decide_keep_checking_then_commit() {
    // First window: label=Other("ko") (cough corruption). Second:
    // clean English.
    let (actions, committed, _drift) = drive_decide_sequence(&["ko", "en"], 1);
    assert_eq!(
        actions,
        vec![
            AudioLidAction::KeepChecking,
            AudioLidAction::Commit(RouterDecision::Deepgram),
        ]
    );
    assert_eq!(committed, Some(RouterDecision::Deepgram));
}

#[test]
fn audio_lid_decide_keep_checking_multiple_then_commit() {
    // Two consecutive Other windows then English.
    let (actions, committed, _drift) = drive_decide_sequence(&["ko", "fr", "en"], 1);
    assert_eq!(
        actions,
        vec![
            AudioLidAction::KeepChecking,
            AudioLidAction::KeepChecking,
            AudioLidAction::Commit(RouterDecision::Deepgram),
        ]
    );
    assert_eq!(committed, Some(RouterDecision::Deepgram));
}

#[test]
fn audio_lid_decide_drift_threshold_one_flips_on_single_disagreement() {
    // Post feature-020-dogfood default: threshold = 1. A single
    // disagreeing post-commit window flips immediately. This
    // mirrors the production default and reflects the
    // correctness > speed asymmetry — dropped Tagalog content is
    // worse than a slow correct paste.
    let (actions, committed, drift) = drive_decide_sequence(&["en", "tl"], 1);
    assert_eq!(
        actions,
        vec![
            AudioLidAction::Commit(RouterDecision::Deepgram),
            AudioLidAction::FireOverrideToWhisper,
        ]
    );
    assert_eq!(committed, Some(RouterDecision::Whisper));
    assert_eq!(drift, 0);
}

#[test]
fn audio_lid_decide_drift_below_threshold_does_not_flip() {
    // Threshold = 2 (rollback / opt-in via env var). A single
    // disagreeing window only accumulates the counter — no fire.
    let (actions, committed, drift) = drive_decide_sequence(&["en", "tl"], 2);
    assert_eq!(
        actions,
        vec![
            AudioLidAction::Commit(RouterDecision::Deepgram),
            AudioLidAction::IncrementDrift { new_count: 1 },
        ]
    );
    assert_eq!(committed, Some(RouterDecision::Deepgram));
    assert_eq!(drift, 1);
}

#[test]
fn audio_lid_decide_drift_at_threshold_two_fires_on_second_disagreement() {
    // Same threshold = 2 rollback scenario: override fires on the
    // *second* consecutive Tagalog window.
    let (actions, committed, drift) = drive_decide_sequence(&["en", "tl", "tl"], 2);
    assert_eq!(
        actions,
        vec![
            AudioLidAction::Commit(RouterDecision::Deepgram),
            AudioLidAction::IncrementDrift { new_count: 1 },
            AudioLidAction::FireOverrideToWhisper,
        ]
    );
    assert_eq!(committed, Some(RouterDecision::Whisper));
    assert_eq!(drift, 0);
}

#[test]
fn audio_lid_decide_whisper_committed_press_never_flips_back_to_deepgram() {
    // Dogfood 2026-05-18: the `whisper → deepgram` override
    // direction was removed because the Whisper commit had already
    // closed the Deepgram socket, leaving a flip-back with nothing
    // to read. After the fix, a Whisper-committed press seeing
    // *any* number of English windows stays on Whisper. The
    // English windows are treated as Agree (no-op, drift counter
    // reset).
    let (actions, committed, drift) = drive_decide_sequence(&["tl", "en", "en", "en", "en"], 1);
    assert_eq!(
        actions,
        vec![
            AudioLidAction::Commit(RouterDecision::Whisper),
            AudioLidAction::Agree,
            AudioLidAction::Agree,
            AudioLidAction::Agree,
            AudioLidAction::Agree,
        ]
    );
    assert_eq!(committed, Some(RouterDecision::Whisper));
    assert_eq!(drift, 0);
}

#[test]
fn audio_lid_decide_agreement_resets_drift_counter() {
    // Threshold = 2 rollback scenario for the agreement-reset
    // case: en commit → tl (drift=1) → en (agreement → reset) →
    // tl (drift=1 again, not 2) → no flip.
    let (actions, committed, drift) = drive_decide_sequence(&["en", "tl", "en", "tl"], 2);
    assert_eq!(
        actions,
        vec![
            AudioLidAction::Commit(RouterDecision::Deepgram),
            AudioLidAction::IncrementDrift { new_count: 1 },
            AudioLidAction::Agree,
            AudioLidAction::IncrementDrift { new_count: 1 },
        ]
    );
    assert_eq!(committed, Some(RouterDecision::Deepgram));
    assert_eq!(drift, 1);
}

#[test]
fn audio_lid_decide_post_commit_noise_preserves_drift_counter() {
    // Threshold = 2 rollback scenario. A mid-press pause produces
    // a noise window (label=Other). Noise windows preserve the
    // drift counter so the next Tagalog window can cross the
    // threshold.
    //
    // Sequence: en commit → tl (drift=1) → noise (preserve) → tl
    // (drift=2 → fire override).
    let (actions, committed, drift) = drive_decide_sequence(&["en", "tl", "ko", "tl"], 2);
    assert_eq!(
        actions,
        vec![
            AudioLidAction::Commit(RouterDecision::Deepgram),
            AudioLidAction::IncrementDrift { new_count: 1 },
            AudioLidAction::IgnoreNoise,
            AudioLidAction::FireOverrideToWhisper,
        ]
    );
    assert_eq!(committed, Some(RouterDecision::Whisper));
    assert_eq!(drift, 0);
}

#[test]
fn audio_lid_decide_post_commit_noise_alone_does_not_change_state() {
    // Post-commit Other window with drift=0 stays at drift=0,
    // committed route unchanged.
    let (actions, committed, drift) = drive_decide_sequence(&["en", "ko"], 1);
    assert_eq!(
        actions,
        vec![
            AudioLidAction::Commit(RouterDecision::Deepgram),
            AudioLidAction::IgnoreNoise,
        ]
    );
    assert_eq!(committed, Some(RouterDecision::Deepgram));
    assert_eq!(drift, 0);
}

// ---- backlog 0048: at-release stale drift commit -------------------
//
// The v1 rule (drift counter) and the v2 rule (last-was-Other) are
// independent axes. Each pair of tests below pins one axis while
// the other is in its "no-fire" state, so a future change to one
// rule doesn't silently flip the other.

#[test]
fn audio_lid_decide_release_drift_zero_is_noop() {
    // Press ended with no drift evidence and last verdict wasn't
    // Other — the fallback paths in the release arm handle
    // finalisation as before.
    assert_eq!(
        audio_lid_decide_release_action(Some(RouterDecision::Deepgram), 0, 1, false, true, false,),
        AudioLidReleaseAction::NoOp
    );
}

#[test]
fn audio_lid_decide_release_drift_one_with_floor_one_fires() {
    // The 0048 v1 reproduction case. Press ended at drift=1.
    // Default floor=1, last-was-Other irrelevant (false here).
    assert_eq!(
        audio_lid_decide_release_action(Some(RouterDecision::Deepgram), 1, 1, false, true, false,),
        AudioLidReleaseAction::FireOverrideToWhisper
    );
}

#[test]
fn audio_lid_decide_release_drift_two_with_floor_one_fires() {
    // Sanity: any drift above the floor fires.
    assert_eq!(
        audio_lid_decide_release_action(Some(RouterDecision::Deepgram), 2, 1, false, true, false,),
        AudioLidReleaseAction::FireOverrideToWhisper
    );
}

#[test]
fn audio_lid_decide_release_drift_one_with_floor_two_is_noop() {
    // Env knob escape hatch: raising the floor disables the v1
    // rule for drift below the new floor.
    assert_eq!(
        audio_lid_decide_release_action(Some(RouterDecision::Deepgram), 1, 2, false, true, false,),
        AudioLidReleaseAction::NoOp
    );
}

#[test]
fn audio_lid_decide_release_drift_two_with_floor_two_fires() {
    // Inclusive lower bound on the drift rule.
    assert_eq!(
        audio_lid_decide_release_action(Some(RouterDecision::Deepgram), 2, 2, false, true, false,),
        AudioLidReleaseAction::FireOverrideToWhisper
    );
}

#[test]
fn audio_lid_decide_release_whisper_committed_never_fires() {
    // Whisper-committed press never flips back to Deepgram. Holds
    // for both the drift rule and the last-was-Other rule.
    assert_eq!(
        audio_lid_decide_release_action(Some(RouterDecision::Whisper), 99, 1, true, true, false,),
        AudioLidReleaseAction::NoOp
    );
}

#[test]
fn audio_lid_decide_release_uncommitted_never_fires() {
    // No Deepgram commit to override.
    assert_eq!(
        audio_lid_decide_release_action(None, 99, 1, true, true, false),
        AudioLidReleaseAction::NoOp
    );
}

// ---- backlog 0048 v2: last-post-commit-was-Other rule --------------

#[test]
fn audio_lid_decide_release_last_was_other_fires_when_knob_on() {
    // The 0048 v2 case: whisper-tiny hallucinated `id`/`ru`/`es`
    // on the Tagalog tail → IgnoreNoise → drift counter stayed at
    // 0 but the last-was-Other bit is set. With the knob on,
    // fire the override.
    assert_eq!(
        audio_lid_decide_release_action(Some(RouterDecision::Deepgram), 0, 1, true, true, false,),
        AudioLidReleaseAction::FireOverrideToWhisper
    );
}

#[test]
fn audio_lid_decide_release_last_was_other_is_noop_when_knob_off() {
    // Knob off → v1 behavior. Last-was-Other doesn't fire on its
    // own.
    assert_eq!(
        audio_lid_decide_release_action(Some(RouterDecision::Deepgram), 0, 1, true, false, false,),
        AudioLidReleaseAction::NoOp
    );
}

#[test]
fn audio_lid_decide_release_both_rules_match_fires() {
    // Both v1 (drift>=floor) and v2 (last-was-Other) match — the
    // function still fires (not double-fire, just one fire). The
    // override helper itself is idempotent.
    assert_eq!(
        audio_lid_decide_release_action(Some(RouterDecision::Deepgram), 1, 1, true, true, false,),
        AudioLidReleaseAction::FireOverrideToWhisper
    );
}

#[test]
fn audio_lid_decide_release_last_was_other_with_whisper_committed_never_fires() {
    // Mirror the Whisper-committed invariant for the v2 rule too.
    assert_eq!(
        audio_lid_decide_release_action(Some(RouterDecision::Whisper), 0, 1, true, true, false,),
        AudioLidReleaseAction::NoOp
    );
}

// ---- backlog 0052: MUNI_AUDIO_LID_HYBRID_VETO_DRIFT resolver -----

// Process-global env state requires a serialising Mutex when tests
// run concurrently. Mirrors the pattern used by the VAD env tests in
// `lib.rs::lib_tests::VAD_ENV_LOCK`.
static AUDIO_LID_HYBRID_VETO_DRIFT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn load_audio_lid_hybrid_veto_drift_default_when_unset() {
    let _guard = AUDIO_LID_HYBRID_VETO_DRIFT_ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    std::env::remove_var(MUNI_AUDIO_LID_HYBRID_VETO_DRIFT_ENV);
    assert_eq!(
        load_audio_lid_hybrid_veto_drift(),
        DEFAULT_AUDIO_LID_HYBRID_VETO_DRIFT
    );
}

#[test]
fn load_audio_lid_hybrid_veto_drift_parses_on() {
    let _guard = AUDIO_LID_HYBRID_VETO_DRIFT_ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for v in ["on", "ON", "true", "TRUE", "1", "yes", "YES"] {
        std::env::set_var(MUNI_AUDIO_LID_HYBRID_VETO_DRIFT_ENV, v);
        assert!(
            load_audio_lid_hybrid_veto_drift(),
            "value {v:?} should parse as true"
        );
    }
    std::env::remove_var(MUNI_AUDIO_LID_HYBRID_VETO_DRIFT_ENV);
}

#[test]
fn load_audio_lid_hybrid_veto_drift_parses_off() {
    let _guard = AUDIO_LID_HYBRID_VETO_DRIFT_ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for v in ["off", "OFF", "false", "FALSE", "0", "no", "NO"] {
        std::env::set_var(MUNI_AUDIO_LID_HYBRID_VETO_DRIFT_ENV, v);
        assert!(
            !load_audio_lid_hybrid_veto_drift(),
            "value {v:?} should parse as false"
        );
    }
    std::env::remove_var(MUNI_AUDIO_LID_HYBRID_VETO_DRIFT_ENV);
}

#[test]
fn load_audio_lid_hybrid_veto_drift_default_when_unparseable() {
    let _guard = AUDIO_LID_HYBRID_VETO_DRIFT_ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    std::env::set_var(MUNI_AUDIO_LID_HYBRID_VETO_DRIFT_ENV, "maybe");
    assert_eq!(
        load_audio_lid_hybrid_veto_drift(),
        DEFAULT_AUDIO_LID_HYBRID_VETO_DRIFT
    );
    std::env::remove_var(MUNI_AUDIO_LID_HYBRID_VETO_DRIFT_ENV);
}

// ---- backlog 0052: symmetric hybrid text-LID veto (mid-press) ----

/// Veto downgrades the threshold-crossing fire to Agree (reset).
#[test]
fn audio_lid_decide_drift_fire_vetoed_when_hybrid_recent_english() {
    // Same scenario as the existing fire test (drift threshold = 1,
    // single tl window post-commit) but with the hybrid veto active.
    // The action MUST downgrade to Agree.
    let action = audio_lid_decide_action(
        &LidLabel::Tagalog,
        0.05,
        Some(RouterDecision::Deepgram),
        0,
        1,
        true, // hybrid_recent_english
    );
    assert_eq!(action, AudioLidAction::Agree);
}

/// Veto OFF preserves the existing fire behavior.
#[test]
fn audio_lid_decide_drift_fire_unaffected_when_hybrid_recent_english_false() {
    let action = audio_lid_decide_action(
        &LidLabel::Tagalog,
        0.05,
        Some(RouterDecision::Deepgram),
        0,
        1,
        false,
    );
    assert_eq!(action, AudioLidAction::FireOverrideToWhisper);
}

/// Veto does NOT block the IncrementDrift (sub-threshold) action.
/// Justification: the at-release rule reads the drift counter
/// independently; vetoing the increment would suppress useful
/// release-time evidence.
#[test]
fn audio_lid_decide_drift_increment_unaffected_by_veto() {
    let action = audio_lid_decide_action(
        &LidLabel::Tagalog,
        0.05,
        Some(RouterDecision::Deepgram),
        0,
        2, // threshold = 2 → first disagreement increments, doesn't fire
        true,
    );
    assert_eq!(action, AudioLidAction::IncrementDrift { new_count: 1 });
}

/// Veto does NOT block first-window commit decisions. The veto
/// only applies to the (Some(Deepgram), Some(Whisper)) drift arm.
#[test]
fn audio_lid_decide_first_commit_unaffected_by_veto() {
    let action = audio_lid_decide_action(&LidLabel::English, 0.95, None, 0, 1, true);
    assert_eq!(action, AudioLidAction::Commit(RouterDecision::Deepgram));
}

/// Veto does NOT block first-Tagalog commit decisions either.
/// (Edge case: if hybrid had already armed before audio-LID's first
/// window landed, we still want the first verdict to commit Whisper.
/// The atomic should not be `true` in this case in practice — the
/// hybrid is spawned by run_audio_lid_pass AFTER the first audio-LID
/// classify — but pin the semantic explicitly.)
#[test]
fn audio_lid_decide_first_taglish_commit_unaffected_by_veto() {
    let action = audio_lid_decide_action(&LidLabel::Taglish, 0.40, None, 0, 1, true);
    assert_eq!(action, AudioLidAction::Commit(RouterDecision::Whisper));
}

/// Replicates the dogfood Press 7 scenario from
/// `docs/findings/006_feat_027_post_implementation_dogfood.md`:
/// drift counter at 1, second tl window arrives, hybrid said English.
/// Without veto → FireOverrideToWhisper. With veto → Agree.
#[test]
fn audio_lid_decide_replicates_backlog_0052_press_7_with_veto() {
    // Without veto: replicates the false positive.
    let without_veto = audio_lid_decide_action(
        &LidLabel::Tagalog,
        0.04,
        Some(RouterDecision::Deepgram),
        1, // already incremented once
        2, // threshold = 2
        false,
    );
    assert_eq!(without_veto, AudioLidAction::FireOverrideToWhisper);

    // With veto: blocks the fire.
    let with_veto = audio_lid_decide_action(
        &LidLabel::Tagalog,
        0.04,
        Some(RouterDecision::Deepgram),
        1,
        2,
        true,
    );
    assert_eq!(with_veto, AudioLidAction::Agree);
}

// ---- backlog 0052: symmetric hybrid text-LID veto (at-release) ----

/// Veto blocks the v1 drift-counter rule.
#[test]
fn audio_lid_decide_release_drift_one_with_floor_one_vetoed() {
    assert_eq!(
        audio_lid_decide_release_action(
            Some(RouterDecision::Deepgram),
            1,
            1,
            false,
            true,
            true, // hybrid_recent_english
        ),
        AudioLidReleaseAction::NoOp
    );
}

/// Veto blocks the v2 last-was-Other rule.
#[test]
fn audio_lid_decide_release_last_was_other_vetoed() {
    assert_eq!(
        audio_lid_decide_release_action(
            Some(RouterDecision::Deepgram),
            0,
            1,
            true, // last_was_other
            true, // treat_other_as_taglish (feat/026 v2)
            true, // hybrid_recent_english
        ),
        AudioLidReleaseAction::NoOp
    );
}

/// Veto OFF preserves the existing fire behavior.
#[test]
fn audio_lid_decide_release_unaffected_when_hybrid_recent_english_false() {
    assert_eq!(
        audio_lid_decide_release_action(Some(RouterDecision::Deepgram), 1, 1, false, true, false,),
        AudioLidReleaseAction::FireOverrideToWhisper
    );
}

/// Whisper-committed press unaffected by veto bit (no Deepgram to
/// override anyway).
#[test]
fn audio_lid_decide_release_whisper_committed_unaffected_by_veto() {
    assert_eq!(
        audio_lid_decide_release_action(Some(RouterDecision::Whisper), 99, 1, true, true, true,),
        AudioLidReleaseAction::NoOp
    );
}

// ---- feature 020 fix 3: MIN_P_EN_TO_COMMIT_DEEPGRAM gate -----------

#[test]
fn audio_lid_decide_weak_english_does_not_commit_deepgram() {
    // Dogfood 2026-05-18: real Taglish presses whose first window
    // saw English with low confidence (p_en in [0.30, 0.50]) were
    // committing Deepgram and dropping the Tagalog tail. The
    // confidence gate defers commit; the second window — which
    // typically carries the actual Tagalog signal — gets to
    // commit Whisper.
    //
    // Window 1: en p_en=0.40 → below gate → KeepChecking.
    // Window 2: tl → commit Whisper. ✓
    let (actions, committed, _drift) =
        drive_decide_sequence_with_p_en(&["en", "tl"], &[0.40, TEST_DEFAULT_P_EN], 1);
    assert_eq!(
        actions,
        vec![
            AudioLidAction::KeepChecking,
            AudioLidAction::Commit(RouterDecision::Whisper),
        ]
    );
    assert_eq!(committed, Some(RouterDecision::Whisper));
}

#[test]
fn audio_lid_decide_strong_english_commits_deepgram() {
    // Above the gate → Commit Deepgram immediately on first window.
    let (actions, committed, _drift) = drive_decide_sequence_with_p_en(&["en"], &[0.80], 1);
    assert_eq!(
        actions,
        vec![AudioLidAction::Commit(RouterDecision::Deepgram)]
    );
    assert_eq!(committed, Some(RouterDecision::Deepgram));
}

#[test]
fn audio_lid_decide_gate_at_exactly_the_floor_commits() {
    // The gate is `p_en < MIN_P_EN_TO_COMMIT_DEEPGRAM`, so a value
    // *exactly equal* to the floor passes. This pins the inclusive
    // semantics so a future tuning of MIN_P_EN_TO_COMMIT_DEEPGRAM
    // doesn't silently flip the boundary.
    let (actions, committed, _drift) =
        drive_decide_sequence_with_p_en(&["en"], &[MIN_P_EN_TO_COMMIT_DEEPGRAM], 1);
    assert_eq!(
        actions,
        vec![AudioLidAction::Commit(RouterDecision::Deepgram)]
    );
    assert_eq!(committed, Some(RouterDecision::Deepgram));
}

#[test]
fn audio_lid_decide_gate_only_applies_to_deepgram_commits() {
    // A Tagalog first window with low p_en still commits Whisper —
    // the gate only protects against weak English commits, not
    // weak Tagalog commits (those are already routing to the safe
    // path).
    let (actions, committed, _drift) = drive_decide_sequence_with_p_en(&["tl"], &[0.01], 1);
    assert_eq!(
        actions,
        vec![AudioLidAction::Commit(RouterDecision::Whisper)]
    );
    assert_eq!(committed, Some(RouterDecision::Whisper));
}

#[test]
fn audio_lid_decide_gate_only_applies_pre_commit() {
    // Once a Deepgram commit has landed, subsequent English
    // windows with low p_en are post-commit agreement (drift
    // reset), not "keep checking". The gate is only a pre-commit
    // filter — a flapping low-confidence stream shouldn't keep
    // perturbing an already-committed route.
    let (actions, committed, _drift) =
        drive_decide_sequence_with_p_en(&["en", "en"], &[TEST_DEFAULT_P_EN, 0.30], 1);
    assert_eq!(
        actions,
        vec![
            AudioLidAction::Commit(RouterDecision::Deepgram),
            AudioLidAction::Agree
        ]
    );
    assert_eq!(committed, Some(RouterDecision::Deepgram));
}

// ---- feature 025: should_fire_audio_lid_window ----------------

#[test]
fn should_fire_audio_lid_window_returns_false_when_buffer_not_yet_full_first_window() {
    // First-window predicate: rolling has less than the window
    // length → not ready, regardless of gate or cadence.
    assert!(!should_fire_audio_lid_window(
        false,
        0,
        AUDIO_LID_WINDOW_SAMPLES - 1,
        0,
        false,
    ));
    assert!(!should_fire_audio_lid_window(
        false,
        0,
        AUDIO_LID_WINDOW_SAMPLES - 1,
        AUDIO_LID_WINDOW_SAMPLES,
        true,
    ));
}

#[test]
fn should_fire_audio_lid_window_returns_true_for_first_window_with_full_buffer() {
    // First-window: rolling >= window → fire, regardless of
    // accumulated_since_last_window (which is meaningless pre-first).
    assert!(should_fire_audio_lid_window(
        false,
        0,
        AUDIO_LID_WINDOW_SAMPLES,
        0,
        false,
    ));
    assert!(should_fire_audio_lid_window(
        false,
        0,
        AUDIO_LID_WINDOW_SAMPLES * 2,
        0,
        false,
    ));
}

#[test]
fn should_fire_audio_lid_window_first_window_ignores_gate_even_when_active() {
    // First-window protection: even when the gate is active and
    // the entire pre-classify span is "silent" (samples_since
    // matches the window size), the first window still fires —
    // protects route-commit latency on silent starts.
    assert!(should_fire_audio_lid_window(
        false,
        0,
        AUDIO_LID_WINDOW_SAMPLES,
        AUDIO_LID_WINDOW_SAMPLES,
        true,
    ));
    assert!(should_fire_audio_lid_window(
        false,
        0,
        AUDIO_LID_WINDOW_SAMPLES,
        usize::MAX,
        true,
    ));
}

#[test]
fn should_fire_audio_lid_window_returns_false_when_advance_cadence_not_elapsed() {
    // Post-first-window: cadence hasn't elapsed → wait, even if
    // rolling is far past the window length and the gate is off.
    assert!(!should_fire_audio_lid_window(
        true,
        AUDIO_LID_WINDOW_ADVANCE_SAMPLES - 1,
        AUDIO_LID_WINDOW_SAMPLES * 2,
        0,
        false,
    ));
}

#[test]
fn should_fire_audio_lid_window_returns_true_when_gate_disabled_and_buffer_ready() {
    // Gate off, post-first-window, cadence elapsed, full rolling:
    // fire even when the would-be silence counter is enormous —
    // proves the gate-off path is bit-identical to pre-feat/025.
    assert!(should_fire_audio_lid_window(
        true,
        AUDIO_LID_WINDOW_ADVANCE_SAMPLES,
        AUDIO_LID_WINDOW_SAMPLES,
        999_999,
        false,
    ));
}

#[test]
fn should_fire_audio_lid_window_returns_false_when_gate_active_and_candidate_window_all_silent() {
    // Gate on, candidate window's worth of samples is all silent:
    // skip.
    assert!(!should_fire_audio_lid_window(
        true,
        AUDIO_LID_WINDOW_ADVANCE_SAMPLES,
        AUDIO_LID_WINDOW_SAMPLES,
        AUDIO_LID_WINDOW_SAMPLES,
        true,
    ));
    assert!(!should_fire_audio_lid_window(
        true,
        AUDIO_LID_WINDOW_ADVANCE_SAMPLES,
        AUDIO_LID_WINDOW_SAMPLES,
        AUDIO_LID_WINDOW_SAMPLES * 4,
        true,
    ));
}

#[test]
fn should_fire_audio_lid_window_returns_true_when_gate_active_and_window_has_recent_speech() {
    // Gate on, post-first-window, but the most recent window had
    // any speech (counter strictly less than the window length):
    // fire.
    assert!(should_fire_audio_lid_window(
        true,
        AUDIO_LID_WINDOW_ADVANCE_SAMPLES,
        AUDIO_LID_WINDOW_SAMPLES,
        AUDIO_LID_WINDOW_SAMPLES - 1,
        true,
    ));
    assert!(should_fire_audio_lid_window(
        true,
        AUDIO_LID_WINDOW_ADVANCE_SAMPLES,
        AUDIO_LID_WINDOW_SAMPLES,
        0,
        true,
    ));
}

#[test]
fn should_fire_audio_lid_window_returns_true_at_gate_boundary_minus_one() {
    // Boundary test proving the gate predicate is strict `<`, not
    // `<=`: a single speech sample within the candidate window
    // tips the predicate to fire.
    assert!(should_fire_audio_lid_window(
        true,
        AUDIO_LID_WINDOW_ADVANCE_SAMPLES,
        AUDIO_LID_WINDOW_SAMPLES,
        AUDIO_LID_WINDOW_SAMPLES - 1,
        true,
    ));
    assert!(!should_fire_audio_lid_window(
        true,
        AUDIO_LID_WINDOW_ADVANCE_SAMPLES,
        AUDIO_LID_WINDOW_SAMPLES,
        AUDIO_LID_WINDOW_SAMPLES,
        true,
    ));
}

// ---- feature 021: should_spawn_audio_hybrid -----------------

#[test]
fn should_spawn_audio_hybrid_skips_strong_english() {
    // Strong English first window (`p_en >= 0.90` after the round-2
    // dogfood tightening) → audio-LID is confident; the hybrid task
    // would only add cost without changing the route.
    assert!(!should_spawn_audio_hybrid(
        &LidLabel::English,
        0.95,
        CONFIDENCE_TO_SKIP_HYBRID_TASK
    ));
}

#[test]
fn should_spawn_audio_hybrid_fires_on_weak_english() {
    // Weak English (`p_en` below the skip threshold) is exactly
    // the J49-class failure shape. Spawn the hybrid task so the
    // secondary text-LID gets a chance to recover the Tagalog.
    assert!(should_spawn_audio_hybrid(
        &LidLabel::English,
        0.45,
        CONFIDENCE_TO_SKIP_HYBRID_TASK
    ));
}

#[test]
fn should_spawn_audio_hybrid_fires_on_moderate_english() {
    // 2026-05-18 round-2 dogfood: "The thing is, hindi pa fully
    // tested yung change." came in at `p_en=0.76` — real Taglish
    // that audio-LID over-confidently read as English. Threshold
    // 0.90 must let this band spawn the hybrid. Without this, the
    // press commits Deepgram and the Tagalog content is dropped.
    assert!(should_spawn_audio_hybrid(
        &LidLabel::English,
        0.76,
        CONFIDENCE_TO_SKIP_HYBRID_TASK
    ));
}

#[test]
fn should_spawn_audio_hybrid_skips_at_exact_threshold() {
    // Boundary semantics: a `p_en` exactly equal to the skip
    // threshold counts as "confident" and skips the hybrid task.
    // Inclusive on the skip side — pins the boundary so a future
    // threshold tweak doesn't silently flip it.
    assert!(!should_spawn_audio_hybrid(
        &LidLabel::English,
        CONFIDENCE_TO_SKIP_HYBRID_TASK,
        CONFIDENCE_TO_SKIP_HYBRID_TASK
    ));
}

#[test]
fn should_spawn_audio_hybrid_skips_tagalog() {
    // Tagalog first window already routes to Whisper; a secondary
    // `taglish` / `tagalog` agreement is a no-op (the override
    // direction is deepgram → whisper only, asymmetric per the
    // post-2026-05-18 architectural decision).
    assert!(!should_spawn_audio_hybrid(
        &LidLabel::Tagalog,
        0.10,
        CONFIDENCE_TO_SKIP_HYBRID_TASK
    ));
}

#[test]
fn should_spawn_audio_hybrid_skips_taglish() {
    // Same rationale as Tagalog — already routing to Whisper.
    assert!(!should_spawn_audio_hybrid(
        &LidLabel::Taglish,
        0.40,
        CONFIDENCE_TO_SKIP_HYBRID_TASK
    ));
}

#[test]
fn should_spawn_audio_hybrid_fires_on_other_label_with_low_p_en() {
    // The phonetically-adjacent failure shape (top1 = `ar`/`hi`/`pt`
    // etc. with weak `p_en`). audio-LID has no recovery path for
    // these; spawn the hybrid so the secondary text-LID can
    // re-evaluate the text.
    assert!(should_spawn_audio_hybrid(
        &LidLabel::Other("ar".into()),
        0.05,
        CONFIDENCE_TO_SKIP_HYBRID_TASK
    ));
}

#[test]
fn should_spawn_audio_hybrid_fires_on_other_label_even_with_high_p_en() {
    // High `p_en` on an `Other` label is still an uncertain
    // verdict — the routing layer can't commit on `Other` regardless
    // of `p_en`. Spawn the hybrid so the press isn't stuck in
    // "keep checking" forever.
    assert!(should_spawn_audio_hybrid(
        &LidLabel::Other("ar".into()),
        0.85,
        CONFIDENCE_TO_SKIP_HYBRID_TASK
    ));
}

#[test]
fn should_spawn_audio_hybrid_low_threshold_skips_modest_english() {
    // Edge case: a very low caller-supplied threshold makes even
    // modest English confidence enough to skip. Documents that
    // the function uses the *passed-in* threshold rather than a
    // hardcoded one.
    assert!(!should_spawn_audio_hybrid(&LidLabel::English, 0.40, 0.30));
}

// ---- feature 024 (backlog 0042): release-path trim helper -------------

/// Test-only `SessionDeps` factory parameterised by streaming-VAD
/// factory presence. Reuses the unreachable Deepgram pool so the
/// release-trim helper can be exercised without hitting the network.
fn minimal_deps_for_trim_test(
    streaming_vad_factory: Option<crate::vad::StreamingVadFactory>,
) -> SessionDeps {
    SessionDeps {
        deepgram_pool: unreachable_pool(),
        groq: None,
        prompt: None,
        injector: Arc::new(MockInjector::new()) as Arc<dyn PlatformInjector>,
        emitter: recording_emitter().0,
        state_notifier: recording_state_notifier().0,
        present_error: crate::error_presenter::noop_presenter(),
        show_repaste_notice: noop_repaste_notice(),
        history: None,
        mic_silenced: MicSilencedFlag::default(),
        whisper: None,
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: EnglishFastModeFlag::default(),
        bilingual_mode: BilingualModeFlag::default(),
        usage_tx: None,
        my_words: Arc::new(crate::my_words::MyWords::default()),
        about_me: crate::about_me::AboutMe::empty(),
        vocabulary: crate::vocabulary::Vocabulary::empty(),
        user_prompt: crate::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory,
    }
}

/// A factory returning [`crate::vad::PassThroughStreamingVad`] —
/// stable, deterministic, returns the input unchanged. Used by the
/// trim-helper tests so we can assert on the mirror-vs-fallback
/// branch decision without depending on Silero behaviour.
fn pass_through_streaming_factory() -> crate::vad::StreamingVadFactory {
    Arc::new(|| {
        Box::new(crate::vad::PassThroughStreamingVad) as Box<dyn crate::vad::StreamingVadDetector>
    })
}

#[tokio::test]
async fn resolve_trimmed_release_buffer_returns_original_when_factory_missing() {
    let deps = minimal_deps_for_trim_test(None);
    let original = vec![1_i16; 1000];
    let result = resolve_trimmed_release_buffer(&deps, None, &original).await;
    assert_eq!(
        result, original,
        "no factory → must return the original buffer unchanged"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn resolve_trimmed_release_buffer_returns_original_when_kill_switch_off() {
    // Factory present (one of the two switches on) but the trim
    // kill switch is OFF. Helper must not consume the factory.
    // `await_holding_lock` is allowed here: the lock is the
    // env-var serialization mechanism — releasing it before the
    // `.await` would defeat the test's purpose.
    let _guard = TRIM_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::remove_var(crate::MUNI_VAD_TRIM_RELEASE_BUFFER_ENV);
    let deps = minimal_deps_for_trim_test(Some(pass_through_streaming_factory()));
    let original = vec![2_i16; 1000];
    let result = resolve_trimmed_release_buffer(&deps, None, &original).await;
    assert_eq!(
        result, original,
        "trim kill switch off → must return the original buffer"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn resolve_trimmed_release_buffer_uses_mirror_when_populated() {
    let _guard = TRIM_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var(crate::MUNI_VAD_TRIM_RELEASE_BUFFER_ENV, "on");
    let deps = minimal_deps_for_trim_test(Some(pass_through_streaming_factory()));
    let mirror: Arc<TokioMutex<Vec<i16>>> = Arc::new(TokioMutex::new(vec![42_i16; 500]));
    let original = vec![1_i16; 1000];
    let result = resolve_trimmed_release_buffer(&deps, Some(&mirror), &original).await;
    std::env::remove_var(crate::MUNI_VAD_TRIM_RELEASE_BUFFER_ENV);
    assert_eq!(
        result,
        vec![42_i16; 500],
        "populated mirror → must take precedence over original"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn resolve_trimmed_release_buffer_falls_back_to_oneshot_when_mirror_empty() {
    let _guard = TRIM_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var(crate::MUNI_VAD_TRIM_RELEASE_BUFFER_ENV, "on");
    let deps = minimal_deps_for_trim_test(Some(pass_through_streaming_factory()));
    let mirror: Arc<TokioMutex<Vec<i16>>> = Arc::new(TokioMutex::new(Vec::new()));
    let original = vec![7_i16; 1000];
    let result = resolve_trimmed_release_buffer(&deps, Some(&mirror), &original).await;
    std::env::remove_var(crate::MUNI_VAD_TRIM_RELEASE_BUFFER_ENV);
    // PassThroughStreamingVad's `extract_speech` returns the
    // buffer unchanged — confirms the fallback path executed.
    assert_eq!(
        result, original,
        "empty mirror → one-shot fallback (PassThrough returns input)"
    );
}

/// Env-var tests share process state; serialize them like the
/// other env-var tests in this module so parallel workers can't
/// observe each other's writes.
static TRIM_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ---- feature 025 (backlog 0046): audio-LID silence gate ---------------

/// Drive the windowing loop's per-chunk gate composition manually
/// over a synthetic press, returning how many windows would have
/// fired a real `classify_audio_window` call. Captures the exact
/// chunk-feed → counter → predicate sequencing in
/// [`DictationSession::run_audio_lid_pass`] without standing up
/// `DeepgramClient` / broadcast channels / async-task wiring.
///
/// `gate_on` flips both the sibling-VAD construction AND the
/// `gate_active` predicate argument so the gate-off branch is
/// bit-identical to today's behavior.
async fn drive_audio_lid_gate_for_silence_press(
    gate_on: bool,
    chunk_samples: usize,
    num_chunks: usize,
) -> usize {
    let mut sibling_vad: Option<Box<dyn crate::vad::StreamingVadDetector>> = if gate_on {
        Some(Box::new(crate::vad::SilentStreamingVad))
    } else {
        None
    };
    let gate_active = sibling_vad.is_some();
    let mut samples_since_last_speech: usize = 0;
    let mut rolling: Vec<i16> = Vec::new();
    let mut accumulated_since_last_window: usize = 0;
    let mut first_window_done = false;
    let mut windows_fired: usize = 0;
    let chunk = vec![0_i16; chunk_samples];
    for _ in 0..num_chunks {
        // Sibling VAD feed — exact mirror of session.rs's
        // `lid_chunks_rx.recv()` arm in `run_audio_lid_pass`.
        if let Some(vad) = sibling_vad.as_mut() {
            let mut sink: Vec<i16> = Vec::with_capacity(chunk.len());
            vad.process_chunk(&chunk, &mut sink).await;
            if sink.is_empty() {
                samples_since_last_speech = samples_since_last_speech.saturating_add(chunk.len());
            } else {
                samples_since_last_speech = 0;
            }
        }
        rolling.extend_from_slice(&chunk);
        accumulated_since_last_window += chunk.len();
        if rolling.len() > AUDIO_LID_ROLLING_BUFFER_CAP_SAMPLES {
            let drop = rolling.len() - AUDIO_LID_ROLLING_BUFFER_CAP_SAMPLES;
            rolling.drain(..drop);
        }

        // Window-readiness check + gate skip — mirror of the loop
        // body, minus the `apply_audio_lid_verdict` dispatch.
        let window_ready = should_fire_audio_lid_window(
            first_window_done,
            accumulated_since_last_window,
            rolling.len(),
            samples_since_last_speech,
            gate_active,
        );
        let cadence_elapsed_with_full_buffer = first_window_done
            && accumulated_since_last_window >= AUDIO_LID_WINDOW_ADVANCE_SAMPLES
            && rolling.len() >= AUDIO_LID_WINDOW_SAMPLES;
        if !window_ready && cadence_elapsed_with_full_buffer && gate_active {
            // Skip branch: reset cadence so the next chunk doesn't
            // immediately cross the threshold again. Mirrors the
            // production code's `accumulated_since_last_window = 0`
            // on the gate-suppressed path.
            accumulated_since_last_window = 0;
        }
        if window_ready {
            accumulated_since_last_window = 0;
            first_window_done = true;
            windows_fired += 1;
        }
    }
    windows_fired
}

#[tokio::test]
async fn run_audio_lid_pass_with_gate_active_skips_after_first_window_when_all_silent() {
    // 12 s of pure silence at 16 kHz, fed as 3000-sample chunks
    // (~187 ms each → 64 chunks). With `SilentStreamingVad`, every
    // chunk emits zero bytes → `samples_since_last_speech` grows
    // monotonically. The first window MUST fire (route-commit
    // protection); every subsequent window MUST skip.
    let fired = drive_audio_lid_gate_for_silence_press(true, 3000, 64).await;
    assert_eq!(
            fired, 1,
            "with gate ON and SilentStreamingVad over 12 s of silence, only the first window should fire; got {fired}"
        );
}

#[tokio::test]
async fn run_audio_lid_pass_with_gate_off_classifies_repeatedly_on_silent_audio() {
    // Same 12 s silent press, gate OFF: behavior MUST be identical
    // to pre-feat/025 — one classify per ~1 s cadence after the
    // first 2 s buffer fill. Lower bound `>= 3` keeps the assert
    // resilient to small timing drift in the future without
    // losing the gate-off-vs-gate-on causality proof.
    let fired = drive_audio_lid_gate_for_silence_press(false, 3000, 64).await;
    assert!(
            fired >= 3,
            "with gate OFF, baseline behavior should fire multiple classifies on the same 12 s input; got {fired}"
        );
}

/// Env-var lock for the audio-LID gate kill switch's
/// `run_audio_lid_pass`-level wiring tests. Currently only the
/// resolver tests in `lib.rs::tests` mutate the env, but the lock
/// is declared here for future tests that exercise the spawn-site
/// resolution path.
#[allow(dead_code)]
static AUDIO_LID_GATE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ---- silence watchdog (toggle-session auto-commit on quiet) ----

/// Counts how many times the watchdog signaler fired. Each test
/// owns its own counter so they can run in parallel.
fn counting_signaler() -> (Arc<dyn Fn() + Send + Sync>, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let count_for_closure = count.clone();
    let signaler: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        count_for_closure.fetch_add(1, Ordering::Relaxed);
    });
    (signaler, count)
}

/// Continuous silence (RMS < threshold) for the configured window
/// fires the watchdog exactly once.
#[tokio::test]
async fn silence_watchdog_fires_after_threshold_of_continuous_silence() {
    let (amp_tx, amp_rx) = tokio::sync::watch::channel(0.0_f32);
    let (signaler, count) = counting_signaler();
    let handle = spawn_silence_watchdog(amp_rx, Duration::from_millis(200), signaler);

    // Publish quiet samples (well below threshold) for >200 ms.
    for _ in 0..20 {
        let _ = amp_tx.send(0.001);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Give the watchdog one more tick to observe its deadline.
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        count.load(Ordering::Relaxed),
        1,
        "watchdog must signal exactly once after continuous silence"
    );
    // Handle has already returned, but abort is idempotent.
    handle.abort();
}

/// Speech-grade amplitudes (RMS ≥ threshold) keep the watchdog
/// from firing. Models a user actively dictating for the full
/// session duration.
#[tokio::test]
async fn silence_watchdog_does_not_fire_while_speech_is_active() {
    let (amp_tx, amp_rx) = tokio::sync::watch::channel(0.0_f32);
    let (signaler, count) = counting_signaler();
    let handle = spawn_silence_watchdog(amp_rx, Duration::from_millis(150), signaler);

    // Publish loud samples (well above threshold) for longer than
    // the silence window. The watchdog must stay quiet.
    for _ in 0..15 {
        let _ = amp_tx.send(0.2);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        count.load(Ordering::Relaxed),
        0,
        "watchdog must not fire while RMS is above the speech threshold"
    );
    handle.abort();
}

/// A speech frame inside the silence window resets the timer, so
/// the watchdog only fires after the new silence stretch crosses
/// the threshold. Models a user pausing mid-thought.
#[tokio::test]
async fn silence_watchdog_resets_on_intervening_speech() {
    let (amp_tx, amp_rx) = tokio::sync::watch::channel(0.0_f32);
    let (signaler, count) = counting_signaler();
    let handle = spawn_silence_watchdog(amp_rx, Duration::from_millis(200), signaler);

    // ~100 ms of silence (under the threshold).
    for _ in 0..5 {
        let _ = amp_tx.send(0.001);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A burst of speech resets the timer.
    let _ = amp_tx.send(0.25);
    tokio::time::sleep(Duration::from_millis(20)).await;

    // No fire yet — the watchdog saw speech before the original
    // 200 ms window expired.
    assert_eq!(
        count.load(Ordering::Relaxed),
        0,
        "watchdog fired before reset"
    );

    // Another full silence window after the reset → should fire.
    for _ in 0..15 {
        let _ = amp_tx.send(0.001);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        count.load(Ordering::Relaxed),
        1,
        "watchdog must fire once the post-reset silence stretch exceeds threshold"
    );
    handle.abort();
}

// ---- Slice 11 (task 32): capture-start failure teardown ------------

/// Plan 039 task 32 (review follow-up) — a `ToggleLocked` press whose
/// capture-start fails has ALREADY armed the toggle and registered
/// Esc/Enter/NumpadEnter as consuming global shortcuts. The driver must
/// drive the manager's teardown immediately (via the silence signaler)
/// rather than leaving those keys swallowed system-wide until the 60 s
/// watchdog fires. This pins the exact decision the driver's failure arm
/// makes; the hotkey-side effect (signaler → unregister + `Commit`
/// release) is proven by `hotkey::tests::silence_timeout_during_toggle_
/// fires_commit_release`.
#[test]
fn capture_failure_tears_down_armed_toggle_but_not_ptt() {
    // ToggleLocked: an armed toggle must be torn down — signaler fires.
    let (signaler, count) = counting_signaler();
    tear_down_toggle_after_capture_failure(HotkeyMode::ToggleLocked, &signaler);
    assert_eq!(
        count.load(Ordering::Relaxed),
        1,
        "a failed ToggleLocked press must signal teardown exactly once so \
             Esc/Enter stop being swallowed system-wide"
    );

    // PTT: no locked state was armed; the real modifier-release is still
    // pending, so the signaler must NOT fire (a spurious signal would
    // synthesise a stray release with no matching press).
    let (ptt_signaler, ptt_count) = counting_signaler();
    tear_down_toggle_after_capture_failure(HotkeyMode::Ptt, &ptt_signaler);
    assert_eq!(
        ptt_count.load(Ordering::Relaxed),
        0,
        "a failed PTT press armed no toggle and must not signal teardown"
    );
}

// ---- Slice 4 (task 9): forwarder backpressure ----------------------

#[test]
fn is_send_timeout_classifies_by_reason_marker() {
    // Timeout-flavored reasons (stamped by `send_frame_timed`) classify as
    // socket-death; fast synchronous failures do not.
    assert!(is_send_timeout(&MuniError::DeepgramConnectionFailed {
        reason: "send timed out after 1s".into(),
    }));
    assert!(is_send_timeout(&MuniError::GladiaConnectionFailed {
        reason: "stop_recording send timed out after 1s".into(),
    }));
    assert!(!is_send_timeout(&MuniError::DeepgramConnectionFailed {
        reason: "stream closed".into(),
    }));
    assert!(!is_send_timeout(&MuniError::DeepgramConnectionFailed {
        reason: "Sending after closing is not allowed".into(),
    }));
    // A different variant is never a send timeout, whatever its text.
    assert!(!is_send_timeout(&MuniError::GroqConnectionFailed {
        reason: "timed out".into(),
    }));
}

#[derive(Clone, Copy)]
enum SendMode {
    Ok,
    Timeout,
    Fast,
}

/// A [`ReleaseSink`] that returns a fixed outcome for every chunk and
/// counts how many times the forwarder actually attempted a send. Lets the
/// forwarder's consecutive-failure caps be exercised deterministically
/// without a real wedged TCP socket.
struct CountingSink {
    mode: SendMode,
    calls: AtomicUsize,
}

impl CountingSink {
    fn new(mode: SendMode) -> Self {
        Self {
            mode,
            calls: AtomicUsize::new(0),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ReleaseSink for CountingSink {
    fn send_chunk(
        &self,
        _chunk: &[i16],
    ) -> impl std::future::Future<Output = Result<(), MuniError>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mode = self.mode;
        async move {
            match mode {
                SendMode::Ok => Ok(()),
                SendMode::Timeout => Err(MuniError::DeepgramConnectionFailed {
                    reason: "send timed out after 1s".into(),
                }),
                SendMode::Fast => Err(MuniError::DeepgramConnectionFailed {
                    reason: "stream closed".into(),
                }),
            }
        }
    }
}

/// Feed `n` distinct 10-sample chunks into a fresh broadcast channel, then
/// close it so the forwarder drains and exits. Returns (chunks, receiver).
fn scripted_chunks(n: usize) -> (Vec<Vec<i16>>, broadcast::Receiver<Vec<i16>>) {
    let (tx, rx) = broadcast::channel::<Vec<i16>>(n + 8);
    let chunks: Vec<Vec<i16>> = (0..n).map(|i| vec![i as i16; 10]).collect();
    for c in &chunks {
        tx.send(c.clone()).expect("broadcast send");
    }
    // Dropping the sender makes recv() return Closed once drained, so the
    // forwarder terminates without needing the release/drain path.
    drop(tx);
    (chunks, rx)
}

fn total_samples(chunks: &[Vec<i16>]) -> usize {
    chunks.iter().map(Vec::len).sum()
}

#[tokio::test]
async fn auto_forwarder_buffers_every_chunk_and_aborts_fast_on_timeouts() {
    // A wedged socket (every send times out) must abort after exactly
    // MAX_CONSECUTIVE_SEND_TIMEOUTS attempts — NOT ride the 30-cap — while
    // still buffering every chunk for the Whisper/Gladia rescue.
    let (chunks, rx) = scripted_chunks(50);
    let (_rel_tx, rel_rx) = oneshot::channel();
    let aborted = Arc::new(AtomicBool::new(false));
    let sink = Arc::new(CountingSink::new(SendMode::Timeout));

    let (buffer, _peak) =
        forward_and_buffer_until_release(sink.clone(), rx, rel_rx, aborted.clone()).await;

    assert_eq!(
        sink.calls(),
        MAX_CONSECUTIVE_SEND_TIMEOUTS,
        "forwarder must stop sending after the timeout cap, not the 30-cap"
    );
    assert!(
        aborted.load(Ordering::SeqCst),
        "the aborted flag must latch so the LID/rescue path knows the socket is dead"
    );
    assert_eq!(
        buffer.len(),
        total_samples(&chunks),
        "every captured chunk must survive locally even after sends abort"
    );
}

#[tokio::test]
async fn auto_forwarder_uses_30_cap_for_fast_errors() {
    // Instant (non-timeout) failures keep the larger 30-cap: a fast-failing
    // socket is only ~0.5 s of confirmed-dead at ~60 Hz.
    let (chunks, rx) = scripted_chunks(40);
    let (_rel_tx, rel_rx) = oneshot::channel();
    let aborted = Arc::new(AtomicBool::new(false));
    let sink = Arc::new(CountingSink::new(SendMode::Fast));

    let (buffer, _peak) =
        forward_and_buffer_until_release(sink.clone(), rx, rel_rx, aborted.clone()).await;

    assert_eq!(
        sink.calls(),
        30,
        "fast errors must ride the 30-cap, not the 2-timeout cap"
    );
    assert!(aborted.load(Ordering::SeqCst));
    assert_eq!(buffer.len(), total_samples(&chunks));
}

#[tokio::test]
async fn auto_forwarder_sends_every_chunk_on_healthy_socket() {
    let (chunks, rx) = scripted_chunks(20);
    let (_rel_tx, rel_rx) = oneshot::channel();
    let aborted = Arc::new(AtomicBool::new(false));
    let sink = Arc::new(CountingSink::new(SendMode::Ok));

    let (buffer, _peak) =
        forward_and_buffer_until_release(sink.clone(), rx, rel_rx, aborted.clone()).await;

    assert_eq!(
        sink.calls(),
        chunks.len(),
        "healthy socket sends every chunk"
    );
    assert!(!aborted.load(Ordering::SeqCst));
    assert_eq!(buffer.len(), total_samples(&chunks));
}

#[tokio::test]
async fn pool_forwarder_keeps_buffering_after_send_abort() {
    // The pool/keepalive forwarder must NOT return early on a dead socket
    // when buffering PCM (Parakeet backend) — it keeps draining so the
    // on-release transcribe still has the full clip.
    let (chunks, rx) = scripted_chunks(50);
    let (_rel_tx, rel_rx) = oneshot::channel();
    let sink = CountingSink::new(SendMode::Timeout);

    let (buffer, _peak) =
        forward_chunks_until_release(&sink, rx, rel_rx, /* buffer_pcm */ true).await;

    assert_eq!(
        sink.calls(),
        MAX_CONSECUTIVE_SEND_TIMEOUTS,
        "pool forwarder must also fast-abort on consecutive timeouts"
    );
    assert_eq!(
        buffer.len(),
        total_samples(&chunks),
        "buffer_pcm forwarder must keep draining after send abort, not return early"
    );
}

/// A [`ReleaseSink`] that mimics a genuinely wedged half-open socket: each
/// send BLOCKS for a full `deepgram::SEND_TIMEOUT` (1 s) before reporting a
/// timeout-flavored failure. Unlike [`CountingSink`] (which fails instantly)
/// this holds the forwarder's send arm open for the real ~1 s window, so a
/// test can prove the local rescue buffer keeps filling from `chunks_rx`
/// while a send is stuck.
struct WedgedSink {
    calls: Arc<AtomicUsize>,
    /// `start.elapsed()` (ms) captured when the abort-triggering (last)
    /// send returns — i.e. the moment the forwarder abandons the socket.
    abort_at_ms: Arc<std::sync::atomic::AtomicU64>,
    /// `tokio::time::Instant` (NOT `std::time::Instant`): the abort-window
    /// test runs under `#[tokio::test(start_paused = true)]`, which only
    /// virtualises Tokio's clock. A `std` instant reads wall-clock (~0 ms
    /// while the runtime auto-advances virtual time), making the
    /// `abort_ms <= 2_200` bound tautologically true. Measuring virtual
    /// time makes the assertion able to actually fail if the forwarder
    /// stops fast-aborting on consecutive timeouts.
    start: tokio::time::Instant,
}

impl ReleaseSink for WedgedSink {
    fn send_chunk(
        &self,
        _chunk: &[i16],
    ) -> impl std::future::Future<Output = Result<(), MuniError>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let abort_at = self.abort_at_ms.clone();
        let start = self.start;
        async move {
            // A wedged kernel buffer blocks the bounded write for the full
            // deepgram::SEND_TIMEOUT before the timeout fires.
            tokio::time::sleep(Duration::from_secs(1)).await;
            abort_at.store(start.elapsed().as_millis() as u64, Ordering::SeqCst);
            Err(MuniError::DeepgramConnectionFailed {
                reason: "send timed out after 1s".into(),
            })
        }
    }
}

#[tokio::test(start_paused = true)]
async fn slow_sink_never_drops_rescue_buffer_and_aborts_within_window() {
    // Acceptance criterion (task 9): a slow sink must never punch holes in
    // the local rescue buffer. A wedged socket blocks each send ~1 s; cpal
    // keeps delivering ~60 Hz throughout. During the ~2 s fast-abort window
    // ~120 chunks queue in the broadcast channel — which must stay under
    // CHUNK_BROADCAST_CAPACITY (192) so NONE are dropped to RecvError::Lagged
    // (a Lagged drop here permanently loses mid-press audio the rescue needs).
    const CHUNK_HZ: u64 = 60;
    const TOTAL_CHUNKS: usize = 180; // ~3 s of audio, spanning the wedge + drain
    let cap = crate::audio::CHUNK_BROADCAST_CAPACITY;
    let (tx, rx) = broadcast::channel::<Vec<i16>>(cap);
    let (_rel_tx, rel_rx) = oneshot::channel();
    let aborted = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let abort_at_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let sink = Arc::new(WedgedSink {
        calls: calls.clone(),
        abort_at_ms: abort_at_ms.clone(),
        start: tokio::time::Instant::now(),
    });

    // Producer delivers chunks at ~60 Hz. Under start_paused the runtime
    // auto-advances the clock between the producer's 16 ms sleeps and the
    // sink's 1 s blocks, so the wedge/queue race plays out deterministically.
    let producer = tokio::spawn(async move {
        for i in 0..TOTAL_CHUNKS {
            let _ = tx.send(vec![i as i16; 10]);
            tokio::time::sleep(Duration::from_millis(1000 / CHUNK_HZ)).await;
        }
        drop(tx);
    });

    let (buffer, _peak) = forward_and_buffer_until_release(sink, rx, rel_rx, aborted.clone()).await;
    producer.await.expect("producer task");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        MAX_CONSECUTIVE_SEND_TIMEOUTS,
        "a wedged sink must abandon the socket after the timeout cap, not keep retrying"
    );
    assert!(
        aborted.load(Ordering::SeqCst),
        "the aborted flag must latch so the rescue knows the socket is dead"
    );
    assert_eq!(
        buffer.len(),
        TOTAL_CHUNKS * 10,
        "rescue buffer must contain EVERY chunk — zero Lagged drops under a wedged sink"
    );
    let abort_ms = abort_at_ms.load(Ordering::SeqCst);
    assert!(
        abort_ms <= 2_200,
        "forwarder must abandon the wedged socket within ~2 s, took {abort_ms} ms"
    );
}

// ---- Slice 4 (task 12): parked-socket probe budget -----------------

#[tokio::test]
async fn probe_within_true_only_on_ok() {
    assert!(probe_within(Duration::from_secs(1), async { Ok(()) }).await);
    assert!(
        !probe_within(Duration::from_secs(1), async {
            Err(MuniError::DeepgramConnectionFailed {
                reason: "closed".into(),
            })
        })
        .await,
        "a probe error means discard the parked slot"
    );
}

#[tokio::test]
async fn probe_within_gives_up_at_budget() {
    // A wedged probe that would take far longer than the budget must be
    // abandoned at the budget, not ridden to the inner future's completion.
    let budget = Duration::from_millis(50);
    let start = Instant::now();
    let alive = probe_within(budget, async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        Ok(())
    })
    .await;
    assert!(!alive, "a probe that exceeds the budget reads as dead");
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "probe must give up at ~budget, not wait for the slow inner future"
    );
}

#[tokio::test(start_paused = true)]
async fn take_fire_and_forgets_slow_close_of_discarded_parked_socket() {
    // Task 12 acceptance: a half-open parked socket that fails the liveness
    // probe must NOT stall press start on close(). A wedged socket's close()
    // blocks up to deepgram::CLOSE_TIMEOUT (500 ms) draining the close frame
    // into a full kernel buffer; take() must detach that teardown and fall
    // straight through to the inline open. Modeled with a disconnected
    // client (probe fails fast) whose close() is delayed 500 ms via the
    // close_grace seam.
    let mut wedged = DeepgramClient::disconnected_for_test(Duration::from_millis(500));
    // (no-op on a disconnected client, but documents intent if the seam grows)
    wedged.set_close_grace(Duration::from_millis(500));
    let wedged = Arc::new(wedged);

    // Empty key → the inline open fails INSTANTLY (DeepgramMissingApiKey is
    // checked before any network/handshake timer), so take()'s total time
    // is dominated only by whether it awaited the discard's close().
    let pool = DeepgramPool::spawn_with_endpoint(fixed_deepgram_key(""), "ws://127.0.0.1:1".into());
    *pool.parked.lock().await = Some(ParkedEntry {
        client: wedged,
        keepalive_cancel: Arc::new(Notify::new()),
    });

    // Measure the PAUSED (virtual) clock — `start_paused` only virtualizes
    // `tokio::time`, not `std::time::Instant`. If take() awaits the 500 ms
    // close() the runtime auto-advances the virtual clock to 500 ms; if it
    // fire-and-forgets, no timer is awaited and the clock stays put.
    let start = tokio::time::Instant::now();
    let res = pool.take().await;
    let elapsed = start.elapsed();

    assert!(
        res.is_err(),
        "inline open to a dead endpoint must fail — we're on the fallthrough path"
    );
    assert!(
        elapsed < Duration::from_millis(200),
        "take() must fire-and-forget the 500 ms close(), not await it — took {elapsed:?}"
    );
}

// ---- Slice 4 (tasks 10 + 11): rescue + provenance ------------------

#[tokio::test]
async fn whisper_rescue_serves_transcript_and_tags_whisper_fallback() {
    // The shared Groq Whisper → Gladia rescue chain used by BOTH the
    // audio-LID Whisper route and the Deepgram-route rescue (task 10):
    // when Groq Whisper returns text, the press is served and tagged
    // `whisper-fallback` provenance (task 11), never `gladia-primary`.
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"text":"rescued hello"}"#))
        .mount(&server)
        .await;
    let client = GroqWhisperClient::with_endpoint(server.uri()).expect("client builds");

    let injector = Arc::new(MockInjector::new());
    let (session, events, _states, _injector) = session_with(None, None, injector);

    let samples = vec![1i16; 32];
    let out = session
        .transcribe_via_whisper_or_rescue(&client, &samples, "gsk-test-****ABCD", 123)
        .await;

    assert_eq!(
        out,
        Some(("rescued hello".to_string(), 123, SERVED_BY_WHISPER_FALLBACK)),
        "Groq Whisper rescue must serve the transcript and tag whisper-fallback provenance"
    );
    let recorded = events.lock().expect("poisoned").clone();
    assert!(
        recorded
            .iter()
            .any(|(e, p)| e == EVENT_TRANSCRIPT_RAW && p == "rescued hello"),
        "rescue must emit the raw-transcript overlay, got {recorded:?}"
    );
    assert!(
        !recorded.iter().any(|(e, _)| e == EVENT_TRANSCRIPT_ERROR),
        "a successful rescue must not emit an error event, got {recorded:?}"
    );
}

#[tokio::test]
async fn deepgram_route_rescue_short_press_resolves_silent_idle_without_rescue() {
    // Task 10 + finding: a sub-50 ms hotkey graze whose Deepgram socket
    // also died must resolve to silent idle — exactly as the primary path
    // would — NOT run Groq (guaranteed HTTP 400 on a <0.01 s buffer) then
    // Gladia then surface a terminal "transcription unavailable" toast. The
    // shared silent/short-press gate short-circuits before any rescue and
    // before the amber Recovering pill.
    let injector = Arc::new(MockInjector::new());
    let (session, events, states, _injector) = session_with(None, None, injector);

    // < 160 samples = below Groq Whisper's 0.01 s (160-sample) floor.
    let samples = vec![0i16; 80];
    let original = MuniError::DeepgramConnectionFailed {
        reason: "socket died with zero finals".into(),
    };
    let out = session
        .rescue_deepgram_route(&samples, 0, Duration::from_millis(30), &original)
        .await;

    assert_eq!(
        out, None,
        "an accidental too-short press must resolve to silent idle, not a rescue tuple"
    );
    let trail = states.lock().expect("states poisoned").clone();
    assert!(
        !trail.contains(&SessionState::Recovering),
        "silent idle must NOT flash the Recovering pill, got {trail:?}"
    );
    assert_eq!(
        trail.last(),
        Some(&SessionState::Idle),
        "silent idle must land on Idle, got {trail:?}"
    );
    let recorded = events.lock().expect("poisoned").clone();
    assert!(
        recorded
            .iter()
            .any(|(e, p)| e == EVENT_TRANSCRIPT_FINAL && p.is_empty()),
        "silent idle emits an empty final, got {recorded:?}"
    );
    assert!(
        !recorded.iter().any(|(e, _)| e == EVENT_TRANSCRIPT_ERROR),
        "an accidental press must not surface a terminal error, got {recorded:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn deepgram_route_rescue_serves_via_gladia_when_whisper_unavailable() {
    // Task 10 + 11: Deepgram died post-open with zero finals and Groq
    // Whisper is unavailable (no client) — the Deepgram-route rescue must
    // go straight to the Gladia fallback, flash the amber Recovering pill
    // (learned/026), serve the transcript, and tag `gladia-rescue`
    // provenance. Exercises `rescue_deepgram_route`'s whisper-unavailable →
    // direct-Gladia branch end to end against the mock Gladia server.
    let _g = crate::secrets::env_var_test_lock().lock().await;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let (ws_url, _frames) = spawn_chunk_counting_mock_ws().await;
    let post_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/live"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "test-dg-rescue-session",
            "url": ws_url,
        })))
        .mount(&post_server)
        .await;
    std::env::set_var(
        crate::gladia::POST_ENDPOINT_OVERRIDE_ENV,
        format!("{}/v2/live", post_server.uri()),
    );
    std::env::set_var(crate::secrets::GLADIA_ENV_VAR, "test-gladia-key");
    // `rescue_deepgram_route` builds a `(whisper, secrets::get(GROQ_ACCOUNT))`
    // tuple, and Rust evaluates BOTH elements even though `whisper` is None
    // here — so `secrets::get(GROQ_ACCOUNT)` runs. Without an env override it
    // falls through to a live OS-keychain read (`read_keychain`), which can
    // block on an access-control prompt on a machine that already holds a
    // dev Groq entry. Seed the env override so the read short-circuits and
    // the test stays fully hermetic. The value is irrelevant to the assertion
    // (whisper is unavailable regardless, so the rescue still routes to
    // Gladia); it only keeps the keychain untouched.
    std::env::set_var(crate::secrets::GROQ_ENV_VAR, "gsk-test-****ABCD");

    let injector = Arc::new(MockInjector::new());
    // `session_with` leaves `whisper: None` — the whisper-unavailable branch.
    let (session, events, states, _injector) = session_with(None, None, injector);

    // Non-silent (peak 8000 ≫ SILENCED_PEAK_THRESHOLD), speech-length,
    // 3 s press so the silent/short-press gate passes to the real rescue.
    let samples = vec![8000i16; 50_000];
    let original = MuniError::DeepgramConnectionFailed {
        reason: "zero finals, socket died".into(),
    };
    let out = session
        .rescue_deepgram_route(&samples, 8000, Duration::from_secs(3), &original)
        .await;

    std::env::remove_var(crate::gladia::POST_ENDPOINT_OVERRIDE_ENV);
    std::env::remove_var(crate::secrets::GLADIA_ENV_VAR);
    std::env::remove_var(crate::secrets::GROQ_ENV_VAR);

    assert_eq!(
        out,
        Some((
            "fallback transcript".to_string(),
            8000,
            SERVED_BY_GLADIA_RESCUE
        )),
        "whisper-unavailable Deepgram-route rescue must serve via Gladia and tag gladia-rescue"
    );
    let trail = states.lock().expect("states poisoned").clone();
    assert!(
        trail.contains(&SessionState::Recovering),
        "the Deepgram-route rescue must flash the amber Recovering pill, got {trail:?}"
    );
    let recorded = events.lock().expect("poisoned").clone();
    assert!(
        !recorded.iter().any(|(e, _)| e == EVENT_TRANSCRIPT_ERROR),
        "a successful Gladia rescue must not surface an error event, got {recorded:?}"
    );
}

/// Build an [`AutoDetectActive`] wired for a release-path test: a
/// `disconnected_for_test` Deepgram client (models "socket died with zero
/// finals" deterministically — its `finalize()` returns
/// `Err(DeepgramConnectionFailed)` with no accumulated finals, exactly the
/// Failed/empty case task 10's rescue arm handles, and without a flaky
/// mock-WS network peer), a forwarder yielding the buffered PCM + peak, and
/// a decision cell pre-set to Deepgram so `finalize_auto_detect` routes into
/// the Deepgram finalize → Err → rescue arm. All LID/hybrid/trigger
/// machinery is inert (no handles, zero drift, floor unreachable) so the
/// release path runs straight through to the finalize.
fn deepgram_active_dead_socket(samples: Vec<i16>, peak: i16) -> AutoDetectActive {
    AutoDetectActive {
        deepgram_client: Arc::new(DeepgramClient::disconnected_for_test(Duration::ZERO)),
        forwarder: tauri::async_runtime::spawn(async move { (samples, peak) }),
        decision: Arc::new(TokioMutex::new(Some(RouterDecision::Deepgram))),
        decision_notify: Arc::new(Notify::new()),
        release_tx: watch::channel(false).0,
        lid_handle: tauri::async_runtime::spawn(async {}),
        committed: Arc::new(AtomicBool::new(false)),
        gemini_handle: Arc::new(TokioMutex::new(None)),
        confidence_trigger_handle: Arc::new(TokioMutex::new(None)),
        trigger_inflight: Arc::new(AtomicBool::new(false)),
        audio_hybrid_inflight: Arc::new(AtomicUsize::new(0)),
        audio_hybrid_handle: Arc::new(TokioMutex::new(None)),
        audio_hybrid_speech_mirror: Arc::new(TokioMutex::new(Vec::new())),
        audio_lid_drift_counter: Arc::new(AtomicUsize::new(0)),
        audio_lid_release_drift_fire_floor: 3,
        audio_lid_last_post_commit_was_other: Arc::new(AtomicBool::new(false)),
        audio_lid_release_other_as_taglish: false,
        audio_hybrid_recent_text_lid_english: Arc::new(AtomicBool::new(false)),
        audio_lid_hybrid_veto_drift: false,
        released_tx: None,
        pressed_at: Instant::now(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn deepgram_err_arm_rescues_via_whisper_and_persists_provenance() {
    // Task 10 headline acceptance criterion + task 11 provenance, driven
    // through `finalize_auto_detect` ITSELF (not the rescue helper in
    // isolation): a committed-Deepgram press whose stream died with zero
    // finals (finalize -> Err) must replay the buffered PCM through Groq
    // Whisper, paste the rescued transcript, tag `whisper-fallback`, AND
    // persist a history row carrying that provenance. This pins the
    // arm->rescue wiring — that the correct buffered `samples`/`peak` reach
    // the rescue, close-before-rescue ordering, and the whisper-available
    // leg through `rescue_deepgram_route` — end to end.
    //
    // Holds the shared env lock: sets `MUNI_GROQ_KEY` (read by
    // `secrets::get(GROQ_ACCOUNT)` inside the rescue), which is
    // process-global like the other provider keys.
    let _g = crate::secrets::env_var_test_lock().lock().await;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Groq Whisper mock returns text so the rescue's Whisper leg serves.
    let groq = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"text":"rescued through whisper"}"#),
        )
        .mount(&groq)
        .await;
    let whisper = Arc::new(GroqWhisperClient::with_endpoint(groq.uri()).expect("whisper client"));
    std::env::set_var(crate::secrets::GROQ_ENV_VAR, "gsk-test-****ABCD");

    // Real history store so we can assert the persisted row's provenance.
    let dir = tempfile::tempdir().expect("tempdir");
    let history =
        Arc::new(HistoryStore::open(HistoryStore::default_path(dir.path())).expect("open history"));

    let injector = Arc::new(MockInjector::new());
    let (emitter, events) = recording_emitter();
    let (state_notifier, states) = recording_state_notifier();
    let deps = SessionDeps {
        deepgram_pool: unreachable_pool(),
        groq: None,
        prompt: None,
        injector: injector.clone() as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier,
        present_error: crate::error_presenter::noop_presenter(),
        show_repaste_notice: noop_repaste_notice(),
        history: Some(Arc::clone(&history)),
        mic_silenced: MicSilencedFlag::default(),
        whisper: Some(whisper),
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: EnglishFastModeFlag::default(),
        bilingual_mode: BilingualModeFlag::default(),
        usage_tx: None,
        my_words: std::sync::Arc::new(crate::my_words::MyWords::default()),
        about_me: crate::about_me::AboutMe::empty(),
        vocabulary: crate::vocabulary::Vocabulary::empty(),
        user_prompt: crate::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    // Non-silent (peak 8000 ≫ threshold), speech-length 3 s press so the
    // silent/short-press gate passes and the real rescue runs.
    let samples = vec![8000i16; 50_000];
    let active = deepgram_active_dead_socket(samples, 8000);

    let out = session
        .finalize_auto_detect(active, Duration::from_secs(3))
        .await;

    std::env::remove_var(crate::secrets::GROQ_ENV_VAR);

    // (task 10) the Deepgram Err arm routed into the rescue and the Whisper
    // leg served the buffered PCM — carrying the forwarder's peak (8000) —
    // tagged whisper-fallback.
    assert_eq!(
        out,
        Some((
            "rescued through whisper".to_string(),
            8000,
            SERVED_BY_WHISPER_FALLBACK
        )),
        "Deepgram-Err arm must rescue via Groq Whisper and tag whisper-fallback"
    );
    // The amber Recovering pill flashed (learned/026 cross-provider rescue).
    let trail = states.lock().expect("states poisoned").clone();
    assert!(
        trail.contains(&SessionState::Recovering),
        "the Deepgram-route rescue must flash the amber Recovering pill, got {trail:?}"
    );

    // (task 11) deliver the rescued tuple and assert the PERSISTED history
    // row carries the rescue provenance — the tuple->row wiring end to end,
    // not just the returned tag.
    let (text, _peak, served_by) = out.expect("rescued tuple");
    session
        .deliver_final(
            &text,
            &text,
            served_by,
            CompletionMetrics::test_default(),
            false,
            DeliveryContext::immediate(),
        )
        .await;
    assert_eq!(
        injector.captured(),
        vec!["rescued through whisper".to_string()]
    );

    let latest = tokio::task::spawn_blocking(move || {
        for _ in 0..50 {
            if let Ok(Some(rec)) = history.latest() {
                return Some(rec);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    })
    .await
    .expect("join");
    let latest = latest.expect("a history row must be persisted for the rescued press");
    assert_eq!(
        latest.served_by, SERVED_BY_WHISPER_FALLBACK,
        "the persisted history row must carry the rescue provenance"
    );

    let recorded = events.lock().expect("poisoned").clone();
    assert!(
        !recorded.iter().any(|(e, _)| e == EVENT_TRANSCRIPT_ERROR),
        "a successful Whisper rescue must not surface an error event, got {recorded:?}"
    );
}

/// Build a [`WhisperBatchActive`] wired for a release-path test: a
/// forwarder yielding the buffered PCM + peak and a synthetic pool-open
/// error, exactly as [`DictationSession::spawn_whisper_batch_press`] would
/// install it when the Deepgram pool is down at press start.
fn whisper_batch_active(samples: Vec<i16>, peak: i16) -> WhisperBatchActive {
    WhisperBatchActive {
        forwarder: tauri::async_runtime::spawn(async move { (samples, peak) }),
        released_tx: None,
        pressed_at: Instant::now(),
        take_err: MuniError::DeepgramConnectionFailed {
            reason: "pool unreachable (test)".into(),
        },
    }
}

#[tokio::test]
async fn pool_outage_at_press_start_installs_whisper_batch_session() {
    // Plan 039 task 26 — headline entry path. When `DeepgramPool::take`
    // fails at press start on the AutoDetect route, the press must NOT
    // abort with a terminal error; it installs a buffer-only
    // `WhisperBatch` session so the release can still dictate via the
    // Whisper batch route. Pins: (a) the fallback fires instead of
    // `emit_error`, (b) the Listening pill still shows and no `Error`
    // state is stamped (learned/026 — no terminal error on a rescued
    // press), (c) the installed session is the buffer-only variant.

    // Unreachable pool: `take()` returns Err synchronously (empty key /
    // dead endpoint), deterministically modelling a full Deepgram outage.
    let pool = unreachable_pool();
    // A Whisper client must be present so routing picks AutoDetect (and so
    // the buffer-only fallback is viable); its endpoint is never hit in
    // this test because we assert at press start, before release.
    let whisper = Arc::new(
        GroqWhisperClient::with_endpoint("http://127.0.0.1:1".to_string()).expect("whisper client"),
    );
    let (emitter, _events) = recording_emitter();
    let (state_notifier, states) = recording_state_notifier();
    let injector = Arc::new(MockInjector::new());
    let deps = SessionDeps {
        deepgram_pool: pool,
        groq: None,
        prompt: None,
        injector: injector as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier,
        present_error: crate::error_presenter::noop_presenter(),
        show_repaste_notice: noop_repaste_notice(),
        history: None,
        mic_silenced: MicSilencedFlag::default(),
        whisper: Some(whisper),
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: EnglishFastModeFlag::default(),
        bilingual_mode: BilingualModeFlag::default(),
        usage_tx: None,
        my_words: std::sync::Arc::new(crate::my_words::MyWords::default()),
        about_me: crate::about_me::AboutMe::empty(),
        vocabulary: crate::vocabulary::Vocabulary::empty(),
        user_prompt: crate::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    let (_chunk_tx, chunk_rx) = broadcast::channel::<Vec<i16>>(8);
    session
        .handle_hotkey_pressed(chunk_rx, HotkeyMode::Ptt)
        .await;

    // The buffer-only fallback installed a `WhisperBatch` session rather
    // than aborting the press.
    {
        let g = session.active.lock().await;
        assert!(
            matches!(*g, Some(ActiveSession::WhisperBatch(_))),
            "pool outage must install a WhisperBatch session, got {:?}",
            g.as_ref().map(|s| match s {
                ActiveSession::Deepgram(_) => "Deepgram",
                ActiveSession::AutoDetect(_) => "AutoDetect",
                ActiveSession::WhisperBatch(_) => "WhisperBatch",
            })
        );
    }

    let trail = states.lock().expect("poisoned").clone();
    assert_eq!(
        trail.first(),
        Some(&SessionState::Listening),
        "Listening MUST fire before the pool-outage fallback; got {trail:?}",
    );
    assert!(
        !trail.contains(&SessionState::Error),
        "a pool outage that falls back to Whisper batch must NOT stamp Error; got {trail:?}",
    );

    // Clean up the buffer-only forwarder task.
    session.handle_hotkey_cancelled().await;
}

#[tokio::test(flavor = "current_thread")]
async fn whisper_batch_finalize_rescues_via_whisper_with_recovering_pill() {
    // Plan 039 task 26 — release path. A buffer-only press (Deepgram pool
    // was down at start) replays its locally-buffered PCM through the Groq
    // Whisper → Gladia batch chain, pastes the transcript tagged
    // `whisper-fallback`, flashes the amber `Recovering` pill (learned/026,
    // NOT a terminal error), and persists that provenance — proving the
    // acceptance criterion "Deepgram completely down ⇒ pressing still
    // dictates via Whisper batch route" end to end.
    let _g = crate::secrets::env_var_test_lock().lock().await;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let groq = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"text":"buffered via whisper"}"#),
        )
        .mount(&groq)
        .await;
    let whisper = Arc::new(GroqWhisperClient::with_endpoint(groq.uri()).expect("whisper client"));
    std::env::set_var(crate::secrets::GROQ_ENV_VAR, "gsk-test-****ABCD");

    let dir = tempfile::tempdir().expect("tempdir");
    let history =
        Arc::new(HistoryStore::open(HistoryStore::default_path(dir.path())).expect("open history"));

    let injector = Arc::new(MockInjector::new());
    let (emitter, events) = recording_emitter();
    let (state_notifier, states) = recording_state_notifier();
    let deps = SessionDeps {
        deepgram_pool: unreachable_pool(),
        groq: None,
        prompt: None,
        injector: injector.clone() as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier,
        present_error: crate::error_presenter::noop_presenter(),
        show_repaste_notice: noop_repaste_notice(),
        history: Some(Arc::clone(&history)),
        mic_silenced: MicSilencedFlag::default(),
        whisper: Some(whisper),
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: EnglishFastModeFlag::default(),
        bilingual_mode: BilingualModeFlag::default(),
        usage_tx: None,
        my_words: std::sync::Arc::new(crate::my_words::MyWords::default()),
        about_me: crate::about_me::AboutMe::empty(),
        vocabulary: crate::vocabulary::Vocabulary::empty(),
        user_prompt: crate::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    // Non-silent (peak 8000 ≫ threshold), speech-length 3 s press so the
    // silent/short-press gate passes and the real batch transcribe runs.
    let samples = vec![8000i16; 50_000];
    let active = whisper_batch_active(samples, 8000);

    let out = session
        .finalize_whisper_batch(active, Duration::from_secs(3))
        .await;

    std::env::remove_var(crate::secrets::GROQ_ENV_VAR);

    assert_eq!(
        out,
        Some((
            "buffered via whisper".to_string(),
            8000,
            SERVED_BY_WHISPER_FALLBACK
        )),
        "buffer-only press must batch-transcribe via Groq Whisper and tag whisper-fallback"
    );
    let trail = states.lock().expect("states poisoned").clone();
    assert!(
        trail.contains(&SessionState::Recovering),
        "the pool-outage buffer-only route must flash the amber Recovering pill, got {trail:?}"
    );

    let (text, _peak, served_by) = out.expect("rescued tuple");
    session
        .deliver_final(
            &text,
            &text,
            served_by,
            CompletionMetrics::test_default(),
            false,
            DeliveryContext::immediate(),
        )
        .await;
    assert_eq!(
        injector.captured(),
        vec!["buffered via whisper".to_string()]
    );

    let latest = tokio::task::spawn_blocking(move || {
        for _ in 0..50 {
            if let Ok(Some(rec)) = history.latest() {
                return Some(rec);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    })
    .await
    .expect("join");
    let latest = latest.expect("a history row must be persisted for the buffered press");
    assert_eq!(
        latest.served_by, SERVED_BY_WHISPER_FALLBACK,
        "the persisted history row must carry the whisper-fallback provenance"
    );

    let recorded = events.lock().expect("poisoned").clone();
    assert!(
        !recorded.iter().any(|(e, _)| e == EVENT_TRANSCRIPT_ERROR),
        "a successful buffer-only Whisper serve must not surface an error event, got {recorded:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn audible_press_clears_stale_mic_silenced_latch() {
    // Regression: `MicSilencedFlag` was a set-once latch cleared only by a
    // process restart. A single benign silent hold earlier in a session
    // (user held the hotkey without speaking, or spoke below the peak
    // threshold) pinned the Permissions card to the amber "Stale" pill for
    // the rest of the session — even while dictation plainly kept working.
    // A press that delivers real, audible content is positive proof the
    // AVFoundation cache is NOT lying, so it must self-heal the latch.
    // This drives `handle_hotkey_released` through the full finalize +
    // content-gate path with a pre-set latch and asserts it clears.
    let _g = crate::secrets::env_var_test_lock().lock().await;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let groq = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"text":"real words"}"#))
        .mount(&groq)
        .await;
    let whisper = Arc::new(GroqWhisperClient::with_endpoint(groq.uri()).expect("whisper client"));
    std::env::set_var(crate::secrets::GROQ_ENV_VAR, "gsk-test-****ABCD");

    // Clone shares the inner Arc<AtomicBool>, so this handle observes what
    // the session mutates. Pre-set the latch to model a prior silent press.
    let mic_silenced = MicSilencedFlag::default();
    mic_silenced.mark_silenced();
    assert!(mic_silenced.is_silenced(), "precondition: latch starts set");

    let injector = Arc::new(MockInjector::new());
    let (emitter, _events) = recording_emitter();
    let (state_notifier, _states) = recording_state_notifier();
    let deps = SessionDeps {
        deepgram_pool: unreachable_pool(),
        groq: None,
        prompt: None,
        injector: injector.clone() as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier,
        present_error: crate::error_presenter::noop_presenter(),
        show_repaste_notice: noop_repaste_notice(),
        history: None,
        mic_silenced: mic_silenced.clone(),
        whisper: Some(whisper),
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: EnglishFastModeFlag::default(),
        bilingual_mode: BilingualModeFlag::default(),
        usage_tx: None,
        my_words: std::sync::Arc::new(crate::my_words::MyWords::default()),
        about_me: crate::about_me::AboutMe::empty(),
        vocabulary: crate::vocabulary::Vocabulary::empty(),
        user_prompt: crate::user_prompt::UserPrompt::empty(),
        vad_detector: None,
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    // Non-silent press (peak 8000 ≫ threshold, 3 s) so the silent/short-press
    // gates pass and the batch transcribe produces real content.
    let samples = vec![8000i16; 50_000];
    let active = whisper_batch_active(samples, 8000);
    {
        let mut g = session.active.lock().await;
        *g = Some(ActiveSession::WhisperBatch(active));
    }

    // clear_silenced() fires synchronously in the content-gate path before
    // delivery is spawned, so the latch is already cleared by the time this
    // returns; awaiting the delivery handle just keeps the test tidy.
    let handle = session.handle_hotkey_released(false).await;

    std::env::remove_var(crate::secrets::GROQ_ENV_VAR);

    assert!(
        !mic_silenced.is_silenced(),
        "an audible press must clear the stale mic-silenced latch so the \
         Permissions pill self-heals without a restart",
    );

    if let Some(handle) = handle {
        let _ = handle.await;
    }
}

/// Fake VAD that always reports "no speech" — models a press where the
/// mic captured audible sound (peak above the amplitude gate) but no
/// recognizable speech (a hotkey tap while thinking, breathing, typing).
struct NoSpeechVad;

#[async_trait::async_trait]
impl crate::vad::VadDetector for NoSpeechVad {
    async fn predict_speech(&self, _samples: &[i16]) -> bool {
        false
    }
    fn provider_label(&self) -> &str {
        "test_no_speech"
    }
}

#[tokio::test(flavor = "current_thread")]
async fn vad_no_speech_on_audible_press_does_not_mark_mic_stale() {
    // Regression for the dominant false positive: the VAD "no speech"
    // gate used to mark the mic "Stale". But VAD finding no *speech* in
    // an AUDIBLE press (peak well above the amplitude gate — the stream
    // plainly carried sound) is proof the mic is alive, not that the AV
    // cache is lying. A user tapping the hotkey without speaking hit this
    // constantly and pinned the pill to Stale for the whole session.
    let mic_silenced = MicSilencedFlag::default();
    assert!(
        !mic_silenced.is_silenced(),
        "precondition: latch starts clear"
    );

    let injector = Arc::new(MockInjector::new());
    let (emitter, _events) = recording_emitter();
    let (state_notifier, _states) = recording_state_notifier();
    let deps = SessionDeps {
        deepgram_pool: unreachable_pool(),
        groq: None,
        prompt: None,
        injector: injector as Arc<dyn PlatformInjector>,
        emitter,
        state_notifier,
        present_error: crate::error_presenter::noop_presenter(),
        show_repaste_notice: noop_repaste_notice(),
        history: None,
        mic_silenced: mic_silenced.clone(),
        whisper: None,
        parakeet: None,
        text_lid: None,
        text_lid_secondary: None,
        audio_lid: None,
        english_fast_mode: EnglishFastModeFlag::default(),
        bilingual_mode: BilingualModeFlag::default(),
        usage_tx: None,
        my_words: std::sync::Arc::new(crate::my_words::MyWords::default()),
        about_me: crate::about_me::AboutMe::empty(),
        vocabulary: crate::vocabulary::Vocabulary::empty(),
        user_prompt: crate::user_prompt::UserPrompt::empty(),
        vad_detector: Some(Arc::new(NoSpeechVad) as Arc<dyn crate::vad::VadDetector>),
        streaming_vad_factory: None,
    };
    let session = DictationSession::new(deps);

    // Audible press: 1 s of samples at peak 8000 (≫ both the whisper-skip
    // threshold and the dead-stream ceiling), long enough for Groq's
    // floor. Gate 1 (amplitude) passes → the content-aware VAD gate fires
    // and resolves the press to idle.
    let samples = vec![8000i16; 16_000];
    let fired = session
        .resolve_silent_press_idle(&samples, 8000, Duration::from_secs(1))
        .await;

    assert!(
        fired,
        "the VAD no-speech gate must fire and resolve the audible-but-speechless press to idle",
    );
    assert!(
        !mic_silenced.is_silenced(),
        "a VAD no-speech verdict on an AUDIBLE press must NOT mark the mic Stale — the stream \
         carried sound, so the mic is provably alive",
    );
}
