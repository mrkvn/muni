//! Gemini text language-ID client (feature 003).
//!
//! Implements [`crate::text_lid::TextLidClassifier`] against Google's
//! `generateContent` REST endpoint. The Whisper transcript is fed in
//! as a single user-turn prompt; Gemini replies with one of
//! `english` / `tagalog` / `taglish` (or, rarely, something else,
//! which `parse_classification` maps to `LidLabel::Other`).
//!
//! ## Why this layer is intentionally thin
//!
//! Feature 003 is an experiment. Gemini is "cheap, fast, decent at
//! Filipino code-switching" today, but the LID provider is expected
//! to change as we learn — local Gemma 3 (no network), a different
//! cloud LLM, or even a small bundled classifier are all candidates.
//! Everything Gemini-specific (auth header shape, request body,
//! endpoint, model name) lives here; the trait abstraction in
//! [`crate::text_lid`] keeps the callers (session.rs, validator IPC)
//! from caring which provider is wired in.
//!
//! Wire contract:
//! - URL: `https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent`.
//! - Method: `POST`, `application/json`.
//! - Auth: `x-goog-api-key: <KEY>` (NOT `Authorization: Bearer …` —
//!   Google uses a custom header for AI Studio keys).
//! - Body: see [`build_request_body`].
//! - Response: `{"candidates":[{"content":{"parts":[{"text":"english"}]}}]}`.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::MuniError;
use crate::secrets;
use crate::text_lid::{
    parse_classification, render_prompt, LidLabel, LidTokenUsage, TextLidClassifier,
};

/// Production endpoint base — model name is appended at request time
/// so the same client can target alternate Gemini models without
/// rebuilding the struct.
pub const DEFAULT_ENDPOINT_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// Default Gemini model id.
///
/// Originally `gemini-3.1-flash-lite-preview` per feature 003's
/// initial plan. Empirical QA in the same iteration showed the
/// preview tier hits HTTP 503 (`"experiencing high demand"`) under
/// modest real-world load, plus full-budget timeouts on slow days.
/// `gemini-2.5-flash-lite` (GA, 100% correct in side-by-side QA
/// against Groq llama-3.1-8b-instant on the long-English regression
/// suite) is the production-stable replacement. It is also slightly
/// cheaper per token ($0.10/$0.40 vs $0.25/$1.50 per 1M).
pub const DEFAULT_MODEL: &str = "gemini-2.5-flash-lite";

/// LID is not allowed to hold up the press indefinitely.
///
/// Empirical (manual QA, feature 003 first run): 50% of Gemini
/// classify calls hit the 3 s ceiling on a typical residential
/// connection — the network or Google's edge is sometimes that slow.
/// 6 s catches those without changing the press's effective latency
/// (the orchestrator aborts the LID task at release + 500 ms anyway,
/// so a slow Gemini call only blocks the LID *task*, not the press
/// pipeline).
pub const LID_TIMEOUT: Duration = Duration::from_secs(6);

/// Cap log lines so a verbose HTML error page (e.g. Google's
/// quota-exceeded responses) doesn't flood the logs.
const MAX_ERROR_BODY_BYTES: usize = 4_096;

/// Gemini text-LID client.
///
/// Owns a `reqwest::Client` so TLS sessions are reused across
/// presses — opening a fresh client every press would add ~200 ms of
/// handshake on top of the model's own latency.
#[derive(Debug, Clone)]
pub struct GeminiLidClient {
    endpoint_base: String,
    model: String,
    label: String,
    http: reqwest::Client,
    timeout: Duration,
}

impl GeminiLidClient {
    /// Construct a client targeting the production Gemini endpoint
    /// with the default model.
    pub fn new() -> Result<Self, MuniError> {
        Self::with_model(DEFAULT_MODEL.to_string())
    }

    /// Construct a client targeting the production endpoint with a
    /// specific Gemini model. Used by the env-driven provider
    /// builder so users can A/B different models (`gemini-2.5-flash`,
    /// `gemini-2.5-flash-lite`, etc.) without rebuilding.
    pub fn with_model(model: String) -> Result<Self, MuniError> {
        Self::with_endpoint(DEFAULT_ENDPOINT_BASE.to_string(), model)
    }

    /// Construct a client targeting a custom endpoint and/or model.
    /// Used by tests to point at a `wiremock` server.
    pub fn with_endpoint(endpoint_base: String, model: String) -> Result<Self, MuniError> {
        let http =
            reqwest::Client::builder()
                .build()
                .map_err(|e| MuniError::GeminiConnectionFailed {
                    reason: e.to_string(),
                })?;
        let label = format!("gemini:{model}");
        Ok(Self {
            endpoint_base,
            model,
            label,
            http,
            timeout: LID_TIMEOUT,
        })
    }

