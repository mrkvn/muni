//! Groq cleanup HTTP/SSE client.
//!
//! Mirrors Swift v1's `GroqClient` (`muni/Core/GroqClient.swift`):
//! a single-shot streaming chat-completion request that aggregates
//! `choices[0].delta.content` deltas across `data: ` lines and stops on
//! `data: [DONE]`. Same endpoint, same headers, same model defaults, same
//! 8 second timeout, same `text/event-stream` Accept header.
//!
//! Wire contract (must stay byte-identical with Swift v1):
//! - URL: see [`DEFAULT_ENDPOINT`].
//! - Method: `POST`.
//! - Headers: `Authorization: Bearer <KEY>`, `Content-Type: application/json`,
//!   `Accept: text/event-stream`.
//! - Body: `{ model, temperature, stream: true, messages: [...] }`.
//! - Response: SSE lines. Lines starting with `data: ` carry either
//!   `[DONE]` (terminator) or a JSON envelope; we extract
//!   `choices[0].delta.content` (streaming) or `choices[0].message.content`
//!   (non-streaming fallback) and concatenate.
//!
//! Error handling matches Swift: HTTP non-2xx → typed `GroqServerError`
//! with a truncated body; per-line JSON parse failures are silently skipped
//! (an isolated bad line should not fail the whole press); transport
//! errors → `GroqConnectionFailed`.

use std::time::Duration;

use futures_util::StreamExt;
use serde::Serialize;

use crate::asr_stream::truncate_for_log;
use crate::error::MuniError;

/// Production Groq chat-completions endpoint.
pub const DEFAULT_ENDPOINT: &str = "https://api.groq.com/openai/v1/chat/completions";

/// Env var that redirects the cleanup HTTP client at boot. Empty,
/// unset, or whitespace-only → [`DEFAULT_ENDPOINT`]. Intended as a
/// dogfood/debug knob: point Muni at a local mock server to exercise
/// the cleanup pipeline — including the failure-and-retry path —
/// without hitting prod Groq. The override is read once at boot
/// inside [`GroqClient::new`] so a single press never pays an env
/// lookup on its hot path.
pub const GROQ_ENDPOINT_ENV: &str = "MUNI_GROQ_ENDPOINT";

/// Default model for the cleanup pass. Switched from
/// `openai/gpt-oss-120b` to `openai/gpt-oss-20b` (2026-05-13) to chase
/// lower cleanup latency: dogfood at 120b showed cleanup latency
/// swinging from 370 ms to >2 s on similar-shape transcripts, dominating
/// release→paste time on otherwise-fast presses. The 20b variant
/// stays in the same gpt-oss reasoning family so `reasoning_effort`
/// pinning logic and the `is_gpt_oss_reasoning_model` predicate keep
/// working unchanged. Override per-host without a rebuild by setting
/// [`CLEANUP_MODEL_ENV`] in `.env`.
pub const DEFAULT_MODEL: &str = "openai/gpt-oss-20b";

/// Env var that overrides the cleanup model at boot. Empty / unset
/// → [`DEFAULT_MODEL`]. Mirrors the `MUNI_LID_MODEL` knob on the
/// LID classifier so dev workflows that already source `.env` keep
/// working.
pub const CLEANUP_MODEL_ENV: &str = "MUNI_CLEANUP_MODEL";

/// Default temperature. Mirrors Swift v1's `CleanupPrompt.defaultTemperature`.
pub const DEFAULT_TEMPERATURE: f32 = 0.2;

/// Default `reasoning_effort` for `openai/gpt-oss-*` cleanup requests.
/// Promoted from `"low"` to `"medium"` on 2026-05-25 after sustained
/// dogfooding at `"medium"` (set via env override) confirmed the
/// disambiguation quality wins on rules 9/10 justify the ~29% cleanup
/// latency cost measured in the 2026-05-13 dogfood (~966 ms → ~1250 ms
/// mean). The speed-first priority in [`.claude/CLAUDE.md`] still
/// applies — `"low"` remains available via [`CLEANUP_REASONING_EFFORT_ENV`]
/// for hosts that want the faster baseline back.
pub const DEFAULT_REASONING_EFFORT: &str = "medium";

/// Env var that overrides the cleanup `reasoning_effort` at boot. Empty,
/// unset, or invalid → [`DEFAULT_REASONING_EFFORT`]. Mirrors
/// [`CLEANUP_MODEL_ENV`] so dev workflows that already source `.env`
/// can flip the knob without a rebuild. Accepted values are Groq's
/// documented set: `"low"`, `"medium"`, `"high"` (case-insensitive,
/// whitespace-trimmed).
pub const CLEANUP_REASONING_EFFORT_ENV: &str = "MUNI_CLEANUP_REASONING_EFFORT";

/// Maximum `completion_tokens` budget for `openai/gpt-oss-*` cleanup
/// requests. Groq's server-side default is 2048, which is too tight
/// for `reasoning_effort=medium` on long inputs: the model exhausts
/// the cap during the reasoning phase and never emits any
/// `delta.content`, leaving the pipeline with an empty response that
/// then falls back to the raw transcript (see
/// `[groq][WARN] empty cleanup — falling back to raw` in the log).
/// 8192 is ~4× the server default, well within the 131k context
/// window even with About Me + Vocabulary prepended, and bounds the
/// worst-case streaming latency. Cleanup output never approaches
/// this size — typical reasoning+content is 1k–4k tokens — so the
/// ceiling is a safety margin, not an allocation we expect to pay
/// for in tokens. We do NOT set this on non-reasoning models
/// (Llama et al), which use the legacy `max_tokens` field and have
/// healthier defaults.
pub const DEFAULT_MAX_COMPLETION_TOKENS: u32 = 8192;

/// Default cleanup model for the **long-press profile**. The dual-config
/// arrangement (short profile = the existing env knobs; long profile =
/// these `*_LONG_*` constants) was introduced 2026-05-14 after dogfood
/// surfaced a model/effort trade-off keyed on audio duration: see
/// `.claude/learned/005_cleanup_model_effort_tradeoff_by_audio_length.md`.
/// On long narrative presses (>~5 s of audio, ~3k+ output tokens),
/// `openai/gpt-oss-20b` at `medium` effort takes ~4 s for cleanup;
/// `openai/gpt-oss-120b` at `low` effort matches it on quality but
/// finishes ~2× faster because the reasoning budget at `low` doesn't
/// scale with input length. We default the long profile to `120b`
/// because `low`-effort reasoning is fast at scale on the larger model
/// while output quality holds for narrative inputs. Override per-host
/// without a rebuild by setting [`CLEANUP_LONG_MODEL_ENV`] in `.env`.
pub const DEFAULT_LONG_MODEL: &str = "openai/gpt-oss-120b";

/// Env var that overrides the cleanup model for the long-press
/// profile. Empty / unset → [`DEFAULT_LONG_MODEL`]. Mirrors the
/// [`CLEANUP_MODEL_ENV`] knob so the same `.env` workflow flips
/// the long-profile model without a rebuild. Short-profile model is
/// still controlled by [`CLEANUP_MODEL_ENV`] for backwards compatibility
/// — long-profile gets its own key so the two can be tuned independently.
pub const CLEANUP_LONG_MODEL_ENV: &str = "MUNI_CLEANUP_LONG_MODEL";

/// Default `reasoning_effort` for the long-press profile. `"low"` keeps
/// the reasoning budget bounded regardless of input length — the
/// inflection point in the 2026-05-14 observation: at `medium` on
/// `20b`, reasoning tokens scale with input and dominate latency on
/// long presses; at `low` on `120b`, reasoning stays short while the
/// faster output throughput of `120b` clears the longer content body
/// quickly. See `.claude/learned/005_*.md` for the trade-off data.
pub const DEFAULT_LONG_REASONING_EFFORT: &str = "low";

