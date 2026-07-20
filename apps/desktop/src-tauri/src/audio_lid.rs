//! Local audio-based language identification (feature 020).
//!
//! Replaces the text-LID round-trip (transcribe via Whisper → classify
//! the transcript via an LLM) with a single local pass over the raw
//! audio buffer through whisper.cpp's encoder + language head. The
//! initial model is `ggml-tiny-q5_1.bin` (~32 MB) bundled as a Tauri
//! resource and loaded once at app boot.
//!
//! ## Why a trait
//!
//! Same rationale as [`crate::text_lid::TextLidClassifier`]: the
//! concrete backend may change (model variant, alternative audio-LID
//! impls — SeamlessM4T, VoxLingua107) without touching call sites.
//!
//! ## What it returns
//!
//! [`AudioLidVerdict`] carries:
//! - `label` — the routing decision (English | Tagalog | Taglish | Other),
//!   derived from the top-1 ISO code + the en/tl probability split.
//! - `top1_lang` — raw ISO code (`"en"`, `"tl"`, `"ko"`, …) from
//!   whisper's language head. Useful in the sliding-window "non-en/non-tl
//!   = keep checking" rule, which only the routing layer can apply.
//! - `top1_prob`, `p_en`, `p_tl` — confidence numbers for logging and
//!   future windowing heuristics.
//! - `latency_ms` — time spent inside `classify`, so the caller can
//!   record it in `UsageRecord.latency_ms` without re-timing.
//!
//! The routing decision is computed here (not at the call site) so
//! tests can exercise the en/tl → label mapping without standing up a
//! `WhisperContext`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tauri::{AppHandle, Manager, Runtime};
use whisper_rs::{WhisperContext, WhisperContextParameters};

use crate::error::MuniError;
use crate::text_lid::LidLabel;

/// Filename of the bundled model. Same name appears in `tauri.conf.json`
/// `bundle.resources` and in the source-tree dev fallback under
/// `apps/desktop/src-tauri/resources/`.
pub const AUDIO_LID_MODEL_FILENAME: &str = "ggml-tiny-q5_1.bin";

/// Probability floor on the *secondary* language for a verdict to count
/// as Taglish (both en and tl above the floor). Mirrors the spike's
/// `PROB_MIN_FOR_TAGLISH_SECONDARY` constant.
///
/// Default raised from `0.10` → `0.18` on 2026-05-21 (feat/027, backlog
/// 0047) after re-measure showed 4/10 false-positive flips on English-only
/// content with mid-press silence — three of which were triggered by
/// weak `p_tl ∈ {0.11, 0.13, 0.17}` windows. Runtime-tunable via
/// [`MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV`].
pub const TAGLISH_SECONDARY_PROB_FLOOR: f32 = 0.18;

/// Env var overriding [`TAGLISH_SECONDARY_PROB_FLOOR`]. Floor value
/// in `[0.0, 1.0)`. Unset / unparseable / out-of-range values fall
/// back to [`TAGLISH_SECONDARY_PROB_FLOOR`]. Resolved once per
/// classifier at construction time (boot-stable; restart required
/// to apply a change).
pub const MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV: &str =
    "MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR";

/// Resolve [`MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV`] once at
/// classifier construction. Falls back to
/// [`TAGLISH_SECONDARY_PROB_FLOOR`] for unset / unparseable /
/// non-finite / out-of-range (`[0.0, 1.0)`) values. Warns on parse
/// failure or invalid range; silent on absent env (default path).
pub fn load_audio_lid_taglish_secondary_floor() -> f32 {
    match std::env::var(MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV) {
        Ok(raw) => match raw.trim().parse::<f32>() {
            Ok(v) if v.is_finite() && (0.0..1.0).contains(&v) => v,
            Ok(v) => {
                log::warn!(
                    target: "lid",
                    "{MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV}={v} out of range [0.0, 1.0) — using default {TAGLISH_SECONDARY_PROB_FLOOR}"
                );
                TAGLISH_SECONDARY_PROB_FLOOR
            }
            Err(_) => {
                log::warn!(
                    target: "lid",
                    "{MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV}={raw:?} not parseable — using default {TAGLISH_SECONDARY_PROB_FLOOR}"
                );
                TAGLISH_SECONDARY_PROB_FLOOR
            }
        },
        Err(_) => TAGLISH_SECONDARY_PROB_FLOOR,
    }
}

