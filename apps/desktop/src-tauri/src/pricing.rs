//! Provider pricing constants + pure cost computation (feature 005).
//!
//! All numbers in USD. Insert-time cost is computed by [`compute_cost`]
//! against a price row resolved for the call's UTC calendar month —
//! historical months never repaint when prices change because each
//! `api_calls` row freezes its `cost_usd` at insert time.
//!
//! [`DEFAULT_PRICES`] is the bootstrap rate sheet baked into the binary
//! so the very first dictation on a fresh install can compute a cost
//! without waiting for the first `prices_client` fetch. The refresher
//! supersedes these on success; failures leave the seed rates in place.

use serde::{Deserialize, Serialize};

/// Provider identifier slug. String-typed at the schema layer so a new
/// provider doesn't need a Rust enum bump on day one; matches the
/// `provider TEXT` column in `api_calls` and `price_history`.
pub const PROVIDER_DEEPGRAM: &str = "deepgram";
pub const PROVIDER_GROQ: &str = "groq";
pub const PROVIDER_GEMINI: &str = "gemini";
pub const PROVIDER_GLADIA: &str = "gladia";
/// Feature 020 — local whisper.cpp audio-LID. No per-press cost; the
/// provider slug exists so cost-dashboard queries can segment audio-LID
/// usage rows away from the (paid) Groq/Gemini text-LID rows.
pub const PROVIDER_WHISPER_LOCAL: &str = "whisper_local";

/// Providers that run on-device and never incur a billable cost. The
/// user-facing Cost & Usage rollup omits them — they would only ever
/// render "$— (N calls)" and dilute the paid-provider totals. Add an
/// on-device backend's slug here when it starts logging usage rows
/// (e.g. a local Parakeet ASR backend). Dev per-model views still list
/// these so local usage stays auditable.
pub const LOCAL_PROVIDERS: &[&str] = &[PROVIDER_WHISPER_LOCAL];

/// Whether `provider` runs on-device and is therefore free. See
/// [`LOCAL_PROVIDERS`].
pub fn is_local_provider(provider: &str) -> bool {
    LOCAL_PROVIDERS.contains(&provider)
}

/// Call-kind slugs. Match the `call_kind TEXT` column.
pub const CALL_KIND_ASR: &str = "asr";
pub const CALL_KIND_CLEANUP: &str = "cleanup";
pub const CALL_KIND_LID: &str = "lid";
/// Feature 016 — fire-and-forget cleanup warm-up call fired at boot
/// and on prompt/About Me/Vocabulary/Groq-key save. Distinct from
/// `CALL_KIND_CLEANUP` so per-press dashboards stay clean and the
/// warm-up's diagnostic signal (latency, prefix-cache hit/miss) is
/// queryable on its own. See `groq_warmup` for the spawn surface.
pub const CALL_KIND_CLEANUP_WARMUP: &str = "cleanup_warmup";

/// How a price entry is billed. `PerAudioSecond` rows must populate
/// `usd_per_second`; `PerToken` rows must populate at least one of
/// `usd_per_input_token` / `usd_per_output_token`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceKind {
    PerAudioSecond,
    PerToken,
}

impl PriceKind {
    /// Stable wire string used in JSON / SQLite storage.
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::PerAudioSecond => "per_audio_second",
            Self::PerToken => "per_token",
        }
    }

    /// Parse the wire form back into the typed kind. Unknown strings
    /// are rejected so an unrecognised price row from a future scraper
    /// schema doesn't silently become a `PerAudioSecond` and miscompute.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "per_audio_second" => Some(Self::PerAudioSecond),
            "per_token" => Some(Self::PerToken),
            _ => None,
        }
    }
}

/// One bootstrap price row baked into the binary.
///
/// Owns `&'static str` for provider/model/source so the slice is a real
/// const that the compiler can stash in `.rodata` — no allocation on
/// first-run seed. The refresher converts to owned strings before
/// upserting into `price_history`.
#[derive(Debug, Clone, Copy)]
pub struct PriceEntry {
    pub provider: &'static str,
    pub model: &'static str,
    pub kind: PriceKind,
    pub usd_per_second: Option<f64>,
    pub usd_per_input_token: Option<f64>,
    pub usd_per_output_token: Option<f64>,
    pub source_url: &'static str,
}

