//! Feature 019 confidence-trigger: config resolution + the background
//! mid-press LID re-pass task for [`DictationSession`] (plan 039 slice 25).
//!
//! Extracted verbatim from `session.rs` as a child module. Shared constants
//! and `RollingBuffer` stay in the parent and are reached through
//! `use super::*`; only the public config API is re-exported by `super`.

use super::*;

/// Resolved trigger configuration, frozen at task-spawn time. Carried
/// by value into the trigger task so per-press env-var reads are O(1)
/// instead of O(chunks).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConfidenceTriggerConfig {
    /// `true` when the feature is enabled for this press.
    pub enabled: bool,
    /// Per-chunk confidence below which a chunk counts as low.
    pub threshold: f32,
    /// Consecutive low-confidence chunks needed to fire the re-pass.
    pub consecutive: usize,
    /// Re-pass slice size in samples (16 kHz).
    pub slice_samples: usize,
}

/// Parse env vars into a [`ConfidenceTriggerConfig`]. Out-of-range or
/// unparseable values are logged at warn and fall back to defaults so
/// a misconfigured env can't break a press.
pub fn load_confidence_trigger_config() -> ConfidenceTriggerConfig {
    let enabled = std::env::var(MUNI_LID_CONFIDENCE_TRIGGER_ENV)
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);

    if !enabled {
        return ConfidenceTriggerConfig {
            enabled: false,
            threshold: DEFAULT_CONFIDENCE_TRIGGER_THRESHOLD,
            consecutive: DEFAULT_CONFIDENCE_TRIGGER_CONSECUTIVE,
            slice_samples: DEFAULT_CONFIDENCE_TRIGGER_SLICE_SAMPLES,
        };
    }

    let threshold = match std::env::var(MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD_ENV)
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
    {
        Some(t) if (0.0..=1.0).contains(&t) => t,
        Some(bad) => {
            log::warn!(
                target: "lid",
                "MUNI_LID_CONFIDENCE_TRIGGER_THRESHOLD={bad} out of [0.0, 1.0] — using default {DEFAULT_CONFIDENCE_TRIGGER_THRESHOLD}"
            );
            DEFAULT_CONFIDENCE_TRIGGER_THRESHOLD
        }
        None => DEFAULT_CONFIDENCE_TRIGGER_THRESHOLD,
    };

    let consecutive = match std::env::var(MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE_ENV)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        Some(n) if n >= 1 => n,
        Some(bad) => {
            log::warn!(
                target: "lid",
                "MUNI_LID_CONFIDENCE_TRIGGER_CONSECUTIVE={bad} must be ≥1 — using default {DEFAULT_CONFIDENCE_TRIGGER_CONSECUTIVE}"
            );
            DEFAULT_CONFIDENCE_TRIGGER_CONSECUTIVE
        }
        None => DEFAULT_CONFIDENCE_TRIGGER_CONSECUTIVE,
    };

    let slice_samples = match std::env::var(MUNI_LID_CONFIDENCE_TRIGGER_SLICE_SECONDS_ENV)
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
    {
        Some(s)
            if (CONFIDENCE_TRIGGER_SLICE_SECONDS_MIN..=CONFIDENCE_TRIGGER_SLICE_SECONDS_MAX)
                .contains(&s) =>
        {
            (s * TARGET_SAMPLE_RATE as f32) as usize
        }
        Some(bad) => {
            log::warn!(
                target: "lid",
                "MUNI_LID_CONFIDENCE_TRIGGER_SLICE_SECONDS={bad} out of [{CONFIDENCE_TRIGGER_SLICE_SECONDS_MIN}, {CONFIDENCE_TRIGGER_SLICE_SECONDS_MAX}] — using default {DEFAULT_CONFIDENCE_TRIGGER_SLICE_SAMPLES}"
            );
            DEFAULT_CONFIDENCE_TRIGGER_SLICE_SAMPLES
        }
        None => DEFAULT_CONFIDENCE_TRIGGER_SLICE_SAMPLES,
    };

    ConfidenceTriggerConfig {
        enabled,
        threshold,
        consecutive,
        slice_samples,
    }
}

/// Parse the release-drain override. Out-of-range values fall back to
/// the default. Returning `0` is legal and disables the drain entirely.
pub fn load_confidence_trigger_drain_ms() -> u64 {
    match std::env::var(MUNI_LID_CONFIDENCE_TRIGGER_DRAIN_MS_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(n) if n <= CONFIDENCE_TRIGGER_DRAIN_MS_MAX => n,
        Some(bad) => {
            log::warn!(
                target: "lid",
                "MUNI_LID_CONFIDENCE_TRIGGER_DRAIN_MS={bad} exceeds {CONFIDENCE_TRIGGER_DRAIN_MS_MAX} ms — using default {DEFAULT_CONFIDENCE_TRIGGER_DRAIN_MS}"
            );
            DEFAULT_CONFIDENCE_TRIGGER_DRAIN_MS
        }
        None => DEFAULT_CONFIDENCE_TRIGGER_DRAIN_MS,
    }
}