/// Outcome of one audio-LID classify call. Shared between callers and
/// tests; the routing layer builds its `RouterDecision` from `label`,
/// while the windowing layer reads `top1_lang` + `top1_prob` to decide
/// whether to keep checking subsequent windows.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioLidVerdict {
    /// Routing label derived from `top1_lang`, `p_en`, `p_tl`.
    pub label: LidLabel,
    /// ISO code of the highest-probability language whisper returned.
    pub top1_lang: String,
    /// Probability of `top1_lang`.
    pub top1_prob: f32,
    /// Probability mass on `"en"` (zero if not present in the table).
    pub p_en: f32,
    /// Probability mass on `"tl"` (zero if not present in the table).
    pub p_tl: f32,
    /// Wall-clock time spent inside `classify`. Surfaced so the caller
    /// can record it in the usage row without re-timing the call.
    pub latency_ms: f64,
}

/// Local-audio LID classifier.
///
/// Implementations consume 16 kHz mono int16 PCM (the same format the
/// rest of the audio pipeline produces — see [`crate::audio`]). The
/// trait deliberately accepts `&[i16]` rather than `&[f32]`: the
/// in-memory rolling buffer in `session.rs` is `Vec<i16>`, and the
/// f32 conversion only matters at the whisper.cpp boundary.
#[async_trait]
pub trait AudioLidClassifier: Send + Sync {
    /// Classify `samples`. Implementations MUST NOT panic on empty
    /// input — the orchestrator's fallback "default to multilingual
    /// ASR on LID failure" relies on a clean error path.
    async fn classify(&self, samples: &[i16]) -> Result<AudioLidVerdict, MuniError>;

    /// Short identifier embedded in `[lid]` log lines. Format
    /// convention: `"<provider>:<model>"` — e.g.
    /// `"whisper_local:tiny-q5_1"`.
    fn provider_label(&self) -> &str;
}

/// Concrete whisper.cpp / whisper-rs–backed audio-LID.
#[derive(Clone)]
pub struct WhisperAudioLid {
    ctx: Arc<WhisperContext>,
    provider_label: String,
    threads: usize,
    taglish_secondary_floor: f32,
    /// Flipped `true` once the background Metal pre-warm
    /// ([`WhisperAudioLid::spawn_prewarm`]) has JIT-compiled the kernels.
    /// `classify` reads it purely for a cold-start log line — a not-yet-warm
    /// classify is still correct, just slower, so nothing gates on this.
    warm: Arc<AtomicBool>,
}

impl std::fmt::Debug for WhisperAudioLid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhisperAudioLid")
            .field("provider_label", &self.provider_label)
            .field("threads", &self.threads)
            .field("taglish_secondary_floor", &self.taglish_secondary_floor)
            .finish_non_exhaustive()
    }
}

impl WhisperAudioLid {
    /// Default thread count for the encoder + language head. Apple
    /// Silicon's perf cores saturate well at 4; the spike measured
    /// p95 = 67 ms on M2 Pro with `threads = 4`.
    pub const DEFAULT_THREADS: usize = 4;

    /// Build from a live `AppHandle`. Resolves the bundled model via
    /// the same cascade as [`crate::prompt::CleanupPrompt::from_app`]:
    /// 1. `resource_dir()/resources/<filename>` (production).
    /// 2. In debug builds, `CARGO_MANIFEST_DIR/resources/<filename>`
    ///    so `pnpm tauri dev` works without copying `bundle.resources`.
    pub fn from_app<R: Runtime>(app: &AppHandle<R>) -> Result<Self, MuniError> {
        let model_path = resolve_model_path(app)?;
        Self::from_model_path(&model_path)
    }