/// Env var that overrides the cleanup `reasoning_effort` for the
/// long-press profile. Empty, unset, or invalid →
/// [`DEFAULT_LONG_REASONING_EFFORT`]. Accepted values match Groq's
/// documented set: `"low"`, `"medium"`, `"high"` (case-insensitive,
/// whitespace-trimmed). Mirrors [`CLEANUP_REASONING_EFFORT_ENV`].
pub const CLEANUP_LONG_REASONING_EFFORT_ENV: &str = "MUNI_CLEANUP_LONG_REASONING_EFFORT";

/// Default threshold (in seconds of audio / hotkey-press wall-clock)
/// above which the cleanup pass routes to the long profile. Presses
/// at or below this threshold use the short profile (the existing env
/// values), so the boundary belongs to short — see
/// [`pick_cleanup_config_for_duration`]. Lowered to 10.0 s on
/// 2026-05-25 after sustained dogfooding at that value (set via env
/// override) showed it still comfortably contains the short
/// disambiguation-test catalog phrases (`docs/qa/013`, all < ~5 s)
/// while routing medium-length thoughts (10–15 s) to the stronger
/// long profile sooner. The earlier 15 s pick was a conservative
/// reaction to a 5 s default that clipped 5–6 s short presses; 10 s
/// is the resting point that emerged from real use. The env override
/// lets per-host tuning continue without a rebuild. Stored as `f64`
/// because press durations are `Duration`s converted via
/// `as_secs_f64`.
pub const DEFAULT_LONG_PRESS_THRESHOLD_S: f64 = 10.0;

/// Env var that overrides the long-press threshold in seconds.
/// Empty, unset, non-numeric, or non-positive →
/// [`DEFAULT_LONG_PRESS_THRESHOLD_S`]. Non-positive (≤ 0) is rejected
/// because routing every press to long (with `0.0`) is better
/// expressed by setting [`CLEANUP_LONG_MODEL_ENV`] to match the short
/// model — and a negative value is almost certainly a typo.
pub const CLEANUP_LONG_PRESS_THRESHOLD_ENV: &str = "MUNI_CLEANUP_LONG_PRESS_THRESHOLD_S";

/// Default request timeout for non-cleanup callers (e.g. the warmup
/// ping). Mirrors Swift v1's `GroqClient.timeoutSeconds`. The cleanup
/// hot path uses [`resolve_cleanup_timeout_for_duration`] instead so
/// the user-perceived stall on a failing Groq call stays bounded by
/// the profile-specific timeout, not this generic 8 s default.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8);

/// Default cleanup request timeout (short profile, milliseconds).
/// Sized to swallow the long tail of *successful* cleanups (~5 s on
/// `openai/gpt-oss-20b` in the last 30 days of dogfood) while keeping
/// the failure-mode stall under the 8 s the user noticed.
/// Override per-host without a rebuild by setting
/// [`CLEANUP_TIMEOUT_ENV`] in `.env`.
pub const DEFAULT_CLEANUP_TIMEOUT_MS: u64 = 5_000;

/// Env var that overrides the cleanup timeout (short profile, ms).
/// Empty, unset, non-numeric, or ≤ 0 → [`DEFAULT_CLEANUP_TIMEOUT_MS`].
pub const CLEANUP_TIMEOUT_ENV: &str = "MUNI_CLEANUP_TIMEOUT_MS";

/// Default cleanup request timeout (long profile, milliseconds).
/// `openai/gpt-oss-120b` at `low` effort has consistently finished in
/// ≤ ~3 s in dogfood, so 4 s is a generous-but-bounded ceiling. Also
/// used as the retry-attempt timeout (the retry always runs on the
/// long-profile model regardless of which profile the primary used).
pub const DEFAULT_CLEANUP_LONG_TIMEOUT_MS: u64 = 4_000;

/// Env var that overrides the cleanup timeout (long profile, ms).
/// Empty, unset, non-numeric, or ≤ 0 → [`DEFAULT_CLEANUP_LONG_TIMEOUT_MS`].
pub const CLEANUP_LONG_TIMEOUT_ENV: &str = "MUNI_CLEANUP_LONG_TIMEOUT_MS";

/// Model used on the cleanup retry attempt after the primary cleanup
/// call fails. Pinned to `openai/gpt-oss-120b` because the failure mode
/// we're recovering from (Groq returning a malformed body or hanging)
/// tends to be model/region-specific; falling back to the larger
/// reasoning model gives the retry a meaningfully different shot at
/// success without burning another full short-profile timeout.
pub const CLEANUP_RETRY_MODEL: &str = "openai/gpt-oss-120b";

/// `reasoning_effort` used on the cleanup retry attempt. `"low"` keeps
/// the retry inside the long-profile timeout budget.
pub const CLEANUP_RETRY_EFFORT: &str = "low";

/// Maximum number of bytes to capture from a non-2xx response body before
/// truncation. Matches the Swift v1 ceiling so log volume stays bounded
/// when the server replies with a verbose error page.
const MAX_ERROR_BODY_BYTES: usize = 4_096;

/// One Groq chat message.
///
/// Field names match the OpenAI-compatible Groq API exactly. Lives in this
/// module (rather than `prompt.rs`) so the wire format stays close to its
/// transport.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GroqMessage {
    pub role: String,
    pub content: String,
}

impl GroqMessage {
    pub fn system(content: String) -> Self {
        Self {
            role: "system".into(),
            content,
        }
    }

    pub fn user(content: String) -> Self {
        Self {
            role: "user".into(),
            content,
        }
    }
}

/// `stream_options` sub-object for streaming chat completions.
///
/// `include_usage: true` opts the request into a final SSE chunk
/// carrying token counts after the last `delta` and before `[DONE]`.
/// The OpenAI-compatible Groq endpoint follows the same convention.
/// Feature 005 reads the chunk to record `input_tokens` /
/// `output_tokens` for the cleanup billing line.
#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// Request body for the Groq chat-completions endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct GroqRequest {
    pub model: String,
    pub temperature: f32,
    pub stream: bool,
    pub messages: Vec<GroqMessage>,
    /// Only serialised when `Some`; mocks and non-streaming callers
    /// can leave this field off without affecting the wire shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// OpenAI-style reasoning knob, only meaningful for Groq's
    /// `openai/gpt-oss-*` reasoning family. Sourced from
    /// [`resolve_cleanup_reasoning_effort`] (env-overridable via
    /// [`CLEANUP_REASONING_EFFORT_ENV`], defaults to
    /// [`DEFAULT_REASONING_EFFORT`]) when the model is a gpt-oss
    /// variant. Skipped from the wire body for non-reasoning models
    /// (Llama et al), which reject the field outright.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Cap on total `completion_tokens` (reasoning + content) for
    /// gpt-oss reasoning cleanup requests. Set to
    /// [`DEFAULT_MAX_COMPLETION_TOKENS`] only when the model is a
    /// gpt-oss variant — Llama et al use the legacy `max_tokens`
    /// field and we leave them on their server defaults. See the
    /// constant's docs for the rationale (2026-05-14 medium-effort
    /// empty-cleanup regression).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
}

impl GroqRequest {
    /// Build a streaming cleanup request targeting `model` with the v1
    /// default temperature and `stream_options.include_usage = true`
    /// so the final SSE chunk carries prompt/completion token counts.
    /// For `openai/gpt-oss-*` models the request also sets
    /// `reasoning_effort` from [`resolve_cleanup_reasoning_effort`] (so
    /// `.env` can override without a rebuild). The orchestrator
    /// (Phase 5+) is the only construction site outside tests; it
    /// sources `model` from [`GroqClient::cleanup_model`], which
    /// resolves at boot from [`CLEANUP_MODEL_ENV`] / [`DEFAULT_MODEL`].
    pub fn cleanup(messages: Vec<GroqMessage>, model: &str) -> Self {
        Self::cleanup_with_effort(messages, model, &resolve_cleanup_reasoning_effort())
    }