impl DictationSession {
    /// Feature 019 — background task that watches per-chunk Deepgram
    /// confidence after pass#2 commits English and fires a mid-press
    /// LID re-pass when a run of consecutive low-confidence chunks
    /// crosses the configured threshold. If the re-pass classifies
    /// non-English it calls
    /// [`Self::override_decision_deepgram_to_whisper`] to flip the
    /// route, aborts the forwarder, and closes the Deepgram WS so the
    /// press finalizes against Whisper batch on the full buffered
    /// audio.
    ///
    /// Fires at most once per press in v1. The decision is final: even
    /// when the re-pass says English the task exits rather than
    /// continuing to monitor — re-firing would risk oscillation when
    /// the speaker is between phrases. See the plan's "Why
    /// fire-once-per-press" note for the v2 rationale.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_confidence_trigger_task(
        confidence_rx: mpsc::Receiver<crate::deepgram::ChunkConfidence>,
        audio_rx: broadcast::Receiver<Vec<i16>>,
        decision: Arc<TokioMutex<Option<RouterDecision>>>,
        decision_notify: Arc<Notify>,
        committed: Arc<AtomicBool>,
        release_tx: watch::Sender<bool>,
        forwarder_aborted: Arc<AtomicBool>,
        deepgram_client: Arc<DeepgramClient>,
        whisper_client: Arc<GroqWhisperClient>,
        lid_client: Arc<dyn TextLidClassifier>,
        whisper_api_key: String,
        usage_tx: Option<mpsc::Sender<UsageRecord>>,
        cfg: ConfidenceTriggerConfig,
        trigger_inflight: Arc<AtomicBool>,
    ) -> tauri::async_runtime::JoinHandle<()> {
        tauri::async_runtime::spawn(async move {
            log::info!(
                target: "lid",
                "confidence trigger armed (threshold={}, consecutive={}, slice_samples={})",
                cfg.threshold,
                cfg.consecutive,
                cfg.slice_samples
            );

            let mut rolling = RollingBuffer::new(CONFIDENCE_TRIGGER_ROLLING_BUFFER_CAP_SAMPLES);
            let mut consecutive_low: usize = 0;
            let mut confidence_rx = confidence_rx;
            let mut audio_rx = audio_rx;
            // Own release waiter (plan 039 task 13).
            let mut release_rx = release_tx.subscribe();
            // Dogfood instrumentation: ring of recent confidences so
            // the "fired" / "reset" log lines can show the sequence
            // that drove the decision. Capped at 16 — more than
            // enough context, small enough to keep the line readable.
            const RECENT_CONF_CAP: usize = 16;
            let mut recent_conf: std::collections::VecDeque<f32> =
                std::collections::VecDeque::with_capacity(RECENT_CONF_CAP);

            loop {
                tokio::select! {
                    biased;
                    () = released(&mut release_rx) => {
                        log::debug!(
                            target: "lid",
                            "confidence trigger: release received before fire — exiting"
                        );
                        return;
                    }
                    chunk = audio_rx.recv() => match chunk {
                        Ok(samples) => rolling.push(&samples),
                        Err(RecvError::Lagged(n)) => {
                            log::warn!(
                                target: "lid",
                                "confidence trigger audio lagged by {n} chunks — buffer freshness compromised"
                            );
                            continue;
                        }
                        Err(RecvError::Closed) => {
                            log::debug!(
                                target: "lid",
                                "confidence trigger: audio broadcast closed — exiting"
                            );
                            return;
                        }
                    },
                    event = confidence_rx.recv() => match event {
                        None => {
                            log::debug!(
                                target: "lid",
                                "confidence channel closed — exiting trigger task"
                            );
                            return;
                        }
                        Some(event) => {
                            // Push into the recent-confidence ring.
                            if recent_conf.len() == RECENT_CONF_CAP {
                                recent_conf.pop_front();
                            }
                            recent_conf.push_back(event.confidence);

                            let was_low = event.confidence < cfg.threshold;
                            if was_low {
                                consecutive_low = consecutive_low.saturating_add(1);
                            } else {
                                consecutive_low = 0;
                            }
                            log::debug!(
                                target: "lid",
                                "confidence trigger counter: {} (chunk confidence={:.3} words={} → {}, rolling_buf_samples={})",
                                consecutive_low,
                                event.confidence,
                                event.words_in_chunk,
                                if was_low { "low (+1)" } else { "high (reset)" },
                                rolling.len(),
                            );

                            if consecutive_low < cfg.consecutive {
                                continue;
                            }

                            // Threshold met — try to fire the re-pass.
                            if rolling.len() < CONFIDENCE_TRIGGER_MIN_REPASS_SAMPLES {
                                log::debug!(
                                    target: "lid",
                                    "confidence trigger threshold met but rolling buffer too small ({} samples) — waiting for more audio",
                                    rolling.len()
                                );
                                // Don't reset the counter — the next
                                // chunk may push us over the minimum.
                                continue;
                            }

                            let snapshot = rolling.snapshot_last_n_samples(cfg.slice_samples);
                            let recent_fmt: Vec<String> =
                                recent_conf.iter().map(|c| format!("{c:.2}")).collect();
                            log::info!(
                                target: "lid",
                                "confidence trigger fired — re-pass running (consecutive={} threshold={} slice_samples={} recent_confidences=[{}])",
                                consecutive_low,
                                cfg.threshold,
                                snapshot.len(),
                                recent_fmt.join(",")
                            );

                            // Signal to `finalize_auto_detect` that a
                            // re-pass is mid-flight. The release path
                            // checks this flag and waits briefly for
                            // the verdict (up to
                            // `TRIGGER_REPASS_WAIT_MS`) before
                            // committing the route — so a code-switch
                            // press released near the moment of fire
                            // still has its tail captured.
                            //
                            // Cleared on every return path below via
                            // the drop guard so a cancelled task
                            // (release aborts the handle mid-await)
                            // doesn't leak a stale `true`.
                            //
                            // The guard also fires
                            // `decision_notify.notify_waiters()` on
                            // drop, regardless of verdict. Without
                            // this, an inflight-waiter on a press
                            // where pass#3 returns English (no flip,
                            // so no override notify) sleeps the full
                            // `TRIGGER_REPASS_WAIT_MS` budget — a
                            // ~1.2 s latency leak observed in dogfood
                            // 2026-05-15 on pure-English armed
                            // presses with a fire-during-drain. The
                            // override's own `notify_waiters()` call
                            // on the flip path is now redundant but
                            // harmless (the guard fires after, and
                            // both notifies are idempotent against an
                            // already-woken waiter).
                            struct InflightGuard<'a> {
                                inflight: &'a Arc<AtomicBool>,
                                notify: &'a Arc<Notify>,
                            }
                            impl<'a> Drop for InflightGuard<'a> {
                                fn drop(&mut self) {
                                    self.inflight.store(false, Ordering::SeqCst);
                                    self.notify.notify_waiters();
                                }
                            }
                            trigger_inflight.store(true, Ordering::SeqCst);
                            let _inflight_guard = InflightGuard {
                                inflight: &trigger_inflight,
                                notify: &decision_notify,
                            };

                            let transcript = match Self::transcribe_for_lid(
                                "pass#3",
                                &whisper_client,
                                &snapshot,
                                &whisper_api_key,
                                usage_tx.as_ref(),
                            )
                            .await
                            {
                                Ok(t) => t,
                                Err(()) => {
                                    log::warn!(
                                        target: "lid",
                                        "confidence trigger re-pass transcribe failed — exiting trigger (v1 fires once)"
                                    );
                                    return;
                                }
                            };

                            let label = match Self::classify_text_only(
                                "pass#3",
                                lid_client.as_ref(),
                                &transcript,
                                usage_tx.as_ref(),
                            )
                            .await
                            {
                                Ok(l) => l,
                                Err(()) => {
                                    log::warn!(
                                        target: "lid",
                                        "confidence trigger re-pass classify failed — exiting trigger (v1 fires once)"
                                    );
                                    return;
                                }
                            };

                            if label.is_english() {
                                // Reset and keep monitoring. Dogfood
                                // 2026-05-15 showed that a single
                                // disfluency (cough, "uh", sneeze)
                                // produces a confidence=0.000 chunk
                                // that fires pass#3 on a slice of
                                // surrounding English audio — Whisper
                                // happily transcribes the cough as
                                // "Ahem." and Groq classifies the
                                // slice as English. If we exited here
                                // (the v1 fire-once design), the
                                // trigger would be dead and a *real*
                                // code-switch later in the same press
                                // would slip through. Resetting the
                                // counter and continuing the loop
                                // costs an extra Whisper+LID call on
                                // each future disfluency (bounded —
                                // Deepgram only emits finals at
                                // ~300 ms silence gaps), and the
                                // bidirectional safety holds: the
                                // override is still rejected when
                                // `committed=true` and the cell can
                                // only flip `Deepgram → Whisper`.
                                log::info!(
                                    target: "lid",
                                    "confidence trigger fired but stayed English (reset) — continuing to monitor"
                                );
                                consecutive_low = 0;
                                continue;
                            }

                            let flipped = Self::override_decision_deepgram_to_whisper(
                                &decision,
                                &decision_notify,
                                &committed,
                            )
                            .await;
                            if flipped {
                                log::info!(
                                    target: "lid",
                                    "confidence trigger flipped route to Whisper (pass#3 label={})",
                                    label.as_log_str()
                                );
                                forwarder_aborted.store(true, Ordering::SeqCst);
                                deepgram_client.close().await;
                            } else {
                                log::info!(
                                    target: "lid",
                                    "confidence trigger aborted — finalize won the race"
                                );
                            }
                            return;
                        }
                    }
                }
            }
        })
    }
}