    /// Direct constructor for tests + integration harnesses. Loads the
    /// model file at `path` into a fresh `WhisperContext` and kicks a
    /// **background** Metal-kernel pre-warm (see [`Self::spawn_prewarm`]).
    ///
    /// The pre-warm JIT-compiles the Metal shaders the first real press would
    /// otherwise pay for (several seconds on Apple Silicon on first
    /// `create_state` + `pcm_to_mel`). Plan 039 task 19 moved it OFF the
    /// constructor thread onto `spawn_blocking` so `setup` returns immediately
    /// instead of blocking the whole app boot on the warm-up classify — dogfood
    /// 2026-05-18 caught the first English press dropping to the "LID not ready
    /// after 1000 ms" fallback because the *synchronous* warm-up hadn't landed.
    pub fn from_model_path(path: &std::path::Path) -> Result<Self, MuniError> {
        let path_str = path.to_string_lossy();
        log::info!(
            target: "lid",
            "audio-LID loading model: {}",
            path.display()
        );
        let ctx = WhisperContext::new_with_params(&path_str, WhisperContextParameters::default())
            .map_err(|e| MuniError::AudioLidLoadFailed {
            reason: format!(
                "WhisperContext::new_with_params({}) failed: {e}",
                path.display()
            ),
        })?;
        let model_slug = model_slug_from_filename(path);
        let provider_label = format!("whisper_local:{model_slug}");
        let taglish_secondary_floor = load_audio_lid_taglish_secondary_floor();
        log::info!(
            target: "lid",
            "audio-LID taglish secondary floor: {taglish_secondary_floor} (default {TAGLISH_SECONDARY_PROB_FLOOR})"
        );
        let me = Self {
            ctx: Arc::new(ctx),
            provider_label,
            threads: Self::DEFAULT_THREADS,
            taglish_secondary_floor,
            warm: Arc::new(AtomicBool::new(false)),
        };
        me.spawn_prewarm();
        Ok(me)
    }

    /// Kick the Metal-kernel pre-warm on the blocking pool so it never stalls
    /// `setup` (plan 039 task 19). Boot completes immediately; the warm-up
    /// classify runs in the background and flips the `warm` flag when its
    /// kernels are compiled. Uses `spawn_blocking` because the warm-up is
    /// CPU/GPU-bound synchronous work, not an async I/O wait.
    fn spawn_prewarm(&self) {
        let me = self.clone();
        tauri::async_runtime::spawn_blocking(move || {
            if me.prewarm_metal() {
                me.warm.store(true, Ordering::SeqCst);
            }
        });
    }

    /// Run one dummy classify on a 2 s silence buffer so Metal kernels
    /// are JIT-compiled before the user's first press lands. Logged at
    /// info with the elapsed millis so cold-start regressions are
    /// visible in the boot log. Returns `true` when the warm-up classify
    /// succeeded (kernels are compiled).
    ///
    /// Failures are logged at warn but never bubbled — a warmup
    /// failure is purely a latency regression on the first press, not
    /// a correctness one. The orchestrator's "LID not ready → default
    /// to Whisper" fallback still produces correct output.
    fn prewarm_metal(&self) -> bool {
        let silence = vec![0_i16; AUDIO_LID_PREWARM_SAMPLES];
        let started = Instant::now();
        match run_classify(
            &self.ctx,
            &silence,
            self.threads,
            self.taglish_secondary_floor,
        ) {
            Ok(_) => {
                log::info!(
                    target: "lid",
                    "audio-LID Metal pre-warm completed in {} ms",
                    started.elapsed().as_millis()
                );
                true
            }
            Err(err) => {
                log::warn!(
                    target: "lid",
                    "audio-LID Metal pre-warm failed in {} ms: {} — first press may be slow",
                    started.elapsed().as_millis(),
                    err.user_message()
                );
                false
            }
        }
    }
}

/// Length of the silence buffer used for the boot-time Metal kernel
/// pre-warm. 2 s @ 16 kHz mirrors the production `AUDIO_LID_WINDOW_SAMPLES`
/// so the JIT compiler sees the same input shape the live windowing
/// path will hit.
const AUDIO_LID_PREWARM_SAMPLES: usize = 32_000;

#[async_trait]
impl AudioLidClassifier for WhisperAudioLid {
    async fn classify(&self, samples: &[i16]) -> Result<AudioLidVerdict, MuniError> {
        if samples.is_empty() {
            return Err(MuniError::AudioLidInferenceFailed {
                reason: "empty sample buffer".into(),
            });
        }
        // Cold-start observability: a classify that beats the background
        // pre-warm falls through the same (correct, just slower) non-prewarmed
        // Metal JIT path the pre-warm exists to avoid. Log it so an early-press
        // latency spike is explainable in the boot log.
        if !self.warm.load(Ordering::SeqCst) {
            log::debug!(
                target: "lid",
                "audio-LID classify before Metal pre-warm completed — first press may pay the JIT cost"
            );
        }
        let ctx = self.ctx.clone();
        let threads = self.threads;
        let floor = self.taglish_secondary_floor;
        let samples_owned = samples.to_vec();
        // whisper-rs's `pcm_to_mel` + `lang_detect` are blocking
        // (CPU/GPU bound). Run them on the blocking pool so the
        // session's main tokio runtime isn't stalled while the
        // encoder runs.
        let join =
            tokio::task::spawn_blocking(move || run_classify(&ctx, &samples_owned, threads, floor));
        join.await.map_err(|e| MuniError::AudioLidInferenceFailed {
            reason: format!("spawn_blocking join failed: {e}"),
        })?
    }

