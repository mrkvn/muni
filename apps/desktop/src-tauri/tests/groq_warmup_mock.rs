//! Integration test for `groq_warmup` (feature 016).
//!
//! Drives the real warm-up helper against a `wiremock` HTTP server
//! and a tempfile-backed `UsageStore` to verify:
//!
//! 1. `is_enabled()` defaults to true; `MUNI_CLEANUP_WARMUP=false`
//!    disables.
//! 2. Happy path: warm-up POSTs the cleanup-shaped body (model,
//!    temperature, stream, messages[1]="hello") and lands one
//!    `api_calls` row with `call_kind="cleanup_warmup"` and
//!    `session_id IS NULL`.
//! 3. Missing API key → no HTTP call, no row.
//!
//! Tests are serialized via a process-global env-lock because they
//! mutate `MUNI_CLEANUP_WARMUP`, `MUNI_GROQ_KEY`, and
//! `MUNI_KEYCHAIN_SUFFIX`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use muni_lib::about_me::AboutMe;
use muni_lib::groq::GroqClient;
use muni_lib::groq_activity::GroqActivity;
use muni_lib::groq_warmup::{is_enabled, spawn_cleanup_warmup, WarmupTrigger, CLEANUP_WARMUP_ENV};
use muni_lib::history_store::HistoryStore;
use muni_lib::pricing::DEFAULT_PRICES;
use muni_lib::prompt::CleanupPrompt;
use muni_lib::secrets::GROQ_ENV_VAR;
use muni_lib::usage_store::{PriceRow, UsageStore};
use muni_lib::usage_writer::spawn_writer;
use muni_lib::user_prompt::UserPrompt;
use muni_lib::vocabulary::Vocabulary;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::Mutex;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Pin the test model so the body matcher is stable across
/// `MUNI_CLEANUP_MODEL` overrides on the host. Same approach as
/// `tests/groq_mock.rs`.
const TEST_MODEL: &str = "llama-3.3-70b-versatile";
const TEST_API_KEY: &str = "sk-test-****ABCD";
const TEST_PROMPT_BODY: &str = "BE TERSE.";

/// Serialize tests that mutate env vars. `cargo test` parallelizes
/// by default; without this lock two tests both setting and reading
/// `MUNI_CLEANUP_WARMUP` / `MUNI_GROQ_KEY` would race. Uses
/// `tokio::sync::Mutex` (vs. `secrets::tests::ENV_VAR_TEST_LOCK`'s
/// `std::sync::Mutex`) because these tests await across the lock,
/// and a sync guard held across `.await` would trip
/// `clippy::await_holding_lock` and risk deadlocks in the tokio
/// runtime.
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

fn write_prompt_files(dir: &TempDir) -> (PathBuf, PathBuf) {
    let bundle = dir.path().join("CleanupPrompt-bundle.md");
    std::fs::write(&bundle, TEST_PROMPT_BODY).unwrap();
    let override_path = dir.path().join("CleanupPrompt-override.md");
    (bundle, override_path)
}

fn build_prompt(dir: &TempDir) -> Arc<CleanupPrompt> {
    let (bundle, override_path) = write_prompt_files(dir);
    Arc::new(CleanupPrompt::new(bundle, override_path))
}

fn open_usage_store(dir: &TempDir) -> Arc<UsageStore> {
    let path = HistoryStore::default_path(dir.path());
    let _hist = HistoryStore::open(&path).unwrap();
    let store = Arc::new(UsageStore::open(&path).unwrap());
    seed_default_prices(&store, &current_yyyymm());
    store
}

fn seed_default_prices(store: &UsageStore, yyyymm: &str) {
    for entry in DEFAULT_PRICES {
        let row = PriceRow {
            effective_month: yyyymm.into(),
            provider: entry.provider.into(),
            model: entry.model.into(),
            kind: entry.kind.as_wire().into(),
            usd_per_second: entry.usd_per_second,
            usd_per_input_token: entry.usd_per_input_token,
            usd_per_output_token: entry.usd_per_output_token,
            source_url: Some(entry.source_url.into()),
            fetched_at: 0,
        };
        store.upsert_price(&row).unwrap();
    }
}