    /// **Production entry point** for cleanup requests. Resolves the
    /// `(model, effort)` pair from the press's wall-clock duration via
    /// [`resolve_cleanup_config_for_duration`] — short presses (≤
    /// threshold) get the short profile (env-overridable via
    /// [`CLEANUP_MODEL_ENV`] / [`CLEANUP_REASONING_EFFORT_ENV`]); long
    /// presses (> threshold) get the long profile (env-overridable via
    /// [`CLEANUP_LONG_MODEL_ENV`] / [`CLEANUP_LONG_REASONING_EFFORT_ENV`]).
    /// Threshold is env-overridable via [`CLEANUP_LONG_PRESS_THRESHOLD_ENV`]
    /// and defaults to [`DEFAULT_LONG_PRESS_THRESHOLD_S`].
    ///
    /// Callers should use this constructor unless they have a specific
    /// reason to use [`Self::cleanup`] (short-profile-only shortcut, used
    /// by some tests) or [`Self::cleanup_with_effort`] (explicit-effort
    /// integration test). See `.claude/learned/005_*.md` for the
    /// trade-off data that motivates the dual-config arrangement.
    pub fn cleanup_for_duration(messages: Vec<GroqMessage>, press_duration_s: f64) -> Self {
        let (model, effort) = resolve_cleanup_config_for_duration(press_duration_s);
        Self::cleanup_with_effort(messages, &model, &effort)
    }

    /// Explicit-effort variant of [`Self::cleanup`]. Lets tests build a
    /// request body with a known `reasoning_effort` without depending
    /// on env state, and lets callers that want a non-default effort
    /// (e.g. a one-off integration test) opt in. Production callers
    /// should use [`Self::cleanup_for_duration`] so the duration-aware
    /// routing path stays honoured.
    pub fn cleanup_with_effort(messages: Vec<GroqMessage>, model: &str, effort: &str) -> Self {
        let is_reasoning = is_gpt_oss_reasoning_model(model);
        let reasoning_effort = is_reasoning.then(|| effort.to_string());
        let max_completion_tokens = is_reasoning.then_some(DEFAULT_MAX_COMPLETION_TOKENS);
        Self {
            model: model.to_string(),
            temperature: DEFAULT_TEMPERATURE,
            stream: true,
            messages,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            reasoning_effort,
            max_completion_tokens,
        }
    }
}

/// Resolve the cleanup model from the env once. Returns
/// [`DEFAULT_MODEL`] when [`CLEANUP_MODEL_ENV`] is unset, empty, or
/// whitespace-only. Trims surrounding whitespace so a sloppy `.env`
/// entry (`MUNI_CLEANUP_MODEL=  openai/gpt-oss-20b  `) still works.
pub fn resolve_cleanup_model() -> String {
    std::env::var(CLEANUP_MODEL_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

/// Resolve the Groq chat-completions endpoint from the env. Returns
/// [`DEFAULT_ENDPOINT`] when [`GROQ_ENDPOINT_ENV`] is unset, empty,
/// or whitespace-only. Pure-function counterpart lives in
/// [`parse_groq_endpoint`] for tests.
pub fn resolve_groq_endpoint() -> String {
    parse_groq_endpoint(std::env::var(GROQ_ENDPOINT_ENV).ok())
}

/// Pure-function counterpart to [`resolve_groq_endpoint`].
pub(crate) fn parse_groq_endpoint(raw: Option<String>) -> String {
    raw.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string())
}

/// Resolve the cleanup `reasoning_effort` from the env. Returns
/// [`DEFAULT_REASONING_EFFORT`] when [`CLEANUP_REASONING_EFFORT_ENV`]
/// is unset, empty, whitespace-only, or set to a value outside the
/// accepted allow-list (`"low"`, `"medium"`, `"high"`). Case- and
/// whitespace-insensitive on the env value so sloppy `.env` entries
/// like `MUNI_CLEANUP_REASONING_EFFORT=  Medium  ` still work. The
/// parsing logic is factored into [`parse_cleanup_reasoning_effort`]
/// so tests can exercise it without touching process-global env state.
pub fn resolve_cleanup_reasoning_effort() -> String {
    parse_cleanup_reasoning_effort(std::env::var(CLEANUP_REASONING_EFFORT_ENV).ok())
}

/// Pure-function counterpart to [`resolve_cleanup_reasoning_effort`].
/// Takes the raw env value as `Option<String>` so callers can pass any
/// source they like; tests pass synthetic inputs to verify the
/// allow-list and fallback behaviour without `std::env::set_var` races
/// across parallel test threads.
fn parse_cleanup_reasoning_effort(raw: Option<String>) -> String {
    raw.map(|s| s.trim().to_lowercase())
        .filter(|s| matches!(s.as_str(), "low" | "medium" | "high"))
        .unwrap_or_else(|| DEFAULT_REASONING_EFFORT.to_string())
}

/// Resolve the long-press cleanup model from the env. Returns
/// [`DEFAULT_LONG_MODEL`] when [`CLEANUP_LONG_MODEL_ENV`] is unset,
/// empty, or whitespace-only. Mirrors [`resolve_cleanup_model`] for
/// the long profile. See `.claude/learned/005_*.md` for the dual-config
/// rationale.
pub fn resolve_cleanup_long_model() -> String {
    parse_cleanup_long_model(std::env::var(CLEANUP_LONG_MODEL_ENV).ok())
}

/// Pure-function counterpart to [`resolve_cleanup_long_model`]. Trims
/// surrounding whitespace; empty / whitespace-only → default.
fn parse_cleanup_long_model(raw: Option<String>) -> String {
    raw.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_LONG_MODEL.to_string())
}

/// Resolve the long-press `reasoning_effort` from the env. Returns
/// [`DEFAULT_LONG_REASONING_EFFORT`] when
/// [`CLEANUP_LONG_REASONING_EFFORT_ENV`] is unset, empty,
/// whitespace-only, or set to a value outside `{"low", "medium", "high"}`.
/// Mirrors [`resolve_cleanup_reasoning_effort`] for the long profile.
pub fn resolve_cleanup_long_reasoning_effort() -> String {
    parse_cleanup_long_reasoning_effort(std::env::var(CLEANUP_LONG_REASONING_EFFORT_ENV).ok())
}

/// Pure-function counterpart to [`resolve_cleanup_long_reasoning_effort`].
fn parse_cleanup_long_reasoning_effort(raw: Option<String>) -> String {
    raw.map(|s| s.trim().to_lowercase())
        .filter(|s| matches!(s.as_str(), "low" | "medium" | "high"))
        .unwrap_or_else(|| DEFAULT_LONG_REASONING_EFFORT.to_string())
}

/// Resolve the long-press threshold (seconds) from the env. Returns
/// [`DEFAULT_LONG_PRESS_THRESHOLD_S`] when
/// [`CLEANUP_LONG_PRESS_THRESHOLD_ENV`] is unset, empty,
/// whitespace-only, non-numeric, or ≤ 0.
pub fn resolve_cleanup_long_press_threshold_s() -> f64 {
    parse_cleanup_long_press_threshold_s(std::env::var(CLEANUP_LONG_PRESS_THRESHOLD_ENV).ok())
}

/// Pure-function counterpart to [`resolve_cleanup_long_press_threshold_s`].
/// Reject non-positive values defensively: a `0.0` threshold would
/// route every press to long (better expressed by other knobs); a
/// negative value is almost certainly a typo.
fn parse_cleanup_long_press_threshold_s(raw: Option<String>) -> f64 {
    raw.and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_LONG_PRESS_THRESHOLD_S)
}

/// Pure routing function: given a press duration and a threshold,
/// return the matching profile's `(model, effort)` pair. The boundary
/// belongs to **short** — i.e. `duration > threshold` routes to long,
/// while `duration == threshold` and `duration < threshold` route to
/// short. A press exactly at the threshold is a borderline case where
/// the short profile (faster on small inputs) is the safer pick.
pub(crate) fn pick_cleanup_config_for_duration(
    duration_s: f64,
    threshold_s: f64,
    short: (String, String),
    long: (String, String),
) -> (String, String) {
    if duration_s > threshold_s {
        long
    } else {
        short
    }
}

/// Resolve the cleanup timeout (short profile) from the env. Returns
/// [`DEFAULT_CLEANUP_TIMEOUT_MS`] when [`CLEANUP_TIMEOUT_ENV`] is
/// unset, empty, whitespace-only, non-numeric, or ≤ 0.
pub fn resolve_cleanup_timeout() -> Duration {
    parse_cleanup_timeout_ms(std::env::var(CLEANUP_TIMEOUT_ENV).ok())
}