    fn provider_label(&self) -> &str {
        &self.provider_label
    }
}

fn run_classify(
    ctx: &WhisperContext,
    samples_i16: &[i16],
    threads: usize,
    floor: f32,
) -> Result<AudioLidVerdict, MuniError> {
    let samples_f32 = samples_i16_to_f32(samples_i16);
    let mut state = ctx
        .create_state()
        .map_err(|e| MuniError::AudioLidInferenceFailed {
            reason: format!("create_state failed: {e}"),
        })?;
    let started = Instant::now();
    state
        .pcm_to_mel(&samples_f32, threads)
        .map_err(|e| MuniError::AudioLidInferenceFailed {
            reason: format!("pcm_to_mel failed: {e}"),
        })?;
    let (top_id, probs) =
        state
            .lang_detect(0, threads)
            .map_err(|e| MuniError::AudioLidInferenceFailed {
                reason: format!("lang_detect failed: {e}"),
            })?;
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;

    let top1_lang = lang_str(top_id);
    let top1_prob = probs.get(top_id as usize).copied().unwrap_or(0.0);
    let p_en = prob_for("en", &probs);
    let p_tl = prob_for("tl", &probs);
    let label = label_from_probs(&top1_lang, p_en, p_tl, floor);

    Ok(AudioLidVerdict {
        label,
        top1_lang,
        top1_prob,
        p_en,
        p_tl,
        latency_ms,
    })
}

/// Convert int16 PCM samples into the f32 mono buffer whisper.cpp
/// expects. Saturating divide by `i16::MAX` matches the spike harness
/// and whisper.cpp's own `wav_to_float` convention.
pub fn samples_i16_to_f32(samples: &[i16]) -> Vec<f32> {
    let scale = 1.0_f32 / i16::MAX as f32;
    samples.iter().map(|&s| s as f32 * scale).collect()
}

/// Reverse-lookup helper: given an ISO code (`"en"`, `"tl"`, …), return
/// the probability whisper assigned to it, or 0.0 if the code isn't
/// in the table.
pub fn prob_for(code: &str, probs: &[f32]) -> f32 {
    for id in 0..probs.len() as i32 {
        if lang_str(id) == code {
            return probs[id as usize];
        }
    }
    0.0
}

/// Map the verdict (top-1 ISO code + en/tl masses) to a [`LidLabel`].
///
/// The product rule from feature 003 + 020, parameterised by `floor`
/// (the secondary-language probability threshold for a Taglish verdict):
/// - `top1_lang == "en"` AND `p_tl < floor` → English (route to Deepgram
///   fast path).
/// - `top1_lang == "en"` AND `p_tl >= floor` → Taglish (route to Whisper
///   multilingual).
/// - `top1_lang == "tl"` → Tagalog or Taglish depending on `p_en` vs `floor`.
/// - `top1_lang` is neither en/tl AND `p_tl >= floor` → Taglish. This
///   catches the dogfood 2026-05-18 failure mode where a short Tagalog
///   prefix ("Hindi ko alam", "Sabi ko sa kanya") puts significant
///   probability mass on `tl` but the model mis-picks the top-1 slot.
///   Example surviving at the default `floor=0.18`: `top1=he p_tl=0.22`
///   still recovers as Taglish. (Pre-feat/027, a `top1=ar p_tl=0.17`
///   case also recovered; that case now requires an env override —
///   see trade-off note below.)
///   Without this rule the window labelled as `Other(_)`, the windowing
///   layer kept checking, and by the next window the Tagalog had
///   already scrolled past — so Deepgram committed and dropped the
///   Tagalog. Reading the secondary mass recovers the route.
/// - Anything else → `Other(top1_lang)`. The routing layer's
///   "non-en/non-tl = keep checking" rule reads this and waits for
///   the next window before committing.
///
/// **Default floor raised from `0.10` → `0.18` on 2026-05-21**
/// (feat/027, backlog 0047) after dogfood showed 4/10 false-positive
/// flips on English-only content where weak `p_tl ∈ {0.11, 0.13, 0.17}`
/// windows tripped the old floor. The `he/p_tl=0.22` recovery case
/// still works; the 2026-05-18 `ar/p_tl=0.17` case now requires an env
/// override (`MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR=0.15`) to recover.
pub fn label_from_probs(top1_lang: &str, p_en: f32, p_tl: f32, floor: f32) -> LidLabel {
    match top1_lang {
        "en" => {
            if p_tl >= floor {
                LidLabel::Taglish
            } else {
                LidLabel::English
            }
        }
        "tl" => {
            if p_en >= floor {
                LidLabel::Taglish
            } else {
                LidLabel::Tagalog
            }
        }
        other => {
            if p_tl >= floor {
                LidLabel::Taglish
            } else {
                LidLabel::Other(other.to_string())
            }
        }
    }
}

