//! Integration test for the `injection::PlatformInjector` trait contract.
//!
//! The macOS implementation is exercised manually (it requires a real
//! Accessibility grant + a focused text field, neither of which a CI test
//! can provide). What we _can_ verify automatically is that the trait
//! contract behaves the way the session orchestrator expects: callers can
//! pass an arbitrary `Arc<dyn PlatformInjector>`, the future is `Send +
//! 'static`, and a passing paste is observable via captured text.
//!
//! This is the same harness Phase 11's `end_to_end.rs` will reuse to drive
//! the full press → release path against mocked Deepgram/Groq + a captured
//! injector.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use muni_lib::error::MuniError;
use muni_lib::injection::{PlatformInjector, SerializedInjector};

/// In-process injector that records every paste call.
///
/// The recorded calls let tests assert end-to-end that whatever cleaned text
/// the orchestrator produced was the text that reached the injector — so we
/// can lock down the cleanup → paste handoff without a real pasteboard.
struct MockInjector {
    pasted: Mutex<Vec<String>>,
}

impl MockInjector {
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
impl PlatformInjector for MockInjector {
    async fn paste(&self, text: &str) -> Result<(), MuniError> {
        if text.is_empty() {
            return Err(MuniError::NothingToPaste);
        }
        self.pasted.lock().expect("poisoned").push(text.to_string());
        Ok(())
    }
}

#[tokio::test]
async fn paste_captures_non_empty_text_through_dyn_injector() {
    let typed = Arc::new(MockInjector::new());
    let dyn_injector: Arc<dyn PlatformInjector> = typed.clone();

    dyn_injector.paste("Hi.").await.expect("paste");
    dyn_injector.paste("Again.").await.expect("paste");

    assert_eq!(typed.captured(), vec!["Hi.".to_string(), "Again.".into()]);
}

#[tokio::test]
async fn paste_empty_string_returns_nothing_to_paste() {
    let typed = Arc::new(MockInjector::new());
    let dyn_injector: Arc<dyn PlatformInjector> = typed.clone();

    let err = dyn_injector.paste("").await.expect_err("empty must error");

    assert!(matches!(err, MuniError::NothingToPaste));
    assert!(
        typed.captured().is_empty(),
        "rejected paste must not be recorded"
    );
}

#[tokio::test]
async fn paste_future_is_send_and_can_cross_spawn() {
    // The session orchestrator drives `paste(...)` from a tokio task, so the
    // returned future must be `Send`. This test would fail to compile if the
    // trait shape ever drifted away from `Send + Sync` requirements.
    let injector: Arc<dyn PlatformInjector> = Arc::new(MockInjector::new());
    let handle = tokio::spawn(async move { injector.paste("from another task").await });
    handle.await.expect("join").expect("paste");
}

/// Injector that asserts pastes never overlap: it bumps a shared "currently
/// inside paste" counter on entry, holds it across a real delay (the window a
/// second paste could sneak into), and records the peak concurrency seen. A
/// correctly serialized wrapper keeps the peak at 1.
struct OverlapProbeInjector {
    active: AtomicUsize,
    peak: AtomicUsize,
    completed: AtomicUsize,
    hold: Duration,
}

impl OverlapProbeInjector {
    fn new(hold: Duration) -> Self {
        Self {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            hold,
        }
    }
}

#[async_trait]
impl PlatformInjector for OverlapProbeInjector {
    async fn paste(&self, _text: &str) -> Result<(), MuniError> {
        let now = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        // Record the peak concurrency observed inside the critical section.
        self.peak.fetch_max(now, Ordering::AcqRel);
        tokio::time::sleep(self.hold).await;
        self.active.fetch_sub(1, Ordering::AcqRel);
        self.completed.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

/// Plan 039 task 48(a): the `SerializedInjector` decorator must run every paste
/// under one gate so dictation delivery and re-paste can never interleave their
/// snapshot→write→⌘V→restore critical sections. Two concurrent pastes through
/// the wrapper must be observed strictly one-at-a-time.
#[tokio::test]
async fn serialized_injector_never_overlaps_concurrent_pastes() {
    let probe = Arc::new(OverlapProbeInjector::new(Duration::from_millis(50)));
    let inner: Arc<dyn PlatformInjector> = probe.clone();
    let serialized: Arc<dyn PlatformInjector> = Arc::new(SerializedInjector::new(inner));

    // Fire a "dictation delivery" and a "re-paste" concurrently.
    let a = {
        let s = serialized.clone();
        tokio::spawn(async move { s.paste("dictation delivery").await })
    };
    let b = {
        let s = serialized.clone();
        tokio::spawn(async move { s.paste("re-paste").await })
    };
    a.await.expect("join a").expect("paste a");
    b.await.expect("join b").expect("paste b");

    assert_eq!(
        probe.peak.load(Ordering::Acquire),
        1,
        "serialized pastes must never overlap (peak concurrency > 1 means the clipboard race is open)"
    );
    assert_eq!(
        probe.completed.load(Ordering::Acquire),
        2,
        "both pastes must still complete"
    );
}

/// A control test: without the wrapper the probe *does* see overlap, proving the
/// probe actually detects concurrency (so the serialized test above isn't
/// vacuously green).
#[tokio::test]
async fn unserialized_injector_overlaps_prove_probe_detects_concurrency() {
    let probe = Arc::new(OverlapProbeInjector::new(Duration::from_millis(50)));
    let inner: Arc<dyn PlatformInjector> = probe.clone();

    let a = {
        let s = inner.clone();
        tokio::spawn(async move { s.paste("a").await })
    };
    let b = {
        let s = inner.clone();
        tokio::spawn(async move { s.paste("b").await })
    };
    a.await.expect("join a").expect("paste a");
    b.await.expect("join b").expect("paste b");

    assert_eq!(
        probe.peak.load(Ordering::Acquire),
        2,
        "the probe must observe overlap when pastes are NOT serialized"
    );
}