/// Pure-function counterpart to [`resolve_cleanup_timeout`]. Reject
/// non-positive values defensively: a `0` timeout would fire instantly
/// and is almost certainly a typo. Returned as `Duration` so the call
/// site never juggles raw milliseconds.
pub(crate) fn parse_cleanup_timeout_ms(raw: Option<String>) -> Duration {
    parse_timeout_ms_with_default(raw, DEFAULT_CLEANUP_TIMEOUT_MS)
}

/// Resolve the cleanup timeout (long profile) from the env. Returns
/// [`DEFAULT_CLEANUP_LONG_TIMEOUT_MS`] when [`CLEANUP_LONG_TIMEOUT_ENV`]
/// is unset, empty, whitespace-only, non-numeric, or ≤ 0.
pub fn resolve_cleanup_long_timeout() -> Duration {
    parse_cleanup_long_timeout_ms(std::env::var(CLEANUP_LONG_TIMEOUT_ENV).ok())
}

/// Pure-function counterpart to [`resolve_cleanup_long_timeout`].
pub(crate) fn parse_cleanup_long_timeout_ms(raw: Option<String>) -> Duration {
    parse_timeout_ms_with_default(raw, DEFAULT_CLEANUP_LONG_TIMEOUT_MS)
}

/// Shared timeout parser. Trims, parses as `u64` ms, rejects ≤ 0.
fn parse_timeout_ms_with_default(raw: Option<String>, default_ms: u64) -> Duration {
    let ms = raw
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default_ms);
    Duration::from_millis(ms)
}

/// Resolve the cleanup timeout for a given press duration. Short
/// presses (≤ threshold) get [`resolve_cleanup_timeout`]; long presses
/// (> threshold) get [`resolve_cleanup_long_timeout`]. The boundary
/// rule mirrors [`pick_cleanup_config_for_duration`] so the timeout
/// always matches the model/effort the press actually uses.
pub fn resolve_cleanup_timeout_for_duration(duration_s: f64) -> Duration {
    let threshold = resolve_cleanup_long_press_threshold_s();
    if duration_s > threshold {
        resolve_cleanup_long_timeout()
    } else {
        resolve_cleanup_timeout()
    }
}

/// Resolve the cleanup `(model, effort)` pair for a given press
/// duration. Reads three env vars (short model, short effort, long
/// model, long effort, threshold) and forwards to
/// [`pick_cleanup_config_for_duration`]. Returns owned `String`s
/// because the resolvers do. This is the env-reading wrapper used by
/// [`GroqRequest::cleanup_for_duration`] and by the orchestrator when
/// it needs to record the chosen model on the usage row.
pub fn resolve_cleanup_config_for_duration(duration_s: f64) -> (String, String) {
    let short = (resolve_cleanup_model(), resolve_cleanup_reasoning_effort());
    let long = (
        resolve_cleanup_long_model(),
        resolve_cleanup_long_reasoning_effort(),
    );
    let threshold = resolve_cleanup_long_press_threshold_s();
    pick_cleanup_config_for_duration(duration_s, threshold, short, long)
}

/// Whether `model` is a Groq `openai/gpt-oss-*` reasoning model — the
/// only family that accepts (and benefits from) the `reasoning_effort`
/// knob. Mirrors `groq_lid::is_gpt_oss_reasoning_model`; duplicated
/// instead of extracted because the two call sites are independent
/// and the predicate is one line.
fn is_gpt_oss_reasoning_model(model: &str) -> bool {
    model.starts_with("openai/gpt-oss")
}

/// Token usage block surfaced from the final SSE chunk when the
/// request asks for it via `stream_options.include_usage = true`.
///
/// `cached_tokens` is `Some(n)` only when Groq's response carries
/// `usage.prompt_tokens_details.cached_tokens` — the count of input
/// tokens served from Groq's automatic prompt-prefix cache (50%
/// discount). `None` means the server didn't report a value (older
/// API shape, non-gpt-oss models, or a cold prefix). The count is a
/// subset of `prompt_tokens`, not additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageBlock {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: Option<i64>,
}

/// Streaming Groq chat-completions client.
///
/// Owns a `reqwest::Client` so the underlying TCP/TLS connection is reused
/// across presses — opening a fresh client every press would add ~200 ms of
/// TLS handshake on top of the model latency we already pay.
#[derive(Debug, Clone)]
pub struct GroqClient {
    endpoint: String,
    cleanup_model: String,
    http: reqwest::Client,
    timeout: Duration,
}

impl GroqClient {
    /// Construct a client targeting the Groq chat-completions endpoint
    /// resolved from the env ([`resolve_groq_endpoint`], which falls
    /// back to [`DEFAULT_ENDPOINT`]) with the cleanup model resolved
    /// from the env ([`resolve_cleanup_model`]). The endpoint override
    /// is intended for dogfood/debug — point it at a local mock to
    /// exercise the cleanup retry path without touching prod Groq.
    pub fn new() -> Result<Self, MuniError> {
        Self::with_endpoint_and_model(resolve_groq_endpoint(), resolve_cleanup_model())
    }

    /// Construct a client targeting a custom endpoint with the cleanup
    /// model resolved from the env. Used by integration tests that
    /// point the real client at a `wiremock` server but don't care
    /// which model is on the wire.
    pub fn with_endpoint(endpoint: String) -> Result<Self, MuniError> {
        Self::with_endpoint_and_model(endpoint, resolve_cleanup_model())
    }

    /// Construct a client targeting a custom endpoint with an explicit
    /// cleanup model. Tests use this when they assert on the
    /// model-id field in the request body and need a stable value
    /// regardless of host-level env state.
    pub fn with_endpoint_and_model(
        endpoint: String,
        cleanup_model: String,
    ) -> Result<Self, MuniError> {
        // Default reqwest `pool_idle_timeout` is 90 s — too short for
        // the realistic "save a setting in Settings, then dictate a
        // few minutes later" pattern. Bumping to 5 min keeps the
        // TLS-warm half of the cleanup warm-up alive across that gap.
        // Worst case (NAT/server-side close mid-window): reqwest
        // transparently retries with a fresh connection, costing one
        // extra round-trip on that one press — same as a cold start
        // would have been. No user-visible regression possible.
        let http = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| MuniError::GroqConnectionFailed {
                reason: e.to_string(),
            })?;
        Ok(Self {
            endpoint,
            cleanup_model,
            http,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// The model id this client uses for cleanup requests. Resolved
    /// once at construction so a single press doesn't pay an env-read
    /// in its hot path and so the usage-record model column matches
    /// the model actually on the wire.
    pub fn cleanup_model(&self) -> &str {
        &self.cleanup_model
    }

    /// Send a streaming completion and return the trimmed concatenated
    /// `delta.content` (or `message.content` if the server falls back to a
    /// non-streaming response) along with any `usage` block emitted in the
    /// final SSE chunk (when the request opted in via
    /// `stream_options.include_usage`).
    ///
    /// Uses the client's default [`DEFAULT_TIMEOUT`]. The cleanup hot
    /// path should call [`Self::complete_with_timeout`] with a
    /// profile-specific budget so a Groq stall stays bounded by the
    /// configured short/long timeout rather than this generic ceiling.
    ///
    /// Returns:
    /// - [`MuniError::GroqMissingApiKey`] if `api_key` is empty.
    /// - [`MuniError::GroqServerError`] for any non-2xx response.
    /// - [`MuniError::GroqConnectionFailed`] for transport/timeout errors.
    /// - [`MuniError::GroqInvalidResponse`] if the response body is not
    ///   readable as UTF-8 mid-stream (a bad-line JSON parse is silently
    ///   skipped, mirroring Swift v1).
    pub async fn complete(
        &self,
        request: GroqRequest,
        api_key: &str,
    ) -> Result<(String, Option<UsageBlock>), MuniError> {
        self.complete_with_timeout(request, api_key, self.timeout)
            .await
    }

    /// Same as [`Self::complete`] but with an explicit per-call timeout.
    /// Used by the cleanup orchestrator so short-profile and long-profile
    /// presses (and the retry attempt) each carry their own bounded
    /// budget without mutating the client's shared default.
    pub async fn complete_with_timeout(
        &self,
        request: GroqRequest,
        api_key: &str,
        timeout: Duration,
    ) -> Result<(String, Option<UsageBlock>), MuniError> {
        if api_key.is_empty() {
            return Err(MuniError::GroqMissingApiKey);
        }

        let response = self
            .http
            .post(&self.endpoint)
            .timeout(timeout)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Accept", "text/event-stream")
            .json(&request)
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
            log::warn!(target: "groq", "non-2xx HTTP {}: {}", status.as_u16(), truncated);
            // A 401/403 means the key is bad (expired/revoked), not a transient
            // server hiccup. Split it into the actionable `GroqKeyRejected` kind
            // (silent notification → API Keys). Note: cleanup still has a
            // raw-text fallback, so this does not block the paste.
            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(MuniError::GroqKeyRejected);
            }
            return Err(MuniError::GroqServerError {
                status: status.as_u16(),
                body: truncated,
            });
        }

        aggregate_sse(response).await
    }
}