/// Whisper's language id → ISO code mapping.
///
/// Whisper-rs exposes the list internally but the public API surface
/// varies across releases. Hard-coding the table here keeps the
/// integration API-version-independent. Copied verbatim from the
/// spike harness (`scripts/lid_spike/src/main.rs::lang_str`).
pub fn lang_str(id: i32) -> String {
    const KNOWN: &[(i32, &str)] = &[
        (0, "en"),
        (1, "zh"),
        (2, "de"),
        (3, "es"),
        (4, "ru"),
        (5, "ko"),
        (6, "fr"),
        (7, "ja"),
        (8, "pt"),
        (9, "tr"),
        (10, "pl"),
        (11, "ca"),
        (12, "nl"),
        (13, "ar"),
        (14, "sv"),
        (15, "it"),
        (16, "id"),
        (17, "hi"),
        (18, "fi"),
        (19, "vi"),
        (20, "he"),
        (21, "uk"),
        (22, "el"),
        (23, "ms"),
        (24, "cs"),
        (25, "ro"),
        (26, "da"),
        (27, "hu"),
        (28, "ta"),
        (29, "no"),
        (30, "th"),
        (31, "ur"),
        (32, "hr"),
        (33, "bg"),
        (34, "lt"),
        (35, "la"),
        (36, "mi"),
        (37, "ml"),
        (38, "cy"),
        (39, "sk"),
        (40, "te"),
        (41, "fa"),
        (42, "lv"),
        (43, "bn"),
        (44, "sr"),
        (45, "az"),
        (46, "sl"),
        (47, "kn"),
        (48, "et"),
        (49, "mk"),
        (50, "br"),
        (51, "eu"),
        (52, "is"),
        (53, "hy"),
        (54, "ne"),
        (55, "mn"),
        (56, "bs"),
        (57, "kk"),
        (58, "sq"),
        (59, "sw"),
        (60, "gl"),
        (61, "mr"),
        (62, "pa"),
        (63, "si"),
        (64, "km"),
        (65, "sn"),
        (66, "yo"),
        (67, "so"),
        (68, "af"),
        (69, "oc"),
        (70, "ka"),
        (71, "be"),
        (72, "tg"),
        (73, "sd"),
        (74, "gu"),
        (75, "am"),
        (76, "yi"),
        (77, "lo"),
        (78, "uz"),
        (79, "fo"),
        (80, "ht"),
        (81, "ps"),
        (82, "tk"),
        (83, "nn"),
        (84, "mt"),
        (85, "sa"),
        (86, "lb"),
        (87, "my"),
        (88, "bo"),
        (89, "tl"),
        (90, "mg"),
        (91, "as"),
        (92, "tt"),
        (93, "haw"),
        (94, "ln"),
        (95, "ha"),
        (96, "ba"),
        (97, "jw"),
        (98, "su"),
    ];
    for (i, code) in KNOWN {
        if *i == id {
            return (*code).to_string();
        }
    }
    format!("lang_{id}")
}

/// Resolve the bundled model path, cascading from the production
/// resource dir to the source-tree dev fallback.
fn resolve_model_path<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, MuniError> {
    if let Ok(rd) = app.path().resource_dir() {
        let candidate = rd.join("resources").join(AUDIO_LID_MODEL_FILENAME);
        if candidate.exists() {
            return Ok(candidate);
        }
        log::debug!(
            target: "lid",
            "audio-LID resource_dir model missing, falling through: {}",
            candidate.display()
        );
    } else {
        log::warn!(
            target: "lid",
            "audio-LID resource_dir lookup failed; trying source-tree fallback"
        );
    }

    #[cfg(debug_assertions)]
    {
        let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join(AUDIO_LID_MODEL_FILENAME);
        if dev.exists() {
            log::info!(
                target: "lid",
                "audio-LID using dev source-tree model path: {}",
                dev.display()
            );
            return Ok(dev);
        }
        log::warn!(
            target: "lid",
            "audio-LID dev fallback model path missing too: {}",
            dev.display()
        );
    }

    Err(MuniError::AudioLidLoadFailed {
        reason: format!(
            "model file '{AUDIO_LID_MODEL_FILENAME}' not found in resource dir or dev fallback"
        ),
    })
}