/// Verified 2026-05-06 — see `.claude/brainstorm/000_api_cost_tracking.md`
/// "DEFAULT_PRICES baked into the binary" table for the source URLs
/// and per-unit derivation.
///
/// Kept as a `&[PriceEntry]` rather than a HashMap so the const lives
/// in `.rodata` and seed-on-first-run is a tight loop. Lookups in the
/// hot path go through `price_history` (SQLite-indexed by
/// `(effective_month, provider, model)`), not this slice.
pub const DEFAULT_PRICES: &[PriceEntry] = &[
    // All-in Nova-3 *streaming* rate, not the headline base rate.
    // Deepgram bills our usage as three stacked per-audio-second line
    // items (observed on the Billing → Spend dashboard, group-by "type
    // of charge"): Nova-3 Stream ($0.29/hr ≈ 73%), Keyterm Prompting
    // ($0.08/hr ≈ 21%) and Base Stream (≈ 6%). We always send keyterms
    // (see `deepgram::DEFAULT_KEYTERMS`), so all three apply to every
    // second — but `PriceKind::PerAudioSecond` carries a single rate, so
    // we fold them into one effective rate: 0.29/hr ÷ 0.73 ≈ $0.397/hr =
    // $0.000_110/audio-second. Modelling only the $0.29/hr base
    // under-counted the real Deepgram bill by ~37%. NOTE: release builds
    // override this from the hosted `MUNI_PRICES_URL` scraper — for the
    // fix to stick in production the scraper must return the all-in rate
    // (or a keyterm line); this constant only governs dev + first-run.
    PriceEntry {
        provider: PROVIDER_DEEPGRAM,
        model: "nova-3",
        kind: PriceKind::PerAudioSecond,
        usd_per_second: Some(0.000_110),
        usd_per_input_token: None,
        usd_per_output_token: None,
        source_url: "https://deepgram.com/pricing",
    },
    PriceEntry {
        provider: PROVIDER_GROQ,
        model: "whisper-large-v3-turbo",
        kind: PriceKind::PerAudioSecond,
        usd_per_second: Some(0.000_011_1),
        usd_per_input_token: None,
        usd_per_output_token: None,
        source_url: "https://groq.com/pricing",
    },
    // Verified 2026-05-12 against https://groq.com/pricing —
    // $0.15/M input, $0.60/M output. Used by the text-LID classifier
    // (post-Round 2 dogfood: gpt-oss-120b is the new LID default per
    // its 100% accuracy on the 20-phrase rigor set vs llama-8b's
    // 89%; speed delta absorbed by parallel ASR).
    PriceEntry {
        provider: PROVIDER_GROQ,
        model: "openai/gpt-oss-120b",
        kind: PriceKind::PerToken,
        usd_per_second: None,
        usd_per_input_token: Some(0.000_000_15),
        usd_per_output_token: Some(0.000_000_60),
        source_url: "https://groq.com/pricing",
    },
    // Verified 2026-05-13 against https://groq.com/pricing —
    // $0.075/M input, $0.30/M output. Also 1000 t/s output vs
    // gpt-oss-120b's 500 t/s, which is the reason for switching
    // the cleanup-pass default from 120b → 20b (cleanup latency
    // was the dominant variable in release→paste time).
    PriceEntry {
        provider: PROVIDER_GROQ,
        model: "openai/gpt-oss-20b",
        kind: PriceKind::PerToken,
        usd_per_second: None,
        usd_per_input_token: Some(0.000_000_075),
        usd_per_output_token: Some(0.000_000_30),
        source_url: "https://groq.com/pricing",
    },
    PriceEntry {
        provider: PROVIDER_GEMINI,
        model: "gemini-2.5-flash-lite",
        kind: PriceKind::PerToken,
        usd_per_second: None,
        usd_per_input_token: Some(0.000_000_1),
        usd_per_output_token: Some(0.000_000_4),
        source_url: "https://ai.google.dev/pricing",
    },
    // Feature 010 — Gladia Solaria-1 real-time STT at $0.75/audio-hour
    // (Starter pay-as-you-go). $0.75/3600 ≈ $0.000_208_3 per audio
    // second. The recurring 10 audio-hour/month free tier is NOT
    // modelled here; `compute_cost` returns gross, and the user reads
    // the bake-off findings doc for the net interpretation.
    PriceEntry {
        provider: PROVIDER_GLADIA,
        model: "solaria-1",
        kind: PriceKind::PerAudioSecond,
        usd_per_second: Some(0.000_208_3),
        usd_per_input_token: None,
        usd_per_output_token: None,
        source_url: "https://www.gladia.io/pricing",
    },
];