    /// Classify `text` using an explicit `api_key` rather than the
    /// keychain.
    ///
    /// Used by:
    /// - The trait `classify` impl, after resolving the live key via
    ///   `secrets::get(GEMINI_ACCOUNT)`.
    /// - The `validate_api_key` IPC handler, which has the user's
    ///   freshly-typed key in hand and needs to test it without
    ///   persisting first.
    pub async fn classify_with_key(
        &self,
        text: &str,
        api_key: &str,
    ) -> Result<(LidLabel, Option<LidTokenUsage>), MuniError> {
        if api_key.is_empty() {
            return Err(MuniError::GeminiMissingApiKey);
        }

        let url = format!("{}/{}:generateContent", self.endpoint_base, self.model);
        let body = build_request_body(text);

        let response = self
            .http
            .post(&url)
            .timeout(self.timeout)
            .header("x-goog-api-key", api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| MuniError::GeminiConnectionFailed {
                reason: e.to_string(),
            })?;

        let status = response.status();
        if !status.is_success() {
            let raw = response
                .text()
                .await
                .unwrap_or_else(|e| format!("(failed to read error body: {e})"));
            let truncated = truncate_for_log(&raw, MAX_ERROR_BODY_BYTES);
            log::warn!(
                target: "lid",
                "gemini non-2xx HTTP {}: {}",
                status.as_u16(),
                truncated
            );
            // Feature 035: Gemini 401/403 is intentionally NOT split into a
            // `GeminiKeyRejected` kind. Gemini drives LID only, which silently
            // self-falls-back (audio-LID / default language) when it fails, so
            // a rejected Gemini key never reaches a user-facing surface — it
            // stays Silent-tier (`GeminiServerError`, log-only). Unlike the ASR
            // providers (Deepgram/Gladia/Groq), there is no actionable prompt
            // to show. Re-split here only if a user-facing Gemini surface is
            // ever added.
            return Err(MuniError::GeminiServerError {
                status: status.as_u16(),
                body: truncated,
            });
        }

        let raw = response
            .text()
            .await
            .map_err(|e| MuniError::GeminiConnectionFailed {
                reason: e.to_string(),
            })?;

        let parsed: GenerateContentResponse = serde_json::from_str(&raw).map_err(|err| {
            log::warn!(
                target: "lid",
                "gemini response parse failed: {err}; body={}",
                truncate_for_log(&raw, MAX_ERROR_BODY_BYTES)
            );
            MuniError::GeminiInvalidResponse
        })?;

        let answer = parsed.first_text().ok_or_else(|| {
            log::warn!(
                target: "lid",
                "gemini response missing candidates[0].content.parts[0].text; body={}",
                truncate_for_log(&raw, MAX_ERROR_BODY_BYTES)
            );
            MuniError::GeminiInvalidResponse
        })?;

        let usage = parsed.usage_metadata.as_ref().and_then(|u| {
            match (u.prompt_token_count, u.candidates_token_count) {
                (Some(input_tokens), Some(output_tokens)) => Some(LidTokenUsage {
                    input_tokens,
                    output_tokens,
                }),
                _ => None,
            }
        });
        Ok((parse_classification(&answer), usage))
    }
}

#[async_trait]
impl TextLidClassifier for GeminiLidClient {
    async fn classify(&self, text: &str) -> Result<(LidLabel, Option<LidTokenUsage>), MuniError> {
        let api_key = secrets::get(secrets::GEMINI_ACCOUNT)?;
        self.classify_with_key(text, &api_key).await
    }

    fn provider_label(&self) -> &str {
        &self.label
    }
}

