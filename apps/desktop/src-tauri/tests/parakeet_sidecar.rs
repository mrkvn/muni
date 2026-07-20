//! Integration tests for `ParakeetClient` driving a real subprocess — the
//! test-only `fake_parakeet_sidecar` bin (see `tests/fixtures/`), which speaks
//! the wire protocol without ONNX or the model. Mirrors the mock-server style
//! of `tests/deepgram_mock.rs`, but for a stdin/stdout sidecar. No 583 MB model
//! is needed, so these run in CI.
//!
//! Gated behind the `test-fixtures` feature: the fixture bin is only built (and
//! `CARGO_BIN_EXE_fake_parakeet_sidecar` only set) under that feature, which
//! keeps the fixture out of release builds. Run via `make test` (or
//! `cargo test --features test-fixtures`).
#![cfg(feature = "test-fixtures")]

use std::path::PathBuf;

use muni_lib::error::MuniError;
use muni_lib::parakeet::ParakeetClient;

/// Path to the fake sidecar binary Cargo builds for integration tests.
fn fake_sidecar_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_parakeet_sidecar"))
}

#[tokio::test]
async fn spawn_warms_and_transcribe_roundtrips() {
    let model_dir = tempfile::tempdir().expect("tempdir");
    let client = ParakeetClient::spawn(fake_sidecar_bin(), Some(model_dir.path().to_path_buf()))
        .await
        .expect("fake sidecar should spawn and report READY");
    assert!(client.pid().is_some(), "warm sidecar should expose a pid");

    let (text, infer_ms) = client
        .transcribe(&[0i16, 1, -1, 100])
        .await
        .expect("first transcribe should succeed");
    assert_eq!(text, "fake transcript");
    assert_eq!(infer_ms, 7);

    // The process stays warm: a second press reuses it.
    let (text2, _) = client
        .transcribe(&[5i16; 32])
        .await
        .expect("second transcribe should succeed on the warm process");
    assert_eq!(text2, "fake transcript");
}

#[tokio::test]
async fn spawn_missing_binary_degrades_gracefully() {
    let model_dir = tempfile::tempdir().expect("tempdir");
    let result = ParakeetClient::spawn(
        PathBuf::from("/nonexistent/parakeet-sidecar-does-not-exist"),
        Some(model_dir.path().to_path_buf()),
    )
    .await;
    let Err(err) = result else {
        panic!("a missing binary must error so the caller maps it to None");
    };
    assert!(
        matches!(err, MuniError::ParakeetSidecarUnavailable { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn spawn_missing_model_dir_degrades_gracefully() {
    let result = ParakeetClient::spawn(
        fake_sidecar_bin(),
        Some(PathBuf::from("/nonexistent/model/dir/parakeet")),
    )
    .await;
    let Err(err) = result else {
        panic!("a missing model dir must error so the caller maps it to None");
    };
    assert!(
        matches!(err, MuniError::ParakeetSidecarUnavailable { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn transcribe_lazily_respawns_after_a_crash() {
    // A model-dir path containing "oneshot" makes the fake sidecar exit after
    // one response, so we can drive the crash → error → respawn sequence.
    let base = tempfile::tempdir().expect("tempdir");
    let oneshot_dir = base.path().join("oneshot");
    std::fs::create_dir(&oneshot_dir).expect("create oneshot dir");

    let client = ParakeetClient::spawn(fake_sidecar_bin(), Some(oneshot_dir))
        .await
        .expect("spawn ok");

    // 1) Served, then the sidecar exits.
    let (t1, _) = client.transcribe(&[1i16, 2, 3]).await.expect("first ok");
    assert_eq!(t1, "fake transcript");

    // 2) The dead sidecar surfaces a transcribe failure (caller falls back to
    //    Deepgram for this press); the client drops the broken child.
    let err = client
        .transcribe(&[1i16, 2, 3])
        .await
        .expect_err("a dead sidecar should error, not hang");
    assert!(
        matches!(err, MuniError::ParakeetTranscribeFailed { .. }),
        "got {err:?}"
    );

    // 3) The respawn runs in the background (task 18: the press path never
    //    reloads inline — callers fall back to Deepgram meanwhile), so whether
    //    the immediately-next call is served is a race the client does not
    //    promise to win. Poll until the respawned sidecar serves; until then
    //    every miss must be the typed fail-fast error.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let t3 = loop {
        match client.transcribe(&[1i16, 2, 3]).await {
            Ok((text, _)) => break text,
            Err(MuniError::ParakeetSidecarUnavailable { .. })
                if std::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(err) => panic!("expected eventual respawn success, got {err:?}"),
        }
    };
    assert_eq!(t3, "fake transcript");
}