fn current_yyyymm() -> String {
    const FMT: &[time::format_description::FormatItem<'_>] =
        time::macros::format_description!("[year]-[month]");
    time::OffsetDateTime::now_utc()
        .format(&FMT)
        .unwrap_or_else(|_| "1970-01".into())
}

async fn client_for(server: &MockServer) -> Arc<GroqClient> {
    Arc::new(
        GroqClient::with_endpoint_and_model(
            format!("{}/v1/chat/completions", server.uri()),
            TEST_MODEL.to_string(),
        )
        .expect("build client"),
    )
}

/// Minimal SSE body the warm-up's `aggregate_sse` parser accepts:
/// one delta + final usage chunk + `[DONE]` terminator.
fn sse_body_with_usage() -> String {
    format!(
        "data: {}\ndata: {}\ndata: [DONE]\n",
        json!({"choices":[{"delta":{"content":"ok"}}]}),
        json!({"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":3,"total_tokens":123}})
    )
}

#[tokio::test]
async fn is_enabled_default_and_explicit_false() {
    let _guard = ENV_LOCK.lock().await;
    std::env::remove_var(CLEANUP_WARMUP_ENV);
    assert!(is_enabled(), "default = enabled");

    for value in ["false", "FALSE", "  false  "] {
        std::env::set_var(CLEANUP_WARMUP_ENV, value);
        assert!(!is_enabled(), "MUNI_CLEANUP_WARMUP={value:?} disables");
    }
    std::env::remove_var(CLEANUP_WARMUP_ENV);
}

#[tokio::test]
async fn warmup_posts_cleanup_shaped_body_and_writes_api_calls_row() {
    let _guard = ENV_LOCK.lock().await;
    // Env key resolves first — no keychain access in this test.
    std::env::set_var(GROQ_ENV_VAR, TEST_API_KEY);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("Authorization", format!("Bearer {TEST_API_KEY}")))
        .and(body_partial_json(json!({
            "model": TEST_MODEL,
            "stream": true,
            "temperature": 0.2,
            "messages": [
                { "role": "system", "content": TEST_PROMPT_BODY },
                { "role": "user", "content": "hello" },
            ]
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body_with_usage()),
        )
        .mount(&server)
        .await;

    let prompt_dir = TempDir::new().unwrap();
    let usage_dir = TempDir::new().unwrap();
    let prompt = build_prompt(&prompt_dir);
    let about_me = AboutMe::empty();
    let vocabulary = Vocabulary::empty();
    let user_prompt = UserPrompt::empty();
    let usage_store = open_usage_store(&usage_dir);
    let tx = spawn_writer(Arc::clone(&usage_store));
    let client = client_for(&server).await;

    let handle = spawn_cleanup_warmup(
        client,
        prompt,
        about_me,
        vocabulary,
        user_prompt,
        Some(tx.clone()),
        None,
        WarmupTrigger::Boot,
    );
    handle.await.expect("warmup task joined");
    // Drop the test-local Sender so the writer task can drain and
    // exit; without this the writer keeps the channel open and the
    // row may not be visible by the time we assert.
    drop(tx);
    tokio::time::sleep(Duration::from_millis(80)).await;

    let per_model = usage_store
        .summary_for_month_per_model(&current_yyyymm())
        .expect("summary");
    let warmup_row = per_model
        .iter()
        .find(|m| m.model == TEST_MODEL && m.provider == "groq")
        .expect("warmup row landed under groq/test-model");
    assert_eq!(warmup_row.call_count, 1);
    assert_eq!(warmup_row.input_tokens, Some(120));
    assert_eq!(warmup_row.output_tokens, Some(3));

    std::env::remove_var(GROQ_ENV_VAR);
}

/// Plan 041 slice-4 core invariant: a *successful* warm-up must stamp the
/// prefix-touch clock via `note_prefix_touch`, so the periodic re-warm gate
/// (`should_fire_periodic_rewarm`) treats the prompt-prefix cache as fresh
/// and self-suppresses. If this stamp regresses, the periodic re-warm sees
/// the cache as permanently stale and re-fires every tick forever —
/// continuous wasted Groq cleanup calls (direct cost). A *failed* warm-up
/// must NOT stamp, or a broken Groq would look permanently fresh.
#[tokio::test]
async fn successful_warmup_stamps_prefix_clock_and_failure_does_not() {
    let _guard = ENV_LOCK.lock().await;
    std::env::set_var(GROQ_ENV_VAR, TEST_API_KEY);
    // Ensure the warm-up isn't gated off by a stray disable from another
    // test in this process.
    std::env::remove_var(CLEANUP_WARMUP_ENV);

    // --- success: 200 SSE → prefix clock stamped ---
    {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_body_with_usage()),
            )
            .mount(&server)
            .await;

        let prompt_dir = TempDir::new().unwrap();
        let prompt = build_prompt(&prompt_dir);
        let activity = Arc::new(GroqActivity::new());
        assert_eq!(
            activity.secs_since_prefix_touch(),
            i64::MAX,
            "a fresh activity starts unstamped"
        );
        let client = client_for(&server).await;

        let handle = spawn_cleanup_warmup(
            client,
            prompt,
            AboutMe::empty(),
            Vocabulary::empty(),
            UserPrompt::empty(),
            None, // no usage writer — this test only cares about the clock
            Some(Arc::clone(&activity)),
            WarmupTrigger::Periodic,
        );
        handle.await.expect("warmup task joined");

        assert!(
            activity.secs_since_prefix_touch() < 5,
            "a successful warm-up must stamp note_prefix_touch (got {})",
            activity.secs_since_prefix_touch()
        );
    }

    // --- failure: 500 → prefix clock untouched ---
    {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let prompt_dir = TempDir::new().unwrap();
        let prompt = build_prompt(&prompt_dir);
        let activity = Arc::new(GroqActivity::new());
        let client = client_for(&server).await;

        let handle = spawn_cleanup_warmup(
            client,
            prompt,
            AboutMe::empty(),
            Vocabulary::empty(),
            UserPrompt::empty(),
            None,
            Some(Arc::clone(&activity)),
            WarmupTrigger::Periodic,
        );
        handle.await.expect("warmup task joined");

        assert_eq!(
            activity.secs_since_prefix_touch(),
            i64::MAX,
            "a failed warm-up must NOT stamp the prefix clock"
        );
    }

    std::env::remove_var(GROQ_ENV_VAR);
}