/// Build the JSON body for a single-turn `generateContent` request.
///
/// Pulled out as a free function so unit tests can assert exact-shape
/// without a network. `temperature: 0.0` keeps the response
/// deterministic; `maxOutputTokens: 16` is enough headroom that
/// `english` / `tagalog` / `taglish` plus any leading whitespace,
/// BOS, or trailing newline tokens all fit. (Empirical, feature 003
/// QA: the previous cap of 5 truncated `tagalog` to `tag` because
/// Gemini Flash-Lite's tokenizer splits the word and prefixes
/// whitespace.)
pub fn build_request_body(text: &str) -> Value {
    let prompt = render_prompt(text);
    json!({
        "contents": [{
            "parts": [{ "text": prompt }]
        }],
        "generationConfig": {
            "temperature": 0.0,
            "maxOutputTokens": 16,
            "topP": 1.0
        }
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentResponse {
    candidates: Option<Vec<Candidate>>,
    /// Optional Google-shape usage block; nullable so an early-error
    /// response without the field still parses.
    usage_metadata: Option<UsageMetadata>,
}

/// Subset of Gemini's `usageMetadata` that we care about. JSON is
/// camelCase (Google convention), so the parent struct above carries
/// `#[serde(rename_all = "camelCase")]` and the field names here
/// match the camelCase wire shape directly.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageMetadata {
    prompt_token_count: Option<i64>,
    candidates_token_count: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    content: Option<CandidateContent>,
}

#[derive(Debug, Deserialize)]
struct CandidateContent {
    parts: Option<Vec<ContentPart>>,
}

#[derive(Debug, Deserialize)]
struct ContentPart {
    text: Option<String>,
}

impl GenerateContentResponse {
    fn first_text(&self) -> Option<String> {
        self.candidates
            .as_ref()?
            .first()?
            .content
            .as_ref()?
            .parts
            .as_ref()?
            .iter()
            .find_map(|p| p.text.clone())
    }
}

/// Truncate a borrowed string to at most `max_bytes` without splitting
/// a UTF-8 codepoint. Mirrors `groq_whisper.rs::truncate_for_log` —
/// duplicated rather than extracted because the two callers are
/// independent and a shared util would have to live in `error.rs`,
/// which doesn't currently expose helpers.
fn truncate_for_log(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn classify_with_empty_key_returns_missing_error() {
        let client =
            GeminiLidClient::with_endpoint("http://127.0.0.1:1".into(), DEFAULT_MODEL.to_string())
                .unwrap();
        match client.classify_with_key("hello", "").await {
            Err(MuniError::GeminiMissingApiKey) => {}
            other => panic!("expected GeminiMissingApiKey, got {other:?}"),
        }
    }

    #[test]
    fn build_request_body_uses_zero_temperature_and_bounded_output() {
        let body = build_request_body("hello world");
        assert_eq!(
            body["generationConfig"]["temperature"],
            json!(0.0),
            "temperature must be 0 for deterministic output"
        );
        // 16 tokens is enough for `english` / `tagalog` / `taglish`
        // plus any leading whitespace + BOS + trailing newline that
        // Gemini's tokenizer slips in front of the answer. The
        // earlier cap of 5 truncated `tagalog` to `tag` in QA.
        assert_eq!(
            body["generationConfig"]["maxOutputTokens"],
            json!(16),
            "output cap must be high enough to fit the longest target word + framing tokens"
        );
    }

    #[test]
    fn build_request_body_carries_rendered_prompt() {
        let body = build_request_body("Hello there.");
        let text = body["contents"][0]["parts"][0]["text"]
            .as_str()
            .expect("text field");
        assert!(text.contains("Hello there."));
        assert!(text.contains("english, tagalog, or taglish"));
    }

    #[test]
    fn first_text_walks_nested_response_shape() {
        let raw = r#"{
            "candidates": [{
                "content": {
                    "parts": [{ "text": "taglish" }]
                }
            }]
        }"#;
        let parsed: GenerateContentResponse = serde_json::from_str(raw).expect("parse");
        assert_eq!(parsed.first_text(), Some("taglish".to_string()));
    }

    #[test]
    fn first_text_returns_none_when_candidates_missing() {
        let raw = r#"{"candidates": []}"#;
        let parsed: GenerateContentResponse = serde_json::from_str(raw).expect("parse");
        assert!(parsed.first_text().is_none());
    }

    #[test]
    fn parses_usage_metadata_when_present() {
        // Google's `usageMetadata` block — feature 005 cost tracking
        // surfaces these so the LID call lands an `api_calls` row
        // with token counts.
        let raw = r#"{
            "candidates": [{
                "content": { "parts": [{ "text": "english" }] }
            }],
            "usageMetadata": {
                "promptTokenCount": 25,
                "candidatesTokenCount": 1,
                "totalTokenCount": 26
            }
        }"#;
        let parsed: GenerateContentResponse = serde_json::from_str(raw).expect("parse");
        let usage = parsed.usage_metadata.expect("usage_metadata");
        assert_eq!(usage.prompt_token_count, Some(25));
        assert_eq!(usage.candidates_token_count, Some(1));
    }
}