/// Drain the response stream as SSE, accumulating `delta.content` deltas
/// until `[DONE]` or clean EOF. Also captures the final `usage` chunk's
/// `prompt_tokens` / `completion_tokens` when the server emits one
/// (only happens if the request opted in via
/// `stream_options.include_usage = true`).
async fn aggregate_sse(
    response: reqwest::Response,
) -> Result<(String, Option<UsageBlock>), MuniError> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut cleaned = String::new();
    let mut usage: Option<UsageBlock> = None;

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| MuniError::GroqConnectionFailed {
            reason: e.to_string(),
        })?;
        let chunk_str = std::str::from_utf8(&bytes).map_err(|_| MuniError::GroqInvalidResponse)?;
        buffer.push_str(chunk_str);

        while let Some(line_end) = buffer.find('\n') {
            let line: String = buffer.drain(..=line_end).collect();
            let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
            match parse_sse_line(trimmed) {
                SseLine::Done => return Ok((cleaned.trim().to_string(), usage)),
                SseLine::Delta(text) => cleaned.push_str(&text),
                SseLine::Usage(block) => usage = Some(block),
                SseLine::Skip => continue,
            }
        }
    }

    // Flush any trailing line not terminated by `\n` (rare, but possible
    // when the server closes mid-line).
    if !buffer.is_empty() {
        match parse_sse_line(buffer.trim()) {
            SseLine::Delta(text) => cleaned.push_str(&text),
            SseLine::Usage(block) => usage = Some(block),
            SseLine::Done | SseLine::Skip => {}
        }
    }

    Ok((cleaned.trim().to_string(), usage))
}

/// Outcome of parsing one SSE line.
#[derive(Debug, PartialEq, Eq)]
enum SseLine {
    /// Concatenate this delta into the running cleaned text.
    Delta(String),
    /// Final `{"choices":[],"usage":{...}}` chunk — captured for cost
    /// accounting (feature 005).
    Usage(UsageBlock),
    /// Stream terminator — return the accumulated text.
    Done,
    /// Comment, blank line, or non-`data:` field — ignore.
    Skip,
}

fn parse_sse_line(line: &str) -> SseLine {
    let Some(rest) = line.strip_prefix("data:") else {
        return SseLine::Skip;
    };
    let payload = rest.trim_start_matches(' ');
    if payload == "[DONE]" {
        return SseLine::Done;
    }
    if payload.is_empty() {
        return SseLine::Skip;
    }
    // Try the usage shape first — the final chunk under
    // `include_usage=true` ships an empty `choices` array, so the
    // delta-content path returns `None` and the caller would have
    // dropped the row otherwise.
    if let Some(usage) = extract_usage_block(payload) {
        return SseLine::Usage(usage);
    }
    match extract_choice_content(payload) {
        Some(text) if !text.is_empty() => SseLine::Delta(text),
        _ => SseLine::Skip,
    }
}

/// Pull `usage.prompt_tokens` / `usage.completion_tokens` from a JSON
/// payload. Returns `None` when the chunk doesn't carry a usage
/// block — every regular `delta` chunk lands here, so the parser
/// keeps walking.
fn extract_usage_block(payload: &str) -> Option<UsageBlock> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let usage = value.get("usage")?;
    let prompt_tokens = usage.get("prompt_tokens").and_then(|v| v.as_i64())?;
    let completion_tokens = usage.get("completion_tokens").and_then(|v| v.as_i64())?;
    // Groq's automatic prompt-prefix cache surfaces a hit count under
    // `usage.prompt_tokens_details.cached_tokens` (OpenAI-compatible
    // shape). Absent on cold prefixes, older API responses, and
    // non-gpt-oss models — keep optional so we don't drop the whole
    // usage row when the field is missing.
    let cached_tokens = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_i64());
    Some(UsageBlock {
        prompt_tokens,
        completion_tokens,
        cached_tokens,
    })
}