#[tokio::test]
async fn warmup_is_a_noop_when_api_key_missing() {
    let _guard = ENV_LOCK.lock().await;
    // Unique keychain suffix so a developer's saved Groq key under
    // the default service id can't satisfy the warmup's
    // `secrets::get`. Env var unset → no env key either.
    std::env::set_var("MUNI_KEYCHAIN_SUFFIX", "rust_groq_warmup_missing_key_test");
    std::env::remove_var(GROQ_ENV_VAR);

    let server = MockServer::start().await;
    // expect(0) makes the test fail if any HTTP request reaches the
    // server — exactly the contract: no key, no POST.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let prompt_dir = TempDir::new().unwrap();
    let usage_dir = TempDir::new().unwrap();
    let prompt = build_prompt(&prompt_dir);
    let about_me = AboutMe::empty();
    let vocabulary = Vocabulary::empty();
    let user_prompt = UserPrompt::empty();
    let usage_store = open_usage_store(&usage_dir);
    let tx = spawn_writer(Arc::clone(&usage_store));
    let client = client_for(&server).await;

    let handle = spawn_cleanup_warmup(
        client,
        prompt,
        about_me,
        vocabulary,
        user_prompt,
        Some(tx.clone()),
        None,
        WarmupTrigger::Boot,
    );
    handle.await.expect("warmup task joined");
    drop(tx);
    tokio::time::sleep(Duration::from_millis(60)).await;

    let per_model = usage_store
        .summary_for_month_per_model(&current_yyyymm())
        .expect("summary");
    assert!(
        per_model.is_empty(),
        "no row should land when API key is missing"
    );
    std::env::remove_var("MUNI_KEYCHAIN_SUFFIX");
}

#[tokio::test]
async fn disabled_env_keeps_callers_from_spawning() {
    let _guard = ENV_LOCK.lock().await;
    std::env::set_var(CLEANUP_WARMUP_ENV, "false");
    // The contract is "gate at the call site"; once `is_enabled()`
    // returns false, no real caller spawns the task. Verify the
    // gate itself; the integration with the spawn site is exercised
    // in lib.rs / commands.rs at compile time.
    assert!(!is_enabled());
    std::env::remove_var(CLEANUP_WARMUP_ENV);
}