/// Derive a short model slug for log lines / usage records from the
/// bundled filename: `ggml-tiny-q5_1.bin` → `tiny-q5_1`.
fn model_slug_from_filename(path: &std::path::Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    stem.strip_prefix("ggml-").unwrap_or(stem).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_str_maps_english_and_tagalog() {
        assert_eq!(lang_str(0), "en");
        assert_eq!(lang_str(89), "tl");
        // A neighbour to guard against off-by-one if the table is ever
        // re-typed by hand.
        assert_eq!(lang_str(5), "ko");
    }

    #[test]
    fn lang_str_unknown_returns_lang_id_fallback() {
        assert_eq!(lang_str(999), "lang_999");
    }

    #[test]
    fn samples_i16_to_f32_silence_round_trips_to_zero() {
        let zeros = vec![0_i16; 16];
        let f = samples_i16_to_f32(&zeros);
        assert_eq!(f.len(), 16);
        assert!(f.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn samples_i16_to_f32_saturates_at_one() {
        let f = samples_i16_to_f32(&[i16::MAX]);
        assert!((f[0] - 1.0).abs() < 1e-6);
        let f = samples_i16_to_f32(&[-i16::MAX]);
        assert!((f[0] + 1.0).abs() < 1e-6);
    }

    #[test]
    fn samples_i16_to_f32_empty_returns_empty() {
        assert!(samples_i16_to_f32(&[]).is_empty());
    }

    #[test]
    fn prob_for_returns_zero_when_code_absent() {
        // table has 99 entries; an empty probs slice yields 0 for every
        // code via the fall-through.
        assert_eq!(prob_for("en", &[]), 0.0);
    }

    #[test]
    fn prob_for_picks_correct_index() {
        // Indices 0 = en, 89 = tl. Build a vector that just covers
        // those indices.
        let mut probs = vec![0.0_f32; 99];
        probs[0] = 0.7;
        probs[89] = 0.2;
        assert!((prob_for("en", &probs) - 0.7).abs() < 1e-6);
        assert!((prob_for("tl", &probs) - 0.2).abs() < 1e-6);
        assert_eq!(prob_for("xx", &probs), 0.0);
    }

    #[test]
    fn label_from_probs_pure_english() {
        assert_eq!(
            label_from_probs("en", 0.95, 0.01, TAGLISH_SECONDARY_PROB_FLOOR),
            LidLabel::English
        );
    }

    #[test]
    fn label_from_probs_pure_tagalog() {
        assert_eq!(
            label_from_probs("tl", 0.02, 0.95, TAGLISH_SECONDARY_PROB_FLOOR),
            LidLabel::Tagalog
        );
    }

    #[test]
    fn label_from_probs_taglish_from_en_top1() {
        // English on top, but Tagalog probability mass is high enough
        // that the press is mixed — route to multilingual.
        assert_eq!(
            label_from_probs("en", 0.55, 0.30, TAGLISH_SECONDARY_PROB_FLOOR),
            LidLabel::Taglish
        );
    }

    #[test]
    fn label_from_probs_taglish_from_tl_top1() {
        assert_eq!(
            label_from_probs("tl", 0.30, 0.55, TAGLISH_SECONDARY_PROB_FLOOR),
            LidLabel::Taglish
        );
    }

    #[test]
    fn label_from_probs_other_for_non_en_non_tl_top1_with_low_p_tl() {
        // Top-1 is a non-en/non-tl language AND `p_tl` is below the
        // Taglish floor — this is genuine noise (cough, silence, weak
        // Other). Stay Other so the windowing layer keeps checking.
        match label_from_probs("ko", 0.05, 0.05, TAGLISH_SECONDARY_PROB_FLOOR) {
            LidLabel::Other(code) => assert_eq!(code, "ko"),
            other => panic!("expected Other(ko), got {other:?}"),
        }
    }

    #[test]
    fn label_from_probs_taglish_floor_is_inclusive() {
        // Exactly TAGLISH_SECONDARY_PROB_FLOOR on the secondary should
        // count as Taglish (route to Whisper). This is the same bar the
        // spike's Verdict::Pass uses for taglish_sentence_mix cases.
        assert_eq!(
            label_from_probs(
                "en",
                0.6,
                TAGLISH_SECONDARY_PROB_FLOOR,
                TAGLISH_SECONDARY_PROB_FLOOR
            ),
            LidLabel::Taglish
        );
    }

    #[test]
    fn label_from_probs_taglish_when_other_top1_carries_tagalog_secondary_mass() {
        // Dogfood 2026-05-18 (Fix 4): when the model mis-picks top-1
        // on a short Tagalog prefix but the secondary mass on `tl`
        // crosses the floor, treat as Taglish. Real observed cases:
        //   "Hindi ko alam exactly…" → top1=ar p_tl=0.17 → Taglish (pre-feat/027)
        //   "Hindi ko alam exactly…" → top1=he p_tl=0.22 → Taglish (still)
        //
        // feat/027 (backlog 0047) raised the default floor 0.10 → 0.18.
        // The 2026-05-18 `ar/p_tl=0.17` boundary now regresses to Other(ar);
        // the `he/p_tl=0.22` case is preserved. Acknowledged trade-off —
        // see `.claude/brainstorm/008_backlog_0047_remeasure_protocol.md`.
        assert!(matches!(
            label_from_probs("ar", 0.06, 0.17, TAGLISH_SECONDARY_PROB_FLOOR),
            LidLabel::Other(_)
        ));
        assert_eq!(
            label_from_probs("he", 0.04, 0.22, TAGLISH_SECONDARY_PROB_FLOOR),
            LidLabel::Taglish
        );
        // Other "noisy" languages with weak Tagalog secondary stay
        // Other — the rule fires only when the secondary mass is real.
        assert!(matches!(
            label_from_probs("ru", 0.14, 0.00, TAGLISH_SECONDARY_PROB_FLOOR),
            LidLabel::Other(_)
        ));
        assert!(matches!(
            label_from_probs("pt", 0.08, 0.00, TAGLISH_SECONDARY_PROB_FLOOR),
            LidLabel::Other(_)
        ));
    }

    #[test]
    fn label_from_probs_other_top1_floor_is_inclusive_for_taglish() {
        // Exactly at the floor crosses (mirrors the en/tl boundary
        // test). Pins the inclusive semantics so future tuning of the
        // floor doesn't silently flip the boundary.
        assert_eq!(
            label_from_probs(
                "ar",
                0.05,
                TAGLISH_SECONDARY_PROB_FLOOR,
                TAGLISH_SECONDARY_PROB_FLOOR
            ),
            LidLabel::Taglish
        );
    }

    /// Backlog 0047 dogfood (2026-05-21) — the three weak-`p_tl` false
    /// positives observed on English-only content with mid-press silence.
    /// All three must label as English with the post-feat/027 default floor
    /// (0.18). Pins the dogfood-derived calibration so future tuning can't
    /// silently regress.
    #[test]
    fn label_from_probs_blocks_backlog_0047_weak_ptl_false_positives() {
        // Press 1 trigger: p_tl=0.11
        assert_eq!(
            label_from_probs("en", 0.77, 0.11, TAGLISH_SECONDARY_PROB_FLOOR),
            LidLabel::English
        );
        // Press 7 trigger: p_tl=0.13
        assert_eq!(
            label_from_probs("en", 0.45, 0.13, TAGLISH_SECONDARY_PROB_FLOOR),
            LidLabel::English
        );
        // Press 9 trigger: p_tl=0.17 (just below new floor)
        assert_eq!(
            label_from_probs("en", 0.26, 0.17, TAGLISH_SECONDARY_PROB_FLOOR),
            LidLabel::English
        );
    }

    /// Backlog 0047 dogfood (2026-05-21) — Press 13's genuine code-switch
    /// at p_tl=0.28 must STILL flip. This is the lowest-confidence correct
    /// flip in the dataset; protects the genuine-detection guard-rail.
    #[test]
    fn label_from_probs_preserves_backlog_0047_genuine_code_switch() {
        // Press 13 trigger: top1=tl p_tl=0.28 p_en=0.22 → still Taglish
        // (p_en >= floor=0.18). Whichever, NOT English (the routing
        // layer flips to Whisper on either Taglish or Tagalog label).
        assert_eq!(
            label_from_probs("tl", 0.22, 0.28, TAGLISH_SECONDARY_PROB_FLOOR),
            LidLabel::Taglish
        );
    }

    #[test]
    fn model_slug_from_filename_strips_ggml_prefix_and_extension() {
        let p = std::path::PathBuf::from("/foo/bar/ggml-tiny-q5_1.bin");
        assert_eq!(model_slug_from_filename(&p), "tiny-q5_1");
    }

    #[test]
    fn model_slug_from_filename_passes_through_when_no_ggml_prefix() {
        let p = std::path::PathBuf::from("/foo/bar/custom-model.bin");
        assert_eq!(model_slug_from_filename(&p), "custom-model");
    }

    #[test]
    fn audio_lid_verdict_carries_label_and_probs() {
        let v = AudioLidVerdict {
            label: LidLabel::English,
            top1_lang: "en".into(),
            top1_prob: 0.91,
            p_en: 0.91,
            p_tl: 0.02,
            latency_ms: 67.5,
        };
        assert_eq!(v.label, LidLabel::English);
        assert!(v.p_en > v.p_tl);
        assert!(v.latency_ms > 0.0);
    }

    #[test]
    fn from_model_path_returns_load_failed_on_missing_file() {
        let nonexistent = std::path::PathBuf::from("/tmp/this/does/not/exist.bin");
        let err =
            WhisperAudioLid::from_model_path(&nonexistent).expect_err("expected load failure");
        match err {
            MuniError::AudioLidLoadFailed { .. } => {}
            other => panic!("expected AudioLidLoadFailed, got {other:?}"),
        }
    }

    // ---- feat/027 (backlog 0047): taglish secondary floor resolver -------
    //
    // `MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR` is a process-global env
    // var. Serialize the resolver tests with a local Mutex so parallel
    // workers can't observe each other's writes — mirrors the
    // `VAD_ENV_LOCK` pattern in `lib.rs::lib_tests`.
    static LID_FLOOR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Resolver respects an env override within the valid range.
    #[test]
    fn load_audio_lid_taglish_secondary_floor_parses_valid() {
        let _guard = LID_FLOOR_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV, "0.25");
        let v = load_audio_lid_taglish_secondary_floor();
        std::env::remove_var(MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV);
        assert!((v - 0.25).abs() < 1e-6);
    }

    /// Resolver falls back to default when env var is absent.
    #[test]
    fn load_audio_lid_taglish_secondary_floor_default_when_unset() {
        let _guard = LID_FLOOR_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV);
        let v = load_audio_lid_taglish_secondary_floor();
        assert!((v - TAGLISH_SECONDARY_PROB_FLOOR).abs() < 1e-6);
    }

    /// Resolver falls back on non-numeric env value.
    #[test]
    fn load_audio_lid_taglish_secondary_floor_default_when_unparseable() {
        let _guard = LID_FLOOR_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV, "not-a-number");
        let v = load_audio_lid_taglish_secondary_floor();
        std::env::remove_var(MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV);
        assert!((v - TAGLISH_SECONDARY_PROB_FLOOR).abs() < 1e-6);
    }

    /// Resolver falls back on out-of-range (>= 1.0) value.
    #[test]
    fn load_audio_lid_taglish_secondary_floor_default_when_out_of_range_high() {
        let _guard = LID_FLOOR_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV, "1.5");
        let v = load_audio_lid_taglish_secondary_floor();
        std::env::remove_var(MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV);
        assert!((v - TAGLISH_SECONDARY_PROB_FLOOR).abs() < 1e-6);
    }

    /// Resolver falls back on negative value.
    #[test]
    fn load_audio_lid_taglish_secondary_floor_default_when_negative() {
        let _guard = LID_FLOOR_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV, "-0.1");
        let v = load_audio_lid_taglish_secondary_floor();
        std::env::remove_var(MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV);
        assert!((v - TAGLISH_SECONDARY_PROB_FLOOR).abs() < 1e-6);
    }

    /// Resolver falls back on NaN / Inf.
    #[test]
    fn load_audio_lid_taglish_secondary_floor_default_when_non_finite() {
        let _guard = LID_FLOOR_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV, "NaN");
        let v_nan = load_audio_lid_taglish_secondary_floor();
        std::env::set_var(MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV, "inf");
        let v_inf = load_audio_lid_taglish_secondary_floor();
        std::env::remove_var(MUNI_AUDIO_LID_TAGLISH_SECONDARY_FLOOR_ENV);
        assert!((v_nan - TAGLISH_SECONDARY_PROB_FLOOR).abs() < 1e-6);
        assert!((v_inf - TAGLISH_SECONDARY_PROB_FLOOR).abs() < 1e-6);
    }
}