/// Compute USD cost for one call.
///
/// `kind` selects the billing model:
/// - `PriceKind::PerAudioSecond` requires `audio_seconds.is_some()` and
///   `usd_per_second.is_some()`; output is the product.
/// - `PriceKind::PerToken` produces `(in_tokens × usd_per_input_token) +
///   (out_tokens × usd_per_output_token)`. Either side may be missing
///   (a request that returns no completion still has prompt tokens) —
///   we sum whatever we have. Returns `None` only when *both* sides are
///   missing or when both rate fields are missing.
///
/// `None` propagates to a NULL `cost_usd` column. Raw usage units are
/// always recorded so the row can be re-priced offline if the rate
/// shows up later.
pub fn compute_cost(
    kind: PriceKind,
    audio_seconds: Option<f64>,
    in_tokens: Option<i64>,
    out_tokens: Option<i64>,
    usd_per_second: Option<f64>,
    usd_per_input_token: Option<f64>,
    usd_per_output_token: Option<f64>,
) -> Option<f64> {
    match kind {
        PriceKind::PerAudioSecond => {
            let secs = audio_seconds?;
            let rate = usd_per_second?;
            Some(secs * rate)
        }
        PriceKind::PerToken => {
            let in_cost = match (in_tokens, usd_per_input_token) {
                (Some(n), Some(rate)) if n >= 0 => Some(n as f64 * rate),
                _ => None,
            };
            let out_cost = match (out_tokens, usd_per_output_token) {
                (Some(n), Some(rate)) if n >= 0 => Some(n as f64 * rate),
                _ => None,
            };
            match (in_cost, out_cost) {
                (Some(a), Some(b)) => Some(a + b),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn price_kind_round_trips_via_wire() {
        for kind in [PriceKind::PerAudioSecond, PriceKind::PerToken] {
            let wire = kind.as_wire();
            assert_eq!(PriceKind::from_wire(wire), Some(kind));
        }
        assert_eq!(PriceKind::from_wire("nonsense"), None);
    }

    #[test]
    fn compute_cost_per_audio_second_is_product() {
        let cost = compute_cost(
            PriceKind::PerAudioSecond,
            Some(60.0),
            None,
            None,
            Some(0.000_08),
            None,
            None,
        );
        assert!(approx(cost.unwrap(), 60.0 * 0.000_08));
    }

    #[test]
    fn compute_cost_per_audio_second_missing_inputs_yields_none() {
        // No audio_seconds.
        assert!(compute_cost(
            PriceKind::PerAudioSecond,
            None,
            None,
            None,
            Some(0.000_08),
            None,
            None,
        )
        .is_none());
        // No rate.
        assert!(compute_cost(
            PriceKind::PerAudioSecond,
            Some(60.0),
            None,
            None,
            None,
            None,
            None,
        )
        .is_none());
    }

    #[test]
    fn compute_cost_per_token_sums_input_and_output() {
        let cost = compute_cost(
            PriceKind::PerToken,
            None,
            Some(1_000_000),
            Some(500_000),
            None,
            Some(0.000_000_59),
            Some(0.000_000_79),
        );
        assert!(approx(
            cost.unwrap(),
            1_000_000.0 * 0.000_000_59 + 500_000.0 * 0.000_000_79,
        ));
    }

    #[test]
    fn compute_cost_per_token_partial_sides_still_compute() {
        // Output-only (no completion tokens reported).
        let in_only = compute_cost(
            PriceKind::PerToken,
            None,
            Some(1000),
            None,
            None,
            Some(0.000_000_05),
            Some(0.000_000_08),
        );
        assert!(approx(in_only.unwrap(), 1000.0 * 0.000_000_05));

        let out_only = compute_cost(
            PriceKind::PerToken,
            None,
            None,
            Some(2000),
            None,
            Some(0.000_000_05),
            Some(0.000_000_08),
        );
        assert!(approx(out_only.unwrap(), 2000.0 * 0.000_000_08));
    }

    #[test]
    fn compute_cost_per_token_missing_both_sides_yields_none() {
        let cost = compute_cost(
            PriceKind::PerToken,
            None,
            None,
            None,
            None,
            Some(0.000_000_59),
            Some(0.000_000_79),
        );
        assert!(cost.is_none());
    }

    #[test]
    fn default_prices_compute_cost_correctly() {
        // Every default entry must produce a positive cost when fed
        // representative usage units.
        for entry in DEFAULT_PRICES {
            let cost = match entry.kind {
                PriceKind::PerAudioSecond => compute_cost(
                    entry.kind,
                    Some(60.0),
                    None,
                    None,
                    entry.usd_per_second,
                    None,
                    None,
                ),
                PriceKind::PerToken => compute_cost(
                    entry.kind,
                    None,
                    Some(1_000),
                    Some(500),
                    None,
                    entry.usd_per_input_token,
                    entry.usd_per_output_token,
                ),
            };
            let value = cost.unwrap_or_else(|| {
                panic!(
                    "default entry {}/{} produced None cost",
                    entry.provider, entry.model
                )
            });
            assert!(
                value > 0.0,
                "default entry {}/{} should have positive cost, got {}",
                entry.provider,
                entry.model,
                value
            );
        }
    }

    #[test]
    fn default_prices_cover_required_provider_models() {
        // Bootstrap contract: these (provider, model) pairs must ship in
        // the rate sheet. Guard against an accidental edit dropping one.
        let required: &[(&str, &str)] = &[
            (PROVIDER_DEEPGRAM, "nova-3"),
            (PROVIDER_GROQ, "whisper-large-v3-turbo"),
            (PROVIDER_GROQ, "openai/gpt-oss-120b"),
            (PROVIDER_GROQ, "openai/gpt-oss-20b"),
            (PROVIDER_GEMINI, "gemini-2.5-flash-lite"),
            (PROVIDER_GLADIA, "solaria-1"),
        ];
        for (provider, model) in required {
            let found = DEFAULT_PRICES
                .iter()
                .any(|e| e.provider == *provider && e.model == *model);
            assert!(found, "missing default entry for {provider}/{model}");
        }
    }
}