/// Pull `choices[0].delta.content` (streaming) or
/// `choices[0].message.content` (non-streaming) from a JSON payload. Returns
/// `None` for any structural deviation — callers treat that as "skip the
/// line" rather than as a hard failure, mirroring Swift v1.
fn extract_choice_content(payload: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let first = value.get("choices")?.as_array()?.first()?;
    if let Some(delta) = first.get("delta") {
        if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
            return Some(content.to_string());
        }
    }
    if let Some(message) = first.get("message") {
        if let Some(content) = message.get("content").and_then(|v| v.as_str()) {
            return Some(content.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_done_terminator() {
        assert_eq!(parse_sse_line("data: [DONE]"), SseLine::Done);
        assert_eq!(parse_sse_line("data:[DONE]"), SseLine::Done);
    }

    #[test]
    fn parse_sse_extracts_delta_content() {
        let line = r#"data: {"choices":[{"delta":{"content":"Hello"}}]}"#;
        assert_eq!(parse_sse_line(line), SseLine::Delta("Hello".into()));
    }

    #[test]
    fn parse_sse_extracts_message_content_fallback() {
        // Non-streaming response shape — Swift v1 reads this too.
        let line = r#"data: {"choices":[{"message":{"content":"World","role":"assistant"}}]}"#;
        assert_eq!(parse_sse_line(line), SseLine::Delta("World".into()));
    }

    #[test]
    fn parse_sse_ignores_non_data_lines() {
        assert_eq!(parse_sse_line(":heartbeat"), SseLine::Skip);
        assert_eq!(parse_sse_line("event: ping"), SseLine::Skip);
        assert_eq!(parse_sse_line(""), SseLine::Skip);
    }

    #[test]
    fn parse_sse_skips_lines_with_missing_content() {
        // Empty deltas (e.g. role-only chunks) and structurally weird JSON
        // both round-trip as skip rather than blowing up the stream.
        let role_only = r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#;
        assert_eq!(parse_sse_line(role_only), SseLine::Skip);

        let no_choices = r#"data: {"id":"x"}"#;
        assert_eq!(parse_sse_line(no_choices), SseLine::Skip);

        let bad_json = r#"data: not json"#;
        assert_eq!(parse_sse_line(bad_json), SseLine::Skip);
    }

    #[test]
    fn parse_sse_handles_empty_data_payload() {
        assert_eq!(parse_sse_line("data:"), SseLine::Skip);
        assert_eq!(parse_sse_line("data: "), SseLine::Skip);
    }

    #[test]
    fn parse_sse_skips_empty_string_content() {
        let line = r#"data: {"choices":[{"delta":{"content":""}}]}"#;
        assert_eq!(parse_sse_line(line), SseLine::Skip);
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        // 4-byte char "🚀"; truncating in the middle of its bytes must not
        // produce invalid UTF-8.
        let s = "abcd🚀";
        let truncated = truncate_for_log(s, 6);
        assert!(truncated.is_char_boundary(truncated.len()));
        assert!(truncated.starts_with("abcd"));
    }

    #[test]
    fn truncate_passes_through_short_strings() {
        let s = "hello";
        assert_eq!(truncate_for_log(s, 100), s);
    }

    #[test]
    fn extract_choice_content_prefers_delta_over_message() {
        // If both shapes are present (won't happen in practice but the parser
        // shouldn't pick the wrong one), delta wins because it's the streaming
        // shape we expect.
        let payload = r#"{"choices":[{"delta":{"content":"D"},"message":{"content":"M"}}]}"#;
        assert_eq!(extract_choice_content(payload), Some("D".to_string()));
    }

    #[test]
    fn groq_message_helpers_set_role_correctly() {
        let s = GroqMessage::system("sys".into());
        assert_eq!(s.role, "system");
        assert_eq!(s.content, "sys");

        let u = GroqMessage::user("usr".into());
        assert_eq!(u.role, "user");
        assert_eq!(u.content, "usr");
    }

    #[test]
    fn groq_request_cleanup_uses_v1_defaults_with_caller_model() {
        let req = GroqRequest::cleanup(
            vec![GroqMessage::user("hi".into())],
            "llama-3.3-70b-versatile",
        );
        assert_eq!(req.model, "llama-3.3-70b-versatile");
        assert!((req.temperature - DEFAULT_TEMPERATURE).abs() < f32::EPSILON);
        assert!(req.stream);
        assert_eq!(req.messages.len(), 1);
        // Llama is not a reasoning model — `reasoning_effort` must
        // stay absent so Groq doesn't reject the request.
        assert!(req.reasoning_effort.is_none());
    }

    #[test]
    fn groq_request_cleanup_with_effort_passes_through_explicit_value_for_gpt_oss_models() {
        // Use the explicit-effort variant so the test doesn't depend on
        // host env state. Mirrors what production does at boot:
        // resolve_cleanup_reasoning_effort() → cleanup_with_effort(...).
        for model in ["openai/gpt-oss-120b", "openai/gpt-oss-20b"] {
            for effort in ["low", "medium", "high"] {
                let req = GroqRequest::cleanup_with_effort(
                    vec![GroqMessage::user("hi".into())],
                    model,
                    effort,
                );
                assert_eq!(
                    req.reasoning_effort.as_deref(),
                    Some(effort),
                    "gpt-oss model {model} should carry reasoning_effort={effort}"
                );
            }
        }
    }

    #[test]
    fn groq_request_serializes_with_expected_field_names() {
        let req = GroqRequest::cleanup_with_effort(
            vec![GroqMessage::system("a".into())],
            DEFAULT_MODEL,
            DEFAULT_REASONING_EFFORT,
        );
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(&format!("\"model\":\"{DEFAULT_MODEL}\"")));
        assert!(json.contains("\"temperature\":0.2"));
        assert!(json.contains("\"stream\":true"));
        assert!(json.contains("\"messages\""));
        assert!(json.contains("\"role\":\"system\""));
        assert!(json.contains("\"content\":\"a\""));
        // Feature 005 — the cleanup helper opts the request into the
        // final usage chunk so cost tracking can record token counts.
        assert!(
            json.contains("\"stream_options\":{\"include_usage\":true}"),
            "cleanup() should set stream_options.include_usage=true"
        );
        // gpt-oss + DEFAULT_REASONING_EFFORT must be on the wire.
        assert!(
            json.contains(&format!(
                "\"reasoning_effort\":\"{DEFAULT_REASONING_EFFORT}\""
            )),
            "cleanup_with_effort() with gpt-oss should set reasoning_effort={DEFAULT_REASONING_EFFORT}"
        );
        // gpt-oss must carry max_completion_tokens so Groq's tight
        // server default (2048) doesn't strand the reasoning phase
        // without budget for content.
        assert!(
            json.contains(&format!(
                "\"max_completion_tokens\":{DEFAULT_MAX_COMPLETION_TOKENS}"
            )),
            "cleanup_with_effort() with gpt-oss should set max_completion_tokens={DEFAULT_MAX_COMPLETION_TOKENS}"
        );
    }

    #[test]
    fn groq_request_sets_max_completion_tokens_only_for_gpt_oss() {
        // gpt-oss reasoning models need an explicit higher cap to avoid
        // the 2048-default reasoning-exhaustion failure mode. Llama and
        // other non-reasoning models don't have this issue and use the
        // legacy `max_tokens` field, so we leave them on server defaults.
        for model in ["openai/gpt-oss-120b", "openai/gpt-oss-20b"] {
            let req = GroqRequest::cleanup_with_effort(
                vec![GroqMessage::user("hi".into())],
                model,
                DEFAULT_REASONING_EFFORT,
            );
            assert_eq!(
                req.max_completion_tokens,
                Some(DEFAULT_MAX_COMPLETION_TOKENS),
                "gpt-oss model {model} should carry max_completion_tokens"
            );
        }
        let req_llama = GroqRequest::cleanup(
            vec![GroqMessage::user("hi".into())],
            "llama-3.3-70b-versatile",
        );
        assert!(
            req_llama.max_completion_tokens.is_none(),
            "non-reasoning models must not carry max_completion_tokens"
        );
        let json = serde_json::to_string(&req_llama).unwrap();
        assert!(
            !json.contains("max_completion_tokens"),
            "Llama wire body must omit max_completion_tokens entirely"
        );
    }

    #[test]
    fn groq_request_omits_reasoning_effort_for_non_gpt_oss_models() {
        // Llama is not a reasoning model — `reasoning_effort` must stay
        // absent regardless of effort value passed, so Groq doesn't
        // reject the request. Test both env-reading and explicit
        // variants to lock the predicate.
        for effort in ["low", "medium", "high"] {
            let req = GroqRequest::cleanup_with_effort(
                vec![GroqMessage::user("hi".into())],
                "llama-3.3-70b-versatile",
                effort,
            );
            assert!(
                req.reasoning_effort.is_none(),
                "llama must not carry reasoning_effort (effort arg was {effort})"
            );
        }
        let req_default = GroqRequest::cleanup(
            vec![GroqMessage::user("hi".into())],
            "llama-3.3-70b-versatile",
        );
        let json = serde_json::to_string(&req_default).unwrap();
        assert!(
            !json.contains("reasoning_effort"),
            "non-gpt-oss models must not carry reasoning_effort on the wire"
        );
    }

    #[test]
    fn parse_cleanup_reasoning_effort_accepts_allowlist_values() {
        for input in ["low", "medium", "high"] {
            assert_eq!(
                parse_cleanup_reasoning_effort(Some(input.to_string())),
                input,
                "{input} should pass through"
            );
        }
    }

    #[test]
    fn parse_cleanup_reasoning_effort_is_case_and_whitespace_tolerant() {
        for input in ["  Medium  ", "MEDIUM", "Medium\n", "\tmedium"] {
            assert_eq!(
                parse_cleanup_reasoning_effort(Some(input.to_string())),
                "medium",
                "sloppy env value {input:?} should normalise to medium"
            );
        }
    }

    #[test]
    fn parse_cleanup_reasoning_effort_falls_back_on_invalid_or_empty() {
        // Unset env.
        assert_eq!(
            parse_cleanup_reasoning_effort(None),
            DEFAULT_REASONING_EFFORT
        );
        // Empty or whitespace-only env.
        for input in ["", "   ", "\t\n"] {
            assert_eq!(
                parse_cleanup_reasoning_effort(Some(input.to_string())),
                DEFAULT_REASONING_EFFORT,
                "empty/whitespace {input:?} should fall back"
            );
        }
        // Out-of-allowlist values (typos, future-Groq values we haven't
        // verified): fall back rather than passing through and risking
        // a Groq 400.
        for input in ["lo", "med", "extreme", "maximum", "0", "true"] {
            assert_eq!(
                parse_cleanup_reasoning_effort(Some(input.to_string())),
                DEFAULT_REASONING_EFFORT,
                "invalid {input:?} should fall back to default"
            );
        }
    }

    #[test]
    fn is_gpt_oss_reasoning_model_recognises_known_ids() {
        assert!(is_gpt_oss_reasoning_model("openai/gpt-oss-120b"));
        assert!(is_gpt_oss_reasoning_model("openai/gpt-oss-20b"));
        assert!(!is_gpt_oss_reasoning_model("llama-3.3-70b-versatile"));
        assert!(!is_gpt_oss_reasoning_model("llama-3.1-8b-instant"));
        assert!(!is_gpt_oss_reasoning_model(""));
    }

    #[test]
    fn client_cleanup_model_returns_resolved_value() {
        // Explicit-model constructor lets the test pin a value without
        // depending on the host's env state.
        let client = GroqClient::with_endpoint_and_model(
            "http://127.0.0.1:1".to_string(),
            "custom-model".to_string(),
        )
        .unwrap();
        assert_eq!(client.cleanup_model(), "custom-model");
    }

    #[test]
    fn parse_sse_extracts_usage_block_with_empty_choices() {
        // The final chunk under stream_options.include_usage=true ships
        // empty choices and a usage block — feature 005 cost path.
        let line = r#"data: {"choices":[],"usage":{"prompt_tokens":42,"completion_tokens":17,"total_tokens":59}}"#;
        match parse_sse_line(line) {
            SseLine::Usage(block) => {
                assert_eq!(block.prompt_tokens, 42);
                assert_eq!(block.completion_tokens, 17);
                // Older shape without prompt_tokens_details — cache
                // field stays None rather than defaulting to zero,
                // which would otherwise be indistinguishable from a
                // confirmed-zero cache hit.
                assert_eq!(block.cached_tokens, None);
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_extracts_cached_tokens_from_prompt_tokens_details() {
        // Groq's prompt-cache hit shape: `usage.prompt_tokens_details.cached_tokens`
        // sits under prompt_tokens (subset, not additive).
        let line = r#"data: {"choices":[],"usage":{"prompt_tokens":4096,"completion_tokens":12,"total_tokens":4108,"prompt_tokens_details":{"cached_tokens":3840}}}"#;
        match parse_sse_line(line) {
            SseLine::Usage(block) => {
                assert_eq!(block.prompt_tokens, 4096);
                assert_eq!(block.completion_tokens, 12);
                assert_eq!(block.cached_tokens, Some(3840));
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_treats_missing_cached_tokens_inside_details_as_none() {
        // Defensive: server may emit an empty `prompt_tokens_details`
        // object on a cold prefix. Don't synthesize a zero.
        let line = r#"data: {"choices":[],"usage":{"prompt_tokens":4096,"completion_tokens":12,"prompt_tokens_details":{}}}"#;
        match parse_sse_line(line) {
            SseLine::Usage(block) => {
                assert_eq!(block.cached_tokens, None);
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_falls_through_when_usage_missing_one_field() {
        // Defensive: a malformed usage block must not be treated as
        // valid — the parser falls through to the regular delta path
        // (which returns Skip for empty choices).
        let line = r#"data: {"choices":[],"usage":{"prompt_tokens":42}}"#;
        assert_eq!(parse_sse_line(line), SseLine::Skip);
    }

    #[tokio::test]
    async fn complete_with_empty_api_key_returns_missing_error() {
        let client = GroqClient::with_endpoint_and_model(
            "http://127.0.0.1:1".to_string(),
            DEFAULT_MODEL.to_string(),
        )
        .unwrap();
        let req = GroqRequest::cleanup(vec![GroqMessage::user("hi".into())], DEFAULT_MODEL);
        match client.complete(req, "").await {
            Err(MuniError::GroqMissingApiKey) => {}
            other => panic!("expected GroqMissingApiKey, got {other:?}"),
        }
    }

    // ───────────────────────── long-profile resolvers ─────────────────────────

    #[test]
    fn parse_cleanup_long_model_returns_default_when_unset_or_blank() {
        assert_eq!(parse_cleanup_long_model(None), DEFAULT_LONG_MODEL);
        for input in ["", "   ", "\t\n"] {
            assert_eq!(
                parse_cleanup_long_model(Some(input.to_string())),
                DEFAULT_LONG_MODEL,
                "empty/whitespace {input:?} should fall back"
            );
        }
    }

    #[test]
    fn parse_cleanup_long_model_honours_override_and_trims_whitespace() {
        assert_eq!(
            parse_cleanup_long_model(Some("openai/gpt-oss-20b".to_string())),
            "openai/gpt-oss-20b"
        );
        // Trimming matches the short-profile parser shape — sloppy `.env`
        // entries shouldn't 400 Groq.
        assert_eq!(
            parse_cleanup_long_model(Some("  llama-3.3-70b-versatile  ".to_string())),
            "llama-3.3-70b-versatile"
        );
    }

    #[test]
    fn parse_cleanup_long_reasoning_effort_accepts_allowlist_values() {
        for input in ["low", "medium", "high"] {
            assert_eq!(
                parse_cleanup_long_reasoning_effort(Some(input.to_string())),
                input,
                "{input} should pass through"
            );
        }
    }

    #[test]
    fn parse_cleanup_long_reasoning_effort_is_case_and_whitespace_tolerant() {
        for input in ["  Medium  ", "MEDIUM", "Medium\n", "\tmedium"] {
            assert_eq!(
                parse_cleanup_long_reasoning_effort(Some(input.to_string())),
                "medium",
                "sloppy env value {input:?} should normalise to medium"
            );
        }
    }

    #[test]
    fn parse_cleanup_long_reasoning_effort_falls_back_on_invalid_or_empty() {
        assert_eq!(
            parse_cleanup_long_reasoning_effort(None),
            DEFAULT_LONG_REASONING_EFFORT
        );
        for input in ["", "   ", "\t\n", "lo", "extreme", "0"] {
            assert_eq!(
                parse_cleanup_long_reasoning_effort(Some(input.to_string())),
                DEFAULT_LONG_REASONING_EFFORT,
                "invalid {input:?} should fall back"
            );
        }
    }

    #[test]
    fn parse_cleanup_long_press_threshold_s_returns_default_when_unset() {
        assert!(
            (parse_cleanup_long_press_threshold_s(None) - DEFAULT_LONG_PRESS_THRESHOLD_S).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn parse_cleanup_long_press_threshold_s_honours_positive_numeric_override() {
        for (input, expected) in [
            ("4.5", 4.5),
            ("  7  ", 7.0),
            ("10", 10.0),
            ("0.5", 0.5),
            ("999", 999.0),
        ] {
            assert!(
                (parse_cleanup_long_press_threshold_s(Some(input.to_string())) - expected).abs()
                    < f64::EPSILON,
                "{input:?} should parse to {expected}"
            );
        }
    }

    #[test]
    fn parse_cleanup_long_press_threshold_s_rejects_non_positive_and_non_numeric() {
        // Non-positive (≤ 0): rejected to avoid the foot-gun of
        // routing every press to long via "0" — that's better expressed
        // by setting LONG_MODEL to match SHORT_MODEL.
        for input in ["0", "0.0", "-1", "-5.5"] {
            assert!(
                (parse_cleanup_long_press_threshold_s(Some(input.to_string()))
                    - DEFAULT_LONG_PRESS_THRESHOLD_S)
                    .abs()
                    < f64::EPSILON,
                "non-positive {input:?} should fall back"
            );
        }
        // Non-numeric: typos, units, blank.
        for input in ["", "   ", "five", "5s", "abc", "1e"] {
            assert!(
                (parse_cleanup_long_press_threshold_s(Some(input.to_string()))
                    - DEFAULT_LONG_PRESS_THRESHOLD_S)
                    .abs()
                    < f64::EPSILON,
                "non-numeric {input:?} should fall back"
            );
        }
    }

    // ───────────────────────── routing decision ─────────────────────────

    fn synthetic_short() -> (String, String) {
        ("short-model".to_string(), "low".to_string())
    }

    fn synthetic_long() -> (String, String) {
        ("long-model".to_string(), "high".to_string())
    }

    #[test]
    fn pick_cleanup_config_at_threshold_routes_to_short() {
        // The boundary belongs to short — `duration > threshold` routes
        // to long, so exact-equal stays on short.
        let chosen =
            pick_cleanup_config_for_duration(5.0, 5.0, synthetic_short(), synthetic_long());
        assert_eq!(chosen, synthetic_short());
    }

    #[test]
    fn pick_cleanup_config_just_below_threshold_routes_to_short() {
        let chosen =
            pick_cleanup_config_for_duration(4.999, 5.0, synthetic_short(), synthetic_long());
        assert_eq!(chosen, synthetic_short());
    }

    #[test]
    fn pick_cleanup_config_just_above_threshold_routes_to_long() {
        let chosen =
            pick_cleanup_config_for_duration(5.001, 5.0, synthetic_short(), synthetic_long());
        assert_eq!(chosen, synthetic_long());
    }

    #[test]
    fn pick_cleanup_config_zero_duration_routes_to_short() {
        // A zero-length press (denied silent press, etc.) must still
        // route somewhere safely — short is the conservative pick.
        let chosen =
            pick_cleanup_config_for_duration(0.0, 5.0, synthetic_short(), synthetic_long());
        assert_eq!(chosen, synthetic_short());
    }

    #[test]
    fn pick_cleanup_config_very_large_duration_routes_to_long() {
        // No practical upper bound — a 1-hour press routes to long.
        let chosen =
            pick_cleanup_config_for_duration(3600.0, 5.0, synthetic_short(), synthetic_long());
        assert_eq!(chosen, synthetic_long());
    }

    // ─── `cleanup_for_duration` constructor serializes the chosen profile ───

    #[test]
    fn groq_request_cleanup_for_duration_routes_short_press_to_short_profile() {
        // A press well below the default threshold (5 s) must select
        // the short profile. With no `.env` overrides set in this test
        // environment, that's `DEFAULT_MODEL` + `DEFAULT_REASONING_EFFORT`.
        let req = GroqRequest::cleanup_for_duration(vec![GroqMessage::user("hi".into())], 1.0);
        assert_eq!(req.model, DEFAULT_MODEL);
        assert_eq!(
            req.reasoning_effort.as_deref(),
            Some(DEFAULT_REASONING_EFFORT)
        );
        // Reasoning-model gate still applies — max_completion_tokens
        // must be set since the short default is a gpt-oss model.
        assert_eq!(
            req.max_completion_tokens,
            Some(DEFAULT_MAX_COMPLETION_TOKENS)
        );
        // Streaming + usage-include knobs unchanged.
        assert!(req.stream);
        assert!(req.stream_options.as_ref().is_some_and(|o| o.include_usage));
        assert!((req.temperature - DEFAULT_TEMPERATURE).abs() < f32::EPSILON);
    }

    #[test]
    fn groq_request_cleanup_for_duration_routes_long_press_to_long_profile() {
        // A press well above the default threshold (5 s) must select
        // the long profile. With no `.env` overrides, that's
        // `DEFAULT_LONG_MODEL` + `DEFAULT_LONG_REASONING_EFFORT`.
        let req = GroqRequest::cleanup_for_duration(vec![GroqMessage::user("hi".into())], 30.0);
        assert_eq!(req.model, DEFAULT_LONG_MODEL);
        assert_eq!(
            req.reasoning_effort.as_deref(),
            Some(DEFAULT_LONG_REASONING_EFFORT)
        );
        assert_eq!(
            req.max_completion_tokens,
            Some(DEFAULT_MAX_COMPLETION_TOKENS)
        );
        assert!(req.stream);
        assert!(req.stream_options.as_ref().is_some_and(|o| o.include_usage));
    }

    // ─────────────────────── cleanup-timeout parsers ───────────────────────

    #[test]
    fn parse_cleanup_timeout_ms_returns_default_when_unset() {
        assert_eq!(
            parse_cleanup_timeout_ms(None),
            Duration::from_millis(DEFAULT_CLEANUP_TIMEOUT_MS)
        );
    }

    #[test]
    fn parse_cleanup_timeout_ms_honours_positive_integer_override() {
        for (input, expected_ms) in [
            ("3000", 3_000_u64),
            ("  4500  ", 4_500),
            ("1", 1),
            ("60000", 60_000),
        ] {
            assert_eq!(
                parse_cleanup_timeout_ms(Some(input.to_string())),
                Duration::from_millis(expected_ms),
                "{input:?} should parse to {expected_ms} ms"
            );
        }
    }

    #[test]
    fn parse_cleanup_timeout_ms_rejects_zero_negative_and_non_numeric() {
        // `0` is rejected for the same reason `parse_cleanup_long_press_threshold_s`
        // rejects ≤ 0: a zero timeout fires instantly and is almost
        // certainly a typo. Negatives don't parse as u64 — they fall
        // through the parse and get the same default treatment.
        for input in ["0", "-100", "", "   ", "fast", "5s", "1.5", "1e3"] {
            assert_eq!(
                parse_cleanup_timeout_ms(Some(input.to_string())),
                Duration::from_millis(DEFAULT_CLEANUP_TIMEOUT_MS),
                "{input:?} should fall back to default"
            );
        }
    }

    #[test]
    fn parse_cleanup_long_timeout_ms_returns_default_when_unset() {
        assert_eq!(
            parse_cleanup_long_timeout_ms(None),
            Duration::from_millis(DEFAULT_CLEANUP_LONG_TIMEOUT_MS)
        );
    }

    #[test]
    fn parse_cleanup_long_timeout_ms_honours_positive_integer_override() {
        assert_eq!(
            parse_cleanup_long_timeout_ms(Some("6000".into())),
            Duration::from_millis(6_000)
        );
        assert_eq!(
            parse_cleanup_long_timeout_ms(Some("  2500  ".into())),
            Duration::from_millis(2_500)
        );
    }

    #[test]
    fn parse_cleanup_long_timeout_ms_rejects_zero_and_non_numeric() {
        for input in ["0", "-1", "", "  ", "soon", "4.5"] {
            assert_eq!(
                parse_cleanup_long_timeout_ms(Some(input.to_string())),
                Duration::from_millis(DEFAULT_CLEANUP_LONG_TIMEOUT_MS),
                "{input:?} should fall back to long default"
            );
        }
    }

    // ── cleanup-retry constants ──────────────────────────────────────────
    //
    // Pinned so a careless rename of `DEFAULT_LONG_MODEL` (or someone
    // flipping the long profile to a non-gpt-oss-120b model via env) does
    // not silently change what the retry runs. The retry's contract is
    // "always 120b/low" regardless of the long-profile env state — these
    // assertions are the guardrail.

    #[test]
    fn cleanup_retry_constants_are_pinned_to_120b_low() {
        assert_eq!(CLEANUP_RETRY_MODEL, "openai/gpt-oss-120b");
        assert_eq!(CLEANUP_RETRY_EFFORT, "low");
        // The retry model must remain a gpt-oss reasoning model so the
        // `reasoning_effort` knob actually has an effect.
        assert!(is_gpt_oss_reasoning_model(CLEANUP_RETRY_MODEL));
    }

    // ───────────────────────── endpoint override ─────────────────────────

    #[test]
    fn parse_groq_endpoint_returns_default_when_unset() {
        assert_eq!(parse_groq_endpoint(None), DEFAULT_ENDPOINT);
    }

    #[test]
    fn parse_groq_endpoint_honours_override_and_trims_whitespace() {
        assert_eq!(
            parse_groq_endpoint(Some(
                "http://127.0.0.1:8787/openai/v1/chat/completions".into()
            )),
            "http://127.0.0.1:8787/openai/v1/chat/completions"
        );
        assert_eq!(
            parse_groq_endpoint(Some(
                "  http://127.0.0.1:8787/openai/v1/chat/completions  ".into()
            )),
            "http://127.0.0.1:8787/openai/v1/chat/completions"
        );
    }

    #[test]
    fn parse_groq_endpoint_treats_empty_or_whitespace_as_unset() {
        for input in ["", "   ", "\t", "\n"] {
            assert_eq!(
                parse_groq_endpoint(Some(input.to_string())),
                DEFAULT_ENDPOINT,
                "{input:?} should fall back to default"
            );
        }
    }
}
