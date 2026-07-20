//! Language-ID router for [`DictationSession`] (plan 039 slice 25): the
//! two-pass text-LID protocol, the audio-LID pass + verdict application, and
//! the audio-hybrid cross-reference task.
//!
//! Extracted verbatim from `session.rs` as a child module. Pure decision
//! helpers and constants stay in the parent and are reached through
//! `use super::*`; only methods the parent driver still calls are widened to
//! `pub(super)`.

use super::*;

impl DictationSession {
    /// Background task that runs the two-pass text-LID protocol.
    ///
    /// **Pass #1** — wait for [`LID_SLICE_SAMPLES`] of audio,
    /// transcribe it via Whisper, classify the *text* via the
    /// [`TextLidClassifier`]. If non-English, switch to Whisper and
    /// exit. If English, continue to pass #2.
    ///
    /// **Pass #2** — wait for [`LID_SLICE_SAMPLES_SECOND`] of audio
    /// (longer window), transcribe + classify again. Catches the
    /// English-leading Taglish failure mode where the first slice is
    /// dominantly English and the model picks `english` even
    /// though the press is code-switched.
    ///
    /// On any "non-English" verdict the task closes the Deepgram
    /// socket and signals the forwarder via `aborted`; the forwarder
    /// keeps collecting samples for the Whisper batch path.
    ///
    /// **Failure handling — feature 003 rule**: every failure path
    /// (no Whisper client, no Gemini client, missing keys, transcribe
    /// error, classifier error, slice too short) defaults the
    /// decision to **Whisper**, not Deepgram. Whisper handles both
    /// languages correctly; routing to Deepgram on uncertainty
    /// silently produces confidently-wrong English output for any
    /// Tagalog content. The cost of the alternative (a slow English
    /// press) is much smaller than the cost of unreadable output.
    ///
    /// The release watch `release_tx` (backlog 0011) is fired by
    /// [`Self::handle_hotkey_released`] so the pass#2 collection loop
    /// breaks out of `lid_chunks_rx.recv()` on release. The broadcast
    /// `Sender` lives on `AudioCapture` and is **not** dropped on
    /// `stop()`, so without this signal the pass#2 loop hangs after
    /// release until `lid_handle.abort()` fires from
    /// `finalize_auto_detect` — which routes short pure-English
    /// presses (1.5–3.5 s window) into Whisper batch instead of
    /// trusting pass#1's English verdict.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn_lid_task(
        &self,
        deepgram_client: Arc<DeepgramClient>,
        mut lid_chunks_rx: broadcast::Receiver<Vec<i16>>,
        aborted: Arc<AtomicBool>,
        decision: Arc<TokioMutex<Option<RouterDecision>>>,
        decision_notify: Arc<Notify>,
        release_tx: watch::Sender<bool>,
        committed: Arc<AtomicBool>,
        gemini_handle_slot: Arc<TokioMutex<Option<tauri::async_runtime::JoinHandle<()>>>>,
        confidence_trigger_handle_slot: Arc<
            TokioMutex<Option<tauri::async_runtime::JoinHandle<()>>>,
        >,
        trigger_inflight: Arc<AtomicBool>,
        audio_hybrid_handle_slot: Arc<TokioMutex<Option<tauri::async_runtime::JoinHandle<()>>>>,
        audio_hybrid_inflight: Arc<AtomicUsize>,
        audio_hybrid_speech_mirror: Arc<TokioMutex<Vec<i16>>>,
        audio_lid_drift_counter: Arc<AtomicUsize>,
        audio_lid_last_post_commit_was_other: Arc<AtomicBool>,
        audio_lid_release_other_as_taglish: bool,
        // Backlog 0052 — threaded into `run_audio_lid_pass` so the
        // mid-press + at-release drift-fire decisions can consult the
        // hybrid text-LID's most-recent verdict.
        audio_hybrid_recent_text_lid_english: Arc<AtomicBool>,
        audio_lid_hybrid_veto_drift: bool,
    ) -> tauri::async_runtime::JoinHandle<()> {
        let whisper = self.deps.whisper.clone();
        let text_lid = self.deps.text_lid.clone();
        let text_lid_secondary = self.deps.text_lid_secondary.clone();
        let audio_lid = self.deps.audio_lid.clone();
        let vad_detector = self.deps.vad_detector.clone();
        let usage_tx = self.deps.usage_tx.clone();
        let bilingual_mode = self.deps.bilingual_mode.clone();
        // Feature 024 (backlog 0042) — resolve the streaming-VAD
        // factory + the hybrid-side kill switch once at spawn time so
        // the spawned task sees deterministic values (env-var reads
        // serialise here, not inside the hot loop).
        let streaming_vad_factory = self.deps.streaming_vad_factory.clone();
        let stream_hybrid_enabled = crate::resolve_vad_stream_hybrid_enabled();
        // Feature 025 (backlog 0046) — resolve the audio-LID per-window
        // silence-gate kill switch once at spawn time, same shape as
        // the feat/024 resolution above so the env-var read serialises
        // deterministically before the spawned task starts.
        let audio_lid_gate_enabled = crate::resolve_vad_audio_lid_gate_enabled();
        // Feature 019 — resolved once at spawn so per-chunk reads
        // inside the trigger task are O(1) and the dogfood log emitted
        // at "armed" time reflects exactly the config the task uses.
        let confidence_trigger_cfg = load_confidence_trigger_config();
        tauri::async_runtime::spawn(async move {
            // Feature 020 — audio-LID short-circuit. When the audio-LID
            // classifier is wired in (set by `MUNI_LID_PROVIDER=audio_whisper_tiny`
            // at boot) it owns the entire LID protocol: collect ~2 s of
            // audio, classify locally, commit the route, then keep
            // running 1 s-spaced windows to catch mid-press drift. The
            // text-LID two-pass path below is the rollback target —
            // unreached when audio-LID is active.
            //
            // Feature 021 — `run_audio_lid_pass` also owns the audio-LID
            // side of the parallel-Gemini hybrid: a fresh broadcast
            // receiver is resubscribed inside the pass so the hybrid
            // task can observe chunks without contending with the
            // audio-LID windowing loop.
            if let Some(audio_lid_client) = audio_lid {
                let hybrid_audio_rx = lid_chunks_rx.resubscribe();
                // Bundle the per-press coordination handles (plan 039 task 13)
                // so the audio-LID spawn chain takes one argument instead of a
                // nine-deep stack.
                let press_shared = PressShared {
                    decision: decision.clone(),
                    decision_notify: decision_notify.clone(),
                    release_tx: release_tx.clone(),
                    committed: committed.clone(),
                    aborted: aborted.clone(),
                    audio_hybrid_inflight: audio_hybrid_inflight.clone(),
                    audio_hybrid_recent_text_lid_english: audio_hybrid_recent_text_lid_english
                        .clone(),
                    audio_lid_drift_counter: audio_lid_drift_counter.clone(),
                    audio_lid_last_post_commit_was_other: audio_lid_last_post_commit_was_other
                        .clone(),
                };
                Self::run_audio_lid_pass(
                    audio_lid_client,
                    deepgram_client.clone(),
                    lid_chunks_rx,
                    press_shared,
                    usage_tx.clone(),
                    bilingual_mode.clone(),
                    whisper.clone(),
                    text_lid_secondary.clone(),
                    vad_detector.clone(),
                    hybrid_audio_rx,
                    audio_hybrid_handle_slot.clone(),
                    streaming_vad_factory.clone(),
                    stream_hybrid_enabled,
                    audio_lid_gate_enabled,
                    audio_hybrid_speech_mirror.clone(),
                    audio_lid_release_other_as_taglish,
                    audio_lid_hybrid_veto_drift,
                )
                .await;
                return;
            }
            // Text-LID rollback path: its own release waiter (plan 039 task 13).
            // One receiver reused across the pass#1 + pass#2 collection loops —
            // `released` is sticky, so a release that fires during a pass's
            // transcribe still short-circuits the next loop.
            let mut release_rx = release_tx.subscribe();
            // Helper closure: switch to Whisper, closing the Deepgram
            // socket so we stop streaming bytes the user may not want
            // routed there. Capturing `aborted`/`deepgram_client`/
            // `decision`/`decision_notify` keeps the call sites short.
            let switch_to_whisper = |reason: &str| {
                let aborted = aborted.clone();
                let deepgram_client = deepgram_client.clone();
                let decision = decision.clone();
                let decision_notify = decision_notify.clone();
                let reason = reason.to_string();
                async move {
                    log::info!(target: "lid", "switching to Whisper: {reason}");
                    aborted.store(true, Ordering::SeqCst);
                    deepgram_client.close().await;
                    Self::set_decision(&decision, &decision_notify, RouterDecision::Whisper).await;
                }
            };
            // For "fallback" cases we still default to Whisper but
            // log at warn — the difference is only the log target /
            // wording so post-hoc analysis can tell deliberate
            // routing apart from failure-handling fallbacks.
            let fallback_to_whisper = |reason: &str| {
                let aborted = aborted.clone();
                let deepgram_client = deepgram_client.clone();
                let decision = decision.clone();
                let decision_notify = decision_notify.clone();
                let reason = reason.to_string();
                async move {
                    log::warn!(target: "lid", "fallback → Whisper: {reason}");
                    aborted.store(true, Ordering::SeqCst);
                    deepgram_client.close().await;
                    Self::set_decision(&decision, &decision_notify, RouterDecision::Whisper).await;
                }
            };

            // Backlog 0012 escape hatch: user opted into bilingual mode
            // via tray toggle / settings. Skip LID entirely and route
            // every press to Whisper — preserves Tagalog content that
            // Deepgram would silently drop on mid-press code-switches.
            // Checked before pass#1 so we don't pay Whisper transcribe
            // + Groq/Gemini classify costs on a press whose route is
            // already decided.
            if bilingual_mode.is_enabled() {
                log::info!(
                    target: "lid",
                    "bilingual_mode enabled — skipping LID, routing to Whisper for bilingual correctness"
                );
                if whisper.is_none() {
                    fallback_to_whisper("bilingual_mode enabled but Whisper client unavailable")
                        .await;
                    return;
                }
                aborted.store(true, Ordering::SeqCst);
                deepgram_client.close().await;
                Self::set_decision(&decision, &decision_notify, RouterDecision::Whisper).await;
                return;
            }

            let Some(whisper_client) = whisper else {
                fallback_to_whisper("Whisper client unavailable").await;
                return;
            };
            let Some(lid_client) = text_lid else {
                fallback_to_whisper("text-LID classifier unavailable").await;
                return;
            };

            // Mid-press read on the audio-LID pass — cached to keep the press
            // free of a keychain IPC (plan 039 task 17). Env override stays
            // live; keychain layer is invalidated on `secrets://changed`.
            let whisper_api_key = match secrets::get_cached(secrets::GROQ_ACCOUNT) {
                Ok(k) => k,
                Err(err) => {
                    fallback_to_whisper(&format!(
                        "Groq key unavailable for transcription: {}",
                        err.user_message()
                    ))
                    .await;
                    return;
                }
            };

            // Accumulator grows past pass #1 into pass #2 so we don't
            // re-collect bytes between passes.
            let mut slice: Vec<i16> = Vec::with_capacity(LID_SLICE_SAMPLES_SECOND);
            let mut stream_closed = false;

            // --- Pass #1 ---
            //
            // Race the chunk stream against the release watch
            // (`released(&mut release_rx)`). The
            // 1.5 s pass#1 slice can't be assumed to fill before
            // release: stream-startup latency burns ~300 ms of
            // wall-clock, and short presses ("sure, go ahead") often
            // capture only ~1.0–1.4 s of audio. Without the select!,
            // pass#1's bare `recv()` hangs on the never-closing
            // broadcast channel after `AudioCapture::stop()` and the
            // orchestrator times out into Whisper instead of running
            // pass#1 LID on the partial slice. `stream_closed` is
            // also set on release short-circuit because — like a
            // genuinely-closed channel — there is no more audio
            // coming, so pass#2 must not attempt further collection.
            loop {
                if slice.len() >= LID_SLICE_SAMPLES {
                    break;
                }
                tokio::select! {
                    biased;
                    c = lid_chunks_rx.recv() => match c {
                        Ok(chunk) => slice.extend_from_slice(&chunk),
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => {
                            stream_closed = true;
                            break;
                        }
                    },
                    () = released(&mut release_rx) => {
                        stream_closed = true;
                        break;
                    }
                }
            }

            if slice.is_empty() {
                fallback_to_whisper("press too short for any LID slice").await;
                return;
            }

            let pass1_slice_end = slice.len().min(LID_SLICE_SAMPLES);
            // Backlog 0012 dogfood (2026-05-11) showed Groq's pass#1
            // misclassifies long English-leading Taglish prefixes
            // (`"Okay, so the thing I…"`) as `taglish`, sending the
            // press to Whisper batch BEFORE pass#2 — so the pass#2-only
            // Gemini override never gets a chance. Pass#1 transcribe +
            // classify is now also split so the hybrid override can
            // race on the pass#1 transcript. See
            // `.claude/learned/002_deepgram_alone_is_not_enough_for_taglish.md`
            // for the design rationale (Deepgram alone is not safe;
            // strengthen LID, don't bypass it).
            let pass1_transcript = match Self::transcribe_for_lid(
                "pass#1",
                &whisper_client,
                &slice[..pass1_slice_end],
                &whisper_api_key,
                usage_tx.as_ref(),
            )
            .await
            {
                Ok(t) => t,
                Err(()) => {
                    fallback_to_whisper("pass#1 transcribe error").await;
                    return;
                }
            };

            let pass1_label = match Self::classify_text_only(
                "pass#1",
                lid_client.as_ref(),
                &pass1_transcript,
                usage_tx.as_ref(),
            )
            .await
            {
                Ok(label) => label,
                Err(()) => {
                    fallback_to_whisper("pass#1 LID error").await;
                    return;
                }
            };

            if !pass1_label.is_english() {
                // Backlog 0012 (post dogfood 2026-05-11): in hybrid
                // mode, commit Whisper deferred (no abort/close) AND
                // fall through to pass#2 collection. Pass#2 will
                // transcribe a longer 3.5 s slice — large enough for
                // Whisper to capture Tagalog particles ("mas", "kasi",
                // "yung") that the 1.5 s slice routinely smears into
                // English-sounding tokens. Pass#2's Gemini override
                // (running on the 3.5 s text) is the authoritative
                // second check. See
                // `.claude/learned/002_deepgram_alone_is_not_enough_for_taglish.md`
                // and dogfood findings: pass#1 Gemini on the short
                // slice classified mistranscribed Tagalog as English
                // 2/5 times, wrongly flipping correct-Whisper routes
                // to Deepgram with data loss.
                if text_lid_secondary.is_some() {
                    log::info!(
                        target: "lid",
                        "pass#1 (hybrid) classified as {} — deferring teardown and continuing to pass#2 for stronger verdict",
                        pass1_label.as_log_str()
                    );
                    Self::set_decision(&decision, &decision_notify, RouterDecision::Whisper).await;
                    // Fall through to pass#2 collection. Pass#2's
                    // Gemini override may flip Whisper → Deepgram if
                    // the longer transcript classifies as english.
                } else {
                    switch_to_whisper(&format!(
                        "pass#1 classified as {}",
                        pass1_label.as_log_str()
                    ))
                    .await;
                    return;
                }
            }

            // --- Pass #2 ---
            if stream_closed {
                // `set_decision` is first-write-wins; if pass#1 already
                // committed Whisper (hybrid non-english fall-through),
                // this call is a no-op and the cell stays Whisper. If
                // pass#1 said english (and didn't commit), this sets
                // Deepgram per trust-pass#1.
                log::info!(
                    target: "lid",
                    "press ended before pass#2 slice could be collected — finalizing pass#1 verdict"
                );
                Self::set_decision(&decision, &decision_notify, RouterDecision::Deepgram).await;
                return;
            }

            // Pass#2 collection: race the chunk stream against the
            // release signal. The broadcast `Sender` on
            // `AudioCapture` is not dropped on `stop()`, so a bare
            // `recv().await` hangs after release — the caller's
            // `lid_handle.abort()` is the only thing that ends the
            // task in that branch (~1 s wasted). When release fires
            // before the slice fills, fall through to the
            // "trust pass#1 (English)" short-circuit below.
            loop {
                if slice.len() >= LID_SLICE_SAMPLES_SECOND {
                    break;
                }
                tokio::select! {
                    biased;
                    c = lid_chunks_rx.recv() => match c {
                        Ok(chunk) => slice.extend_from_slice(&chunk),
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => break,
                    },
                    () = released(&mut release_rx) => break,
                }
            }

            if slice.len() < LID_SLICE_SAMPLES_SECOND {
                // Same first-write-wins semantics as the stream_closed
                // branch above: if pass#1 hybrid already committed
                // Whisper, this set_decision is a no-op.
                log::info!(
                    target: "lid",
                    "press too short for pass#2 slice ({} samples) — finalizing pass#1 verdict",
                    slice.len()
                );
                Self::set_decision(&decision, &decision_notify, RouterDecision::Deepgram).await;
                return;
            }

            // Pass#2 transcribe is shared by Groq's primary classify
            // AND (in hybrid mode) Gemini's parallel classify so we
            // don't pay 2× Whisper. Done before the parallel-spawn so
            // both classifiers see exactly the same text.
            let pass2_transcript = match Self::transcribe_for_lid(
                "pass#2",
                &whisper_client,
                &slice[..LID_SLICE_SAMPLES_SECOND],
                &whisper_api_key,
                usage_tx.as_ref(),
            )
            .await
            {
                Ok(t) => t,
                Err(()) => {
                    // First-write-wins again: pass#1 hybrid Whisper
                    // commit (if any) stays; otherwise this falls
                    // back to trust-pass#1 Deepgram.
                    log::warn!(
                        target: "lid",
                        "pass#2 transcribe errored — finalizing pass#1 verdict"
                    );
                    Self::set_decision(&decision, &decision_notify, RouterDecision::Deepgram).await;
                    return;
                }
            };

            // Backlog 0012 — when a secondary classifier is wired in
            // (env-gated by `MUNI_LID_HYBRID=true`), spawn Gemini's
            // classify BEFORE awaiting Groq's so they overlap. Gemini
            // checks the `committed` sentinel before mutating the
            // decision cell — late replies (after the orchestrator
            // routed the press) are dropped.
            let hybrid_mode = text_lid_secondary.is_some();
            if let Some(secondary) = text_lid_secondary.clone() {
                let trimmed = pass2_transcript.clone();
                let decision_for_override = decision.clone();
                let decision_notify_for_override = decision_notify.clone();
                let committed_for_override = committed.clone();
                let usage_tx_for_override = usage_tx.clone();
                let handle: tauri::async_runtime::JoinHandle<()> =
                    tauri::async_runtime::spawn(async move {
                        let result = Self::classify_text_only(
                            "gemini-override",
                            secondary.as_ref(),
                            &trimmed,
                            usage_tx_for_override.as_ref(),
                        )
                        .await;
                        if committed_for_override.load(Ordering::SeqCst) {
                            log::debug!(
                                target: "lid",
                                "gemini override late — orchestrator already routed"
                            );
                            return;
                        }
                        match result {
                            Ok(LidLabel::English) => {
                                let flipped = Self::override_decision_groq_to_gemini(
                                    &decision_for_override,
                                    &decision_notify_for_override,
                                    RouterDecision::Deepgram,
                                )
                                .await;
                                if flipped {
                                    log::info!(
                                        target: "lid",
                                        "gemini override applied: pass#2 Whisper → Deepgram"
                                    );
                                }
                            }
                            Ok(other) => {
                                log::info!(
                                    target: "lid",
                                    "gemini override skipped: classified {}",
                                    other.as_log_str()
                                );
                            }
                            Err(()) => {
                                // already logged inside classify_text_only
                            }
                        }
                    });
                // Hand the join handle to `AutoDetectActive` so
                // `finalize_auto_detect` can abort it on release
                // instead of letting an in-flight Gemini RPC keep
                // running (and burning a Gemini call) for a verdict
                // the orchestrator has already discarded via the
                // `committed` sentinel.
                *gemini_handle_slot.lock().await = Some(handle);
            }

            let pass2_label = match Self::classify_text_only(
                "pass#2",
                lid_client.as_ref(),
                &pass2_transcript,
                usage_tx.as_ref(),
            )
            .await
            {
                Ok(label) => label,
                Err(()) => {
                    // First-write-wins: pass#1 hybrid Whisper commit
                    // (if any) stays; otherwise trust-pass#1 Deepgram.
                    log::warn!(
                        target: "lid",
                        "pass#2 LID errored — finalizing pass#1 verdict"
                    );
                    Self::set_decision(&decision, &decision_notify, RouterDecision::Deepgram).await;
                    return;
                }
            };

            if pass2_label.is_english() {
                // First-write-wins: if pass#1 hybrid already committed
                // Whisper, this `set_decision(Deepgram)` is a no-op
                // and Gemini's override is the only path back to
                // Deepgram. Otherwise (pass#1 said english, no
                // commit), this commits Deepgram per trust-pass#2.
                log::info!(
                    target: "lid",
                    "pass#2 = english (Groq) — set_decision(Deepgram) [first-write-wins; gemini override below is the flip mechanism if pass#1 hybrid already committed Whisper]"
                );
                Self::set_decision(&decision, &decision_notify, RouterDecision::Deepgram).await;

                // Feature 019 — arm the mid-press confidence trigger.
                // Spawned ONLY when:
                //   * pass#2 actually committed Deepgram (this branch);
                //   * the cell is still Some(Deepgram) — i.e. pass#1
                //     hybrid didn't already commit Whisper, in which
                //     case `set_decision` above was a no-op and Gemini
                //     may still flip it back; the trigger has no role
                //     on a Whisper-bound press;
                //   * the feature flag is on.
                //
                // Feature 020: this entire block is unreachable when
                // `MUNI_LID_PROVIDER=audio_whisper_tiny` — the top of
                // `spawn_lid_task` short-circuits to
                // `run_audio_lid_pass` before any text-LID code runs,
                // so feat/019's confidence trigger only spawns under
                // the text-LID rollback path. The two mid-press
                // re-route mechanisms (per-word-confidence here,
                // per-window-LID-drift in `run_audio_lid_pass`) are
                // therefore mutually exclusive at boot.
                if confidence_trigger_cfg.enabled {
                    let cell_is_deepgram =
                        matches!(*decision.lock().await, Some(RouterDecision::Deepgram));
                    if cell_is_deepgram {
                        let trigger_audio_rx = lid_chunks_rx.resubscribe();
                        let (conf_tx, conf_rx) =
                            mpsc::channel::<crate::deepgram::ChunkConfidence>(64);
                        deepgram_client
                            .install_confidence_continuation(conf_tx)
                            .await;
                        let whisper_for_trigger = whisper_client.clone();
                        let lid_for_trigger = lid_client.clone();
                        let key_for_trigger = whisper_api_key.clone();
                        let usage_for_trigger = usage_tx.clone();
                        let decision_for_trigger = decision.clone();
                        let notify_for_trigger = decision_notify.clone();
                        let release_for_trigger = release_tx.clone();
                        let committed_for_trigger = committed.clone();
                        let aborted_for_trigger = aborted.clone();
                        let dg_for_trigger = deepgram_client.clone();
                        let inflight_for_trigger = trigger_inflight.clone();
                        let handle = Self::spawn_confidence_trigger_task(
                            conf_rx,
                            trigger_audio_rx,
                            decision_for_trigger,
                            notify_for_trigger,
                            committed_for_trigger,
                            release_for_trigger,
                            aborted_for_trigger,
                            dg_for_trigger,
                            whisper_for_trigger,
                            lid_for_trigger,
                            key_for_trigger,
                            usage_for_trigger,
                            confidence_trigger_cfg,
                            inflight_for_trigger,
                        );
                        *confidence_trigger_handle_slot.lock().await = Some(handle);
                    } else {
                        log::debug!(
                            target: "lid",
                            "confidence trigger not armed — cell is not Deepgram (pass#1 hybrid Whisper still in flight)"
                        );
                    }
                }
            } else if hybrid_mode {
                // Backlog 0012 — defer the Deepgram WS teardown so the
                // in-flight Gemini override can still flip the verdict
                // back to Deepgram. The forwarder keeps streaming
                // bytes (aborted stays false) so Deepgram has the full
                // press's audio if Gemini lands. `finalize_auto_detect`
                // closes the WS once it has consumed the final
                // verdict — idempotent close keeps non-hybrid presses
                // safe too.
                log::info!(
                    target: "lid",
                    "pass#2 (hybrid) classified as {} — deferring teardown for gemini override",
                    pass2_label.as_log_str()
                );
                Self::set_decision(&decision, &decision_notify, RouterDecision::Whisper).await;
            } else {
                switch_to_whisper(&format!(
                    "pass#2 classified as {} (code-switch caught)",
                    pass2_label.as_log_str()
                ))
                .await;
            }
        })
    }

    /// Transcribe an LID slice via Groq Whisper, log the elapsed
    /// transcribe latency, record one `UsageRecord` for the call, and
    /// return the trimmed transcript text. Returns `Err(())` on any
    /// failure (transport, empty transcript) — the caller is expected
    /// to apply the feature 003 failure-handling rule (default to
    /// Whisper).
    ///
    /// Pulled out of [`Self::run_text_lid_pass`] so the hybrid-mode
    /// pass#2 path (backlog 0012) can reuse the same transcript for
    /// both Groq's primary classify and Gemini's parallel classify
    /// without paying 2× Whisper.
    pub(super) async fn transcribe_for_lid(
        label: &str,
        whisper_client: &GroqWhisperClient,
        samples: &[i16],
        whisper_api_key: &str,
        usage_tx: Option<&mpsc::Sender<UsageRecord>>,
    ) -> Result<String, ()> {
        let started = Instant::now();
        let transcript = match whisper_client.transcribe(samples, whisper_api_key).await {
            Ok(t) => t,
            Err(err) => {
                log::warn!(
                    target: "lid",
                    "{label} whisper transcribe failed in {} ms: {}",
                    started.elapsed().as_millis(),
                    err.user_message()
                );
                return Err(());
            }
        };
        let transcribe_elapsed = started.elapsed();
        let trimmed = transcript.trim().to_string();
        if trimmed.is_empty() {
            log::warn!(
                target: "lid",
                "{label} whisper returned empty transcript in {} ms",
                transcribe_elapsed.as_millis()
            );
            return Err(());
        }

        log::debug!(
            target: "lid",
            "{label} whisper transcribe ok in {} ms ({} chars)",
            transcribe_elapsed.as_millis(),
            trimmed.len()
        );

        if let Some(tx) = usage_tx {
            try_send_drop_oldest(
                tx,
                UsageRecord {
                    provider: crate::pricing::PROVIDER_GROQ.into(),
                    model: crate::groq_whisper::DEFAULT_MODEL.into(),
                    call_kind: crate::pricing::CALL_KIND_ASR.into(),
                    audio_seconds: Some(
                        samples.len() as f64 / crate::groq_whisper::PCM_SAMPLE_RATE as f64,
                    ),
                    input_tokens: None,
                    output_tokens: None,
                    latency_ms: Some(transcribe_elapsed.as_millis() as i64),
                    status: "ok".into(),
                    request_id: None,
                    session_id: None,
                    created_at_unix: unix_seconds_now(),
                },
            );
        }

        Ok(trimmed)
    }

    /// Classify a pre-transcribed text via the supplied
    /// [`TextLidClassifier`]. Records one `UsageRecord` for the LID
    /// call. Returns `Err(())` on classify failure; the caller logs
    /// and applies the failure-handling default.
    ///
    /// Backlog 0012 — extracted so the parallel Gemini override task
    /// can classify the pass#2 transcript without re-transcribing.
    pub(super) async fn classify_text_only(
        label: &str,
        lid_client: &dyn TextLidClassifier,
        trimmed: &str,
        usage_tx: Option<&mpsc::Sender<UsageRecord>>,
    ) -> Result<LidLabel, ()> {
        let classify_started = Instant::now();
        let result = lid_client.classify(trimmed).await;
        let classify_elapsed = classify_started.elapsed();
        let provider = lid_client.provider_label();
        match result {
            Ok((lbl, usage)) => {
                log::info!(
                    target: "lid",
                    "{label} text-LID = {} via {provider} (classify {} ms) text={:?}",
                    lbl.as_log_str(),
                    classify_elapsed.as_millis(),
                    trimmed
                );
                if let Some(tx) = usage_tx {
                    let (provider_slug, model_slug) = split_lid_provider_label(provider);
                    try_send_drop_oldest(
                        tx,
                        UsageRecord {
                            provider: provider_slug.into(),
                            model: model_slug.into(),
                            call_kind: crate::pricing::CALL_KIND_LID.into(),
                            audio_seconds: None,
                            input_tokens: usage.map(|u| u.input_tokens),
                            output_tokens: usage.map(|u| u.output_tokens),
                            latency_ms: Some(classify_elapsed.as_millis() as i64),
                            status: "ok".into(),
                            request_id: None,
                            session_id: None,
                            created_at_unix: unix_seconds_now(),
                        },
                    );
                }
                Ok(lbl)
            }
            Err(err) => {
                log::warn!(
                    target: "lid",
                    "{label} classifier failed via {provider} in {} ms (text={:?}): {}",
                    classify_elapsed.as_millis(),
                    trimmed,
                    err.user_message()
                );
                Err(())
            }
        }
    }

    /// Helper: write the router decision and signal the release path.
    async fn set_decision(
        cell: &Arc<TokioMutex<Option<RouterDecision>>>,
        notify: &Arc<Notify>,
        value: RouterDecision,
    ) {
        let mut g = cell.lock().await;
        if g.is_none() {
            *g = Some(value);
        }
        notify.notify_waiters();
    }

    /// Backlog 0012 — apply the Gemini parallel verdict over Groq's
    /// pass#2 verdict. Constrained to flipping `Whisper → Deepgram`
    /// only (Gemini cannot downgrade Groq's English verdict; that
    /// direction is reserved to protect long-Taglish accuracy —
    /// scenario 6 in `docs/qa/001_pass2_commit_hole_close.md`).
    ///
    /// Returns `true` if the cell was flipped and `notify_waiters`
    /// fired; `false` if the override was a no-op (no decision yet,
    /// already Deepgram, or unsupported direction).
    ///
    /// Distinct from [`Self::set_decision`] (which is first-write-wins
    /// to preserve backlog 0011's release-watch path); the override
    /// is the only way an in-flight verdict re-routes a press after
    /// the primary classifier has already committed.
    pub(super) async fn override_decision_groq_to_gemini(
        cell: &Arc<TokioMutex<Option<RouterDecision>>>,
        notify: &Arc<Notify>,
        new_value: RouterDecision,
    ) -> bool {
        let mut g = cell.lock().await;
        if matches!(*g, Some(RouterDecision::Whisper)) && new_value == RouterDecision::Deepgram {
            *g = Some(new_value);
            notify.notify_waiters();
            return true;
        }
        false
    }

    /// Feature 019 — flip a pass#2-committed `Deepgram` decision to
    /// `Whisper` when the confidence-trigger re-pass catches a
    /// mid-press code-switch.
    ///
    /// Mirror of [`Self::override_decision_groq_to_gemini`] but in the
    /// inverse direction (Deepgram → Whisper). Only fires when:
    ///   * `committed` is still `false` — i.e.
    ///     [`Self::finalize_auto_detect`] has not locked the route
    ///     yet; and
    ///   * the current decision is exactly `Some(Deepgram)` — refuses
    ///     to clobber an already-Whisper or never-committed cell.
    ///
    /// Returns `true` when the cell was flipped and the release path
    /// notified; `false` for any no-op (committed, wrong expected,
    /// race lost).
    pub(super) async fn override_decision_deepgram_to_whisper(
        cell: &Arc<TokioMutex<Option<RouterDecision>>>,
        notify: &Arc<Notify>,
        committed: &Arc<AtomicBool>,
    ) -> bool {
        // Cheap pre-check outside the lock — the most common reason for
        // a no-op is that finalize already raced us, and avoiding the
        // lock acquire reduces contention with `finalize_auto_detect`.
        if committed.load(Ordering::SeqCst) {
            return false;
        }
        let mut g = cell.lock().await;
        // Re-check after lock to close the window between the load
        // above and the lock acquire.
        if committed.load(Ordering::SeqCst) {
            return false;
        }
        if matches!(*g, Some(RouterDecision::Deepgram)) {
            *g = Some(RouterDecision::Whisper);
            notify.notify_waiters();
            true
        } else {
            false
        }
    }

    /// Feature 021 fix 2026-05-18 — hybrid-path "commit or override"
    /// to Whisper. Handles both the *pre-commit* (`*g == None`) and
    /// the *post-Deepgram-commit* (`*g == Some(Deepgram)`) cases in
    /// a single atomic operation.
    ///
    /// **Why distinct from [`Self::override_decision_deepgram_to_whisper`]:**
    /// the strict override is the right semantic for the drift
    /// detector and feat/019 confidence trigger — both of those only
    /// fire *after* audio-LID has committed Deepgram, so the route is
    /// always `Some(Deepgram)` when they touch the cell. Broadening
    /// the strict override to also clobber `None` would silently
    /// change behaviour for those callers.
    ///
    /// The audio-LID-hybrid case is structurally different: the
    /// hybrid's leading-slice classify can land in the *gap* between
    /// audio-LID's first "keep checking" window and its eventual
    /// commit. Dogfood 2026-05-18: 4 of 7 retries of "Hindi ko alam
    /// exactly..." had the hybrid land `taglish` while `*g == None`,
    /// the strict override no-op'd, then audio-LID's next English
    /// window committed Deepgram, and "Hindi ko alam" was dropped
    /// from the paste.
    ///
    /// Behaviour matrix:
    ///
    /// | `committed` | `*g`               | Result                |
    /// |-------------|--------------------|-----------------------|
    /// | `true`      | (any)              | `false` (no-op)       |
    /// | `false`     | `Some(Whisper)`    | `false` (already safe)|
    /// | `false`     | `Some(Deepgram)`   | `true` (flip)         |
    /// | `false`     | `None`             | `true` (pre-commit)   |
    ///
    /// Returns `true` when the cell was written and waiters notified;
    /// `false` for any no-op.
    pub(crate) async fn override_or_commit_to_whisper_via_hybrid(
        cell: &Arc<TokioMutex<Option<RouterDecision>>>,
        notify: &Arc<Notify>,
        committed: &Arc<AtomicBool>,
    ) -> bool {
        // Cheap pre-check outside the lock — see
        // `override_decision_deepgram_to_whisper` for the rationale.
        if committed.load(Ordering::SeqCst) {
            return false;
        }
        let mut g = cell.lock().await;
        if committed.load(Ordering::SeqCst) {
            return false;
        }
        match *g {
            Some(RouterDecision::Whisper) => false,
            Some(RouterDecision::Deepgram) | None => {
                *g = Some(RouterDecision::Whisper);
                notify.notify_waiters();
                true
            }
        }
    }

    /// Feature 020 — audio-LID press loop. Replaces the text-LID
    /// two-pass protocol when `SessionDeps.audio_lid` is set
    /// (`MUNI_LID_PROVIDER=audio_whisper_tiny`).
    ///
    /// Lifecycle:
    /// 1. Honour the bilingual-mode escape hatch (skip LID, route to
    ///    Whisper) so the manual override behaves identically across
    ///    LID backends.
    /// 2. Collect chunks into a rolling buffer (capped at
    ///    [`AUDIO_LID_ROLLING_BUFFER_CAP_SAMPLES`]). When at least
    ///    [`AUDIO_LID_WINDOW_SAMPLES`] of audio has accumulated *and*
    ///    [`AUDIO_LID_WINDOW_ADVANCE_SAMPLES`] of fresh audio has
    ///    landed since the previous window, classify the last
    ///    `AUDIO_LID_WINDOW_SAMPLES` and apply the commit/drift rules.
    /// 3. Commit rule: first window whose top-1 is `en` or `tl`
    ///    commits the corresponding route. Non-en/non-tl top-1 logs
    ///    "keep checking" and waits for the next window.
    /// 4. Drift rule: after commit, keep classifying. If
    ///    `MUNI_AUDIO_LID_DRIFT_CONSECUTIVE` windows in a row produce a
    ///    top-1 disagreeing with the committed route, fire the
    ///    `override_decision_*` mechanism (Whisper ↔ Deepgram) — but
    ///    only while `committed` is still false; once finalize locks
    ///    the route, the inner override no-ops.
    /// 5. Stop when the release watch fires, the chunk channel closes,
    ///    or the orchestrator has finalized the route.
    ///
    /// Feature 021 — parallel Gemini hybrid spawn. After the first
    /// window classifies, `should_spawn_audio_hybrid` decides
    /// whether to fire a rolling-window Gemini classify task in
    /// parallel. The task is spawned only when audio-LID is uncertain
    /// (weak English, `Other` label) or when the press has run past
    /// [`MIN_PRESS_DURATION_FOR_LATE_TAGLISH_RECOVERY_SAMPLES`]
    /// (long-press late-Tagalog catch). Clean English / clean Tagalog
    /// presses skip the hybrid task entirely so the per-press cost
    /// line item stays bounded.
    #[allow(clippy::too_many_arguments)]
    async fn run_audio_lid_pass(
        audio_lid_client: Arc<dyn AudioLidClassifier>,
        deepgram_client: Arc<DeepgramClient>,
        mut lid_chunks_rx: broadcast::Receiver<Vec<i16>>,
        // Plan 039 task 13 — per-press coordination bundle (decision cell +
        // notify, release signal, committed/aborted flags, hybrid-inflight
        // counter, drift-state atomics). Replaces the nine-deep arg stack.
        shared: PressShared,
        usage_tx: Option<mpsc::Sender<UsageRecord>>,
        bilingual_mode: BilingualModeFlag,
        whisper_client: Option<Arc<GroqWhisperClient>>,
        text_lid_secondary: Option<Arc<dyn TextLidClassifier>>,
        vad_detector: Option<Arc<dyn crate::vad::VadDetector>>,
        hybrid_audio_rx: broadcast::Receiver<Vec<i16>>,
        audio_hybrid_handle_slot: Arc<TokioMutex<Option<tauri::async_runtime::JoinHandle<()>>>>,
        // Feature 024 (backlog 0042) — streaming VAD factory + mirror
        // for Site D. Resolved once at the spawn site so the kill
        // switch is read deterministically before the task starts.
        streaming_vad_factory: Option<crate::vad::StreamingVadFactory>,
        stream_hybrid_enabled: bool,
        // Feature 025 (backlog 0046) — kill switch for the per-window
        // silence gate. Resolved once at the spawn site so the env-var
        // read is deterministic before the task starts. When false,
        // the gate clause in `should_fire_audio_lid_window` degenerates
        // to today's behavior.
        audio_lid_gate_enabled: bool,
        audio_hybrid_speech_mirror: Arc<TokioMutex<Vec<i16>>>,
        audio_lid_release_other_as_taglish: bool,
        audio_lid_hybrid_veto_drift: bool,
    ) {
        // Unpack the per-press bundle into locals so the (large) body below is
        // unchanged. Cheap Arc/watch clones; `shared` is retained so the two
        // `try_spawn_audio_hybrid` call sites can hand the same bundle onward.
        let decision = shared.decision.clone();
        let decision_notify = shared.decision_notify.clone();
        let release_tx = shared.release_tx.clone();
        let committed = shared.committed.clone();
        let aborted = shared.aborted.clone();
        let audio_hybrid_recent_text_lid_english =
            shared.audio_hybrid_recent_text_lid_english.clone();
        let audio_lid_drift_counter = shared.audio_lid_drift_counter.clone();
        let audio_lid_last_post_commit_was_other =
            shared.audio_lid_last_post_commit_was_other.clone();
        // Reset the shared drift counter at task entry — the
        // orchestrator allocated a fresh `Arc<AtomicUsize>` per press,
        // but resetting here is defensive (any future re-use of the
        // counter or per-process state leaks would be caught).
        audio_lid_drift_counter.store(0, Ordering::SeqCst);
        audio_lid_last_post_commit_was_other.store(false, Ordering::SeqCst);
        // Backlog 0052 — defensive reset mirroring the existing atomic
        // resets above. The orchestrator already allocates a fresh
        // `Arc<AtomicBool>` per press, so this is belt-and-suspenders.
        audio_hybrid_recent_text_lid_english.store(false, Ordering::SeqCst);
        // Bilingual-mode escape hatch: identical to the text-LID path
        // (backlog 0012). Manual override wins over local LID for the
        // same reasons it wins over remote LID.
        if bilingual_mode.is_enabled() {
            log::info!(
                target: "lid",
                "audio-LID: bilingual_mode enabled — skipping LID, routing to Whisper"
            );
            aborted.store(true, Ordering::SeqCst);
            deepgram_client.close().await;
            Self::set_decision(&decision, &decision_notify, RouterDecision::Whisper).await;
            return;
        }

        let drift_threshold = load_audio_lid_drift_consecutive();
        let release_fire_floor = load_audio_lid_release_drift_fire_floor();
        let provider_label_owned = audio_lid_client.provider_label().to_string();
        log::info!(
            target: "lid",
            "audio-LID armed (provider={}, window_samples={AUDIO_LID_WINDOW_SAMPLES}, advance_samples={AUDIO_LID_WINDOW_ADVANCE_SAMPLES}, drift_consecutive={drift_threshold}, release_drift_fire_floor={release_fire_floor}, release_other_as_taglish={audio_lid_release_other_as_taglish}, hybrid_veto_drift={audio_lid_hybrid_veto_drift})",
            provider_label_owned
        );

        // Release waiter for this pass's window loop (plan 039 task 13). A
        // dedicated `watch` receiver — the hybrid task the pass may spawn
        // subscribes its OWN receiver, so a single release fire wakes both.
        let mut release_rx = release_tx.subscribe();

        let mut rolling: Vec<i16> = Vec::with_capacity(AUDIO_LID_ROLLING_BUFFER_CAP_SAMPLES);
        let mut accumulated_since_last_window: usize = 0;
        let mut committed_route: Option<RouterDecision> = None;
        let mut consecutive_drift: usize = 0;
        let mut stream_closed = false;
        let mut first_window_done = false;
        // Feature 021 — wraps the resubscribed receiver so it can be
        // moved into the hybrid task at spawn time. `take()` after
        // spawn ensures we only ever consume it once.
        let mut hybrid_audio_rx_slot = Some(hybrid_audio_rx);
        // Feature 021 — `true` once `spawn_audio_hybrid_task`
        // has stored a handle in `audio_hybrid_handle_slot`. Read by
        // the long-press secondary-trigger check so we don't spawn
        // twice (once on uncertain-first-window, once on long-press).
        let mut hybrid_spawned: bool = false;
        // Feature 021 — `true` once the one-shot cross-reference
        // classify has fired. Used when the long-press trigger fires
        // on a press that's already routed to Whisper (drift override
        // can never flip back, so spawning the full rolling task
        // would waste compute). The one-shot still runs once per
        // press for telemetry/cross-reference value.
        let mut cross_reference_fired: bool = false;

        // Feature 025 (backlog 0046) — sibling streaming VAD for the
        // per-window silence gate. Constructed once per task spawn so
        // each press gets a fresh LSTM state, matching Site D's
        // per-task pattern. `None` when the kill switch is off OR the
        // factory wasn't installed at boot (all three streaming-VAD
        // switches off). When `None`, `gate_active` below is `false`
        // → the predicate degenerates to today's behavior.
        let mut audio_lid_gate_vad: Option<Box<dyn crate::vad::StreamingVadDetector>> =
            if audio_lid_gate_enabled {
                streaming_vad_factory.as_ref().map(|f| f())
            } else {
                None
            };
        let gate_active = audio_lid_gate_vad.is_some();
        // Counter for the gate predicate. Reset to 0 on any
        // speech-byte emission from the sibling VAD; incremented by
        // chunk.len() on any chunk that emits zero bytes. Initial 0
        // keeps the invariant clean — the first-window protection in
        // `should_fire_audio_lid_window` handles the case where the
        // press opens with silence independently.
        let mut samples_since_last_speech: usize = 0;
        // Per-press counters for the loop-exit summary log line.
        // Tracked regardless of `gate_active` so dogfood can compare
        // gate-on vs gate-off press distributions. Held inside the
        // RAII guard so the summary fires from `Drop` — including
        // when `finalize_auto_detect` aborts the task externally
        // (the common happy path).
        let mut summary = AudioLidPressSummaryGuard::new(gate_active);
        if gate_active {
            log::info!(
                target: "lid",
                "audio-LID silence gate armed (window_samples={AUDIO_LID_WINDOW_SAMPLES})"
            );
        }

        loop {
            // Stop entirely once the orchestrator has locked the route —
            // no further window can change anything.
            if committed.load(Ordering::SeqCst) {
                log::debug!(
                    target: "lid",
                    "audio-LID: route committed by orchestrator — exiting window loop"
                );
                return;
            }

            // Decide whether the next window is ready to fire.
            // Feature 025 — delegates to the pure
            // `should_fire_audio_lid_window` predicate so the gate's
            // branching logic is unit-testable in isolation.
            let window_ready = should_fire_audio_lid_window(
                first_window_done,
                accumulated_since_last_window,
                rolling.len(),
                samples_since_last_speech,
                gate_active,
            );

            // Feature 025 — gate-suppressed skip branch. Reached when
            // the advance cadence has elapsed AND the buffer has
            // enough samples AND the gate is active AND the candidate
            // window's worth of recent samples was all silence. Reset
            // `accumulated_since_last_window` so the 1 s cadence keeps
            // ticking forward — without this, a skipped window would
            // leave `accumulated` past the threshold and the next
            // chunk would cross it again immediately, producing a
            // tight skip-loop. Do NOT touch `first_window_done` or
            // `consecutive_drift` — they are preserved by virtue of
            // `apply_audio_lid_verdict` not being called.
            let cadence_elapsed_with_full_buffer = first_window_done
                && accumulated_since_last_window >= AUDIO_LID_WINDOW_ADVANCE_SAMPLES
                && rolling.len() >= AUDIO_LID_WINDOW_SAMPLES;
            if !window_ready && cadence_elapsed_with_full_buffer && gate_active {
                log::debug!(
                    target: "lid",
                    "audio-LID: window skipped (vad_silent, rolling_len={}, samples_since_speech={}, drift_preserved={})",
                    rolling.len(),
                    samples_since_last_speech,
                    consecutive_drift
                );
                summary.record_skipped();
                accumulated_since_last_window = 0;
            }

            if window_ready {
                let start = rolling.len() - AUDIO_LID_WINDOW_SAMPLES;
                let window: Vec<i16> = rolling[start..].to_vec();
                accumulated_since_last_window = 0;
                summary.record_classified();
                let is_first_window = !first_window_done;
                first_window_done = true;

                let verdict = match Self::classify_audio_window(
                    audio_lid_client.as_ref(),
                    &window,
                    usage_tx.as_ref(),
                )
                .await
                {
                    Ok(v) => v,
                    Err(()) => {
                        log::warn!(
                            target: "lid",
                            "audio-LID classify failed — continuing to next window"
                        );
                        if stream_closed {
                            break;
                        }
                        continue;
                    }
                };

                // Feature 021 — selective hybrid-spawn on first-window
                // uncertainty. Spawned exactly once per press; the
                // long-press secondary trigger below catches the
                // late-Tagalog case where the first window was
                // confidently English but the press is long enough
                // for code-switching to be plausible.
                if is_first_window
                    && !hybrid_spawned
                    && should_spawn_audio_hybrid(
                        &verdict.label,
                        verdict.p_en,
                        CONFIDENCE_TO_SKIP_HYBRID_TASK,
                    )
                {
                    if let Some(rx) = hybrid_audio_rx_slot.take() {
                        // Pass `rolling` as the hybrid task's initial
                        // buffer state so its first classify can sample
                        // the *leading* audio of the press (where
                        // leading-Tagalog code-switches like "Hindi ko
                        // alam exactly..." live). Without this, the
                        // hybrid's own buffer would only contain
                        // post-spawn audio, missing the leading
                        // content entirely.
                        Self::try_spawn_audio_hybrid(
                            "first-window-uncertain",
                            rx,
                            rolling.clone(),
                            whisper_client.as_ref(),
                            text_lid_secondary.as_ref(),
                            &shared,
                            &deepgram_client,
                            usage_tx.as_ref(),
                            vad_detector.as_ref(),
                            &audio_hybrid_handle_slot,
                            streaming_vad_factory.as_ref(),
                            stream_hybrid_enabled,
                            &audio_hybrid_speech_mirror,
                            &mut hybrid_spawned,
                        )
                        .await;
                        // If the spawn was vetoed (key resolution fail
                        // / dep missing), put the receiver back so a
                        // later trigger can still grab it. Failure
                        // path logs at error inside the helper.
                        if !hybrid_spawned {
                            // Receiver was consumed — we can't recover
                            // it now. Spawn won't retry this press;
                            // audio-LID alone handles the press.
                            log::warn!(
                                target: "lid",
                                "audio-LID hybrid spawn failed — receiver consumed; audio-LID continues alone"
                            );
                        }
                    } else {
                        log::debug!(
                            target: "lid",
                            "audio-LID hybrid skipped — receiver already consumed (no spawn possible)"
                        );
                    }
                }

                Self::apply_audio_lid_verdict(
                    &verdict,
                    &mut committed_route,
                    &mut consecutive_drift,
                    drift_threshold,
                    &deepgram_client,
                    &decision,
                    &decision_notify,
                    &committed,
                    &aborted,
                    &audio_lid_drift_counter,
                    &audio_lid_last_post_commit_was_other,
                    &audio_hybrid_recent_text_lid_english,
                    audio_lid_hybrid_veto_drift,
                )
                .await;
            }

            // Feature 021 — long-press late-Tagalog secondary trigger.
            // Two dispatch paths depending on the current routing
            // state:
            //
            //   * Route is None or Some(Deepgram): spawn the full
            //     rolling-classify task — the override may flip the
            //     route to Whisper if Gemini/Groq lands `taglish` or
            //     `tagalog` on a fresh slice.
            //
            //   * Route is Some(Whisper): override would be a no-op
            //     (the `deepgram → whisper` direction is the only
            //     one supported; flipping `whisper → deepgram` was
            //     permanently disabled post-2026-05-18 dogfood).
            //     Instead, fire a single one-shot cross-reference
            //     classify for telemetry — it lands a cost-accounted
            //     usage row + a log line showing the secondary's
            //     verdict, useful for offline analysis of routing
            //     accuracy without re-introducing the wasted rolling
            //     task observed in 2026-05-18 dogfood (4× classifies
            //     on a Whisper-committed press).
            if !hybrid_spawned
                && !cross_reference_fired
                && rolling.len() >= MIN_PRESS_DURATION_FOR_LATE_TAGLISH_RECOVERY_SAMPLES
            {
                match committed_route {
                    None | Some(RouterDecision::Deepgram) => {
                        if let Some(rx) = hybrid_audio_rx_slot.take() {
                            // Same as the first-window-uncertain spawn:
                            // seed the hybrid with the leading audio
                            // so its first classify can look at the
                            // start of the press, not just the rolling
                            // tail.
                            Self::try_spawn_audio_hybrid(
                                "long-press-late-taglish",
                                rx,
                                rolling.clone(),
                                whisper_client.as_ref(),
                                text_lid_secondary.as_ref(),
                                &shared,
                                &deepgram_client,
                                usage_tx.as_ref(),
                                vad_detector.as_ref(),
                                &audio_hybrid_handle_slot,
                                streaming_vad_factory.as_ref(),
                                stream_hybrid_enabled,
                                &audio_hybrid_speech_mirror,
                                &mut hybrid_spawned,
                            )
                            .await;
                        }
                    }
                    Some(RouterDecision::Whisper) => {
                        let start = rolling.len() - AUDIO_HYBRID_SLICE_SAMPLES;
                        let slice: Vec<i16> = rolling[start..].to_vec();
                        Self::fire_audio_hybrid_cross_reference(
                            slice,
                            whisper_client.as_ref(),
                            text_lid_secondary.as_ref(),
                            usage_tx.as_ref(),
                            &mut cross_reference_fired,
                        )
                        .await;
                    }
                }
            }

            if stream_closed {
                break;
            }

            tokio::select! {
                biased;
                () = released(&mut release_rx) => {
                    // Backlog 0048 — at-release stale drift commit.
                    // If the press ended with partial drift evidence
                    // (`consecutive_drift >= release_fire_floor`) on a
                    // Deepgram-committed route, fire the override
                    // anyway. Mid-press calibration
                    // (DEFAULT_AUDIO_LID_DRIFT_CONSECUTIVE) is
                    // independent — the at-release fire consumes
                    // evidence that would otherwise be discarded when
                    // the press terminates before the second
                    // consecutive disagreement lands.
                    //
                    // Side-effect sequence mirrors the mid-press
                    // FireOverrideToWhisper arm of
                    // `apply_audio_lid_verdict` (log + override +
                    // abort + close + mutate committed_route).
                    //
                    // Invariant: after a successful fire,
                    // `committed_route == Some(Whisper)`, which makes
                    // the two `committed_route.is_none()` fallback
                    // branches below unreachable on this code path.
                    // The fire requires `committed_route ==
                    // Some(Deepgram)` (so `first_window_done == true`
                    // by construction), so the
                    // `!first_window_done && committed_route.is_none()`
                    // branch is also unreachable post-fire.
                    let last_was_other = audio_lid_last_post_commit_was_other
                        .load(Ordering::SeqCst);
                    // Backlog 0052 — symmetric veto bit (AND'd with the
                    // env knob). Read once before the decide call; the
                    // probe log fires when the veto actually blocked a
                    // would-be fire so dogfood can grep for the event.
                    let hybrid_recent_english = audio_lid_hybrid_veto_drift
                        && audio_hybrid_recent_text_lid_english.load(Ordering::SeqCst);
                    if hybrid_recent_english
                        && matches!(committed_route, Some(RouterDecision::Deepgram))
                        && audio_lid_decide_release_action(
                            committed_route,
                            consecutive_drift,
                            release_fire_floor,
                            last_was_other,
                            audio_lid_release_other_as_taglish,
                            false,
                        ) == AudioLidReleaseAction::FireOverrideToWhisper
                    {
                        log::info!(
                            target: "lid",
                            "audio-LID release: stale-drift fire vetoed by hybrid text-LID (recent_english=true, drift={}, floor={}, last_was_other={}) — keeping route deepgram",
                            consecutive_drift,
                            release_fire_floor,
                            last_was_other,
                        );
                    }
                    match audio_lid_decide_release_action(
                        committed_route,
                        consecutive_drift,
                        release_fire_floor,
                        last_was_other,
                        audio_lid_release_other_as_taglish,
                        hybrid_recent_english,
                    ) {
                        AudioLidReleaseAction::FireOverrideToWhisper => {
                            log::info!(
                                target: "lid",
                                "audio-LID release: stale drift ({}/floor={}, last_was_other={}) — firing override deepgram → whisper",
                                consecutive_drift,
                                release_fire_floor,
                                last_was_other,
                            );
                            let flipped = Self::override_decision_deepgram_to_whisper(
                                &decision,
                                &decision_notify,
                                &committed,
                            )
                            .await;
                            if flipped {
                                aborted.store(true, Ordering::SeqCst);
                                deepgram_client.close().await;
                                log::info!(
                                    target: "lid",
                                    "audio-LID release override applied: deepgram → whisper"
                                );
                                committed_route = Some(RouterDecision::Whisper);
                                consecutive_drift = 0;
                                audio_lid_drift_counter.store(0, Ordering::SeqCst);
                            }
                        }
                        AudioLidReleaseAction::NoOp => {}
                    }
                    stream_closed = true;
                    // If we never managed a single classify before
                    // release, fall back to Whisper — the safe path
                    // (multilingual) per feature 003's rule.
                    if !first_window_done && committed_route.is_none() {
                        log::warn!(
                            target: "lid",
                            "audio-LID: press ended before first window filled (have {} samples) — falling back to Whisper",
                            rolling.len()
                        );
                        aborted.store(true, Ordering::SeqCst);
                        deepgram_client.close().await;
                        Self::set_decision(&decision, &decision_notify, RouterDecision::Whisper).await;
                        return;
                    }
                    // We have at least one verdict. If commit succeeded
                    // it already set the decision; if it didn't (e.g.
                    // every window said `ko`), trust the freshest
                    // verdict's `label` instead — the routing layer's
                    // "keep checking" loop is over.
                    if committed_route.is_none() {
                        // No `en`/`tl` window seen at all. Safe default
                        // is Whisper (multilingual).
                        log::warn!(
                            target: "lid",
                            "audio-LID: release with no en/tl verdict observed — falling back to Whisper"
                        );
                        aborted.store(true, Ordering::SeqCst);
                        deepgram_client.close().await;
                        Self::set_decision(&decision, &decision_notify, RouterDecision::Whisper).await;
                        return;
                    }
                }
                c = lid_chunks_rx.recv() => match c {
                    Ok(chunk) => {
                        // Feature 025 — sidecar VAD signal. The output
                        // buffer is discarded; only its emit/no-emit
                        // state matters. A `None` instance means the
                        // gate is off — counter stays at 0, predicate
                        // degenerates to today's behavior.
                        if let Some(vad) = audio_lid_gate_vad.as_mut() {
                            let mut sink: Vec<i16> = Vec::with_capacity(chunk.len());
                            vad.process_chunk(&chunk, &mut sink).await;
                            if sink.is_empty() {
                                samples_since_last_speech =
                                    samples_since_last_speech.saturating_add(chunk.len());
                            } else {
                                samples_since_last_speech = 0;
                            }
                        }
                        rolling.extend_from_slice(&chunk);
                        accumulated_since_last_window += chunk.len();
                        // Cap the buffer at ROLLING_BUFFER_CAP samples.
                        if rolling.len() > AUDIO_LID_ROLLING_BUFFER_CAP_SAMPLES {
                            let drop = rolling.len() - AUDIO_LID_ROLLING_BUFFER_CAP_SAMPLES;
                            rolling.drain(..drop);
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        log::warn!(
                            target: "lid",
                            "audio-LID chunk channel lagged by {n} — windowing freshness compromised"
                        );
                        continue;
                    }
                    Err(RecvError::Closed) => {
                        stream_closed = true;
                    }
                },
            }
        }
        // Feature 025 — per-press summary fires from
        // `AudioLidPressSummaryGuard::drop` when `summary` goes out of
        // scope here. Survives external `lid_handle.abort()` too — the
        // task cancellation drops locals at the next `.await`, running
        // the guard's `Drop` impl.
    }

    /// Feature 020 — apply a single window's verdict to the route
    /// cell. Side-effecting wrapper around the pure
    /// [`audio_lid_decide_action`] state machine: dispatches the
    /// computed action against the real Deepgram client + decision
    /// cell, and mutates the caller-supplied windowing state in place.
    ///
    /// Pulled out of [`Self::run_audio_lid_pass`] so the commit/drift
    /// rules are testable through `audio_lid_decide_action` alone —
    /// without a real `WhisperContext` or live Deepgram socket.
    #[allow(clippy::too_many_arguments)]
    async fn apply_audio_lid_verdict(
        verdict: &AudioLidVerdict,
        committed_route: &mut Option<RouterDecision>,
        consecutive_drift: &mut usize,
        drift_threshold: usize,
        deepgram_client: &Arc<DeepgramClient>,
        decision: &Arc<TokioMutex<Option<RouterDecision>>>,
        decision_notify: &Arc<Notify>,
        committed: &Arc<AtomicBool>,
        aborted: &Arc<AtomicBool>,
        // Backlog 0048 — shared mirror of the drift counter. Written
        // alongside every mutation of the local `consecutive_drift`
        // so `finalize_auto_detect` can read it at release.
        drift_counter_mirror: &Arc<AtomicUsize>,
        // Backlog 0048 v2 — shared "last post-commit verdict was Other"
        // bit. Set on IgnoreNoise; cleared on every other post-commit
        // action. Pre-commit (KeepChecking) leaves it unchanged because
        // the rule only applies to post-commit verdicts.
        last_post_commit_was_other_mirror: &Arc<AtomicBool>,
        // Backlog 0052 — shared "hybrid recently saw English" bit.
        // Set by `spawn_audio_hybrid_inner_classify` on an explicit English
        // verdict only (plan 039 task 20 — `Other` no longer arms it);
        // never cleared. Read here (AND'd with the env knob) to
        // block the mid-press drift `FireOverrideToWhisper` action by
        // downgrading it to `Agree` in `audio_lid_decide_action`.
        audio_hybrid_recent_text_lid_english_mirror: &Arc<AtomicBool>,
        // Backlog 0052 — env-knob resolved once at session construction.
        // When `false`, the hybrid veto is disabled and behavior matches
        // feat/021's asymmetric direction.
        audio_lid_hybrid_veto_drift: bool,
    ) {
        log::info!(
            target: "lid",
            "audio-LID window: top1={} p={:.2} p_en={:.2} p_tl={:.2} label={} latency={:.0}ms",
            verdict.top1_lang,
            verdict.top1_prob,
            verdict.p_en,
            verdict.p_tl,
            verdict.label.as_log_str(),
            verdict.latency_ms,
        );

        let hybrid_recent_english = audio_lid_hybrid_veto_drift
            && audio_hybrid_recent_text_lid_english_mirror.load(Ordering::SeqCst);
        // Backlog 0052 — surface the vetoed-fire event before dispatching
        // the (downgraded) decision so dogfood log scans can grep for
        // "vetoed by hybrid text-LID". Cheap probe: matches the proposed
        // route + the would-be threshold crossing without recomputing
        // the full state machine.
        if hybrid_recent_english
            && matches!(committed_route, Some(RouterDecision::Deepgram))
            && matches!(
                audio_lid_proposed_route(&verdict.label),
                Some(RouterDecision::Whisper)
            )
            && *consecutive_drift + 1 >= drift_threshold
        {
            log::info!(
                target: "lid",
                "audio-LID drift: threshold {drift_threshold} reached but vetoed by hybrid text-LID (recent_english=true) — keeping route deepgram"
            );
        }

        let action = audio_lid_decide_action(
            &verdict.label,
            verdict.p_en,
            *committed_route,
            *consecutive_drift,
            drift_threshold,
            hybrid_recent_english,
        );

        match action {
            AudioLidAction::KeepChecking => {
                log::info!(
                    target: "lid",
                    "audio-LID: label={} top1={} p_en={:.2} — pre-commit (Other label or low-confidence English), keep checking",
                    verdict.label.as_log_str(),
                    verdict.top1_lang,
                    verdict.p_en,
                );
            }
            AudioLidAction::Commit(route) => {
                *committed_route = Some(route);
                *consecutive_drift = 0;
                drift_counter_mirror.store(0, Ordering::SeqCst);
                last_post_commit_was_other_mirror.store(false, Ordering::SeqCst);
                if matches!(route, RouterDecision::Whisper) {
                    aborted.store(true, Ordering::SeqCst);
                    deepgram_client.close().await;
                }
                log::info!(
                    target: "lid",
                    "audio-LID commit: route={} (label={} top1={})",
                    route.as_log_str(),
                    verdict.label.as_log_str(),
                    verdict.top1_lang
                );
                Self::set_decision(decision, decision_notify, route).await;
            }
            AudioLidAction::Agree => {
                if *consecutive_drift > 0 {
                    log::debug!(
                        target: "lid",
                        "audio-LID drift: agreement — reset counter"
                    );
                }
                *consecutive_drift = 0;
                drift_counter_mirror.store(0, Ordering::SeqCst);
                last_post_commit_was_other_mirror.store(false, Ordering::SeqCst);
            }
            AudioLidAction::IgnoreNoise => {
                // Post-commit non-en/non-tl: preserve drift counter so
                // a mid-press pause doesn't erase accumulated drift
                // evidence (dogfood 2026-05-18 fix). No mirror write
                // for the drift counter — the value is unchanged.
                //
                // Backlog 0048 v2 — record that the latest classified
                // verdict was Other. `finalize_auto_detect` consumes
                // this at release to handle the whisper-tiny
                // hallucination case (gotcha #4) where the Tagalog
                // tail lands as `id`/`ru`/`es` with p_tl below the
                // taglish floor.
                last_post_commit_was_other_mirror.store(true, Ordering::SeqCst);
                log::debug!(
                    target: "lid",
                    "audio-LID drift: noise window (label={}) — drift counter preserved at {}",
                    verdict.label.as_log_str(),
                    *consecutive_drift,
                );
            }
            AudioLidAction::IncrementDrift { new_count } => {
                *consecutive_drift = new_count;
                drift_counter_mirror.store(new_count, Ordering::SeqCst);
                last_post_commit_was_other_mirror.store(false, Ordering::SeqCst);
                log::info!(
                    target: "lid",
                    "audio-LID drift: window disagrees with committed route ({}/{})",
                    new_count,
                    drift_threshold,
                );
            }
            AudioLidAction::FireOverrideToWhisper => {
                log::info!(
                    target: "lid",
                    "audio-LID drift: threshold {drift_threshold} reached — firing override deepgram → whisper"
                );
                let flipped = Self::override_decision_deepgram_to_whisper(
                    decision,
                    decision_notify,
                    committed,
                )
                .await;
                if flipped {
                    aborted.store(true, Ordering::SeqCst);
                    deepgram_client.close().await;
                    log::info!(
                        target: "lid",
                        "audio-LID drift override applied: deepgram → whisper"
                    );
                    *committed_route = Some(RouterDecision::Whisper);
                }
                *consecutive_drift = 0;
                drift_counter_mirror.store(0, Ordering::SeqCst);
                last_post_commit_was_other_mirror.store(false, Ordering::SeqCst);
            }
        }
    }

    /// Feature 020 — wrap a single audio-LID classify call, emit the
    /// usage record, and surface the verdict to the windowing loop.
    /// Mirrors [`Self::classify_text_only`]'s shape (UsageRecord write,
    /// `[lid]` log target) so dashboards and dev terminals stay
    /// consistent.
    async fn classify_audio_window(
        lid_client: &dyn AudioLidClassifier,
        samples: &[i16],
        usage_tx: Option<&mpsc::Sender<UsageRecord>>,
    ) -> Result<AudioLidVerdict, ()> {
        let started = Instant::now();
        let result = lid_client.classify(samples).await;
        let provider = lid_client.provider_label().to_string();
        match result {
            Ok(verdict) => {
                let elapsed = started.elapsed();
                log::info!(
                    target: "lid",
                    "audio-LID = {} via {provider} (classify {:.0} ms) top1={} p_en={:.2} p_tl={:.2}",
                    verdict.label.as_log_str(),
                    verdict.latency_ms,
                    verdict.top1_lang,
                    verdict.p_en,
                    verdict.p_tl
                );
                if let Some(tx) = usage_tx {
                    let (provider_slug, model_slug) = split_lid_provider_label(&provider);
                    try_send_drop_oldest(
                        tx,
                        UsageRecord {
                            provider: provider_slug.into(),
                            model: model_slug.into(),
                            call_kind: crate::pricing::CALL_KIND_LID.into(),
                            audio_seconds: Some(samples.len() as f64 / TARGET_SAMPLE_RATE as f64),
                            input_tokens: None,
                            output_tokens: None,
                            latency_ms: Some(elapsed.as_millis() as i64),
                            status: "ok".into(),
                            request_id: None,
                            session_id: None,
                            created_at_unix: unix_seconds_now(),
                        },
                    );
                }
                Ok(verdict)
            }
            Err(err) => {
                log::warn!(
                    target: "lid",
                    "audio-LID classifier failed via {provider} in {} ms: {}",
                    started.elapsed().as_millis(),
                    err.user_message()
                );
                Err(())
            }
        }
    }

    /// Feature 021 — helper that owns the spawn-prerequisite checks
    /// for the audio-LID hybrid task. Pulled out of
    /// [`Self::run_audio_lid_pass`] so the two call sites (first-window
    /// uncertainty + long-press late-Tagalog) share the same prereq
    /// + key-resolution + spawn dispatch.
    ///
    /// On a successful spawn, stores the handle in
    /// `audio_hybrid_handle_slot` and flips `*spawned` to `true`.
    /// On any spawn precondition failure (missing whisper client,
    /// missing secondary, key resolution error), logs and leaves
    /// `*spawned` unchanged — audio-LID alone handles the press.
    ///
    /// See `.claude/feature_plans/021_hybrid_audio_lid_with_gemini_parallel_text_lid.md`
    /// and `.claude/learned/002_deepgram_alone_is_not_enough_for_taglish.md`
    /// for the asymmetric-override rationale.
    #[allow(clippy::too_many_arguments)]
    async fn try_spawn_audio_hybrid(
        trigger_label: &str,
        audio_rx: broadcast::Receiver<Vec<i16>>,
        initial_buffer: Vec<i16>,
        whisper_client: Option<&Arc<GroqWhisperClient>>,
        text_lid_secondary: Option<&Arc<dyn TextLidClassifier>>,
        // Plan 039 task 13 — per-press coordination bundle (same one the pass
        // holds). The inner classify tasks read/write its hybrid + drift state.
        shared: &PressShared,
        deepgram_client: &Arc<DeepgramClient>,
        usage_tx: Option<&mpsc::Sender<UsageRecord>>,
        vad_detector: Option<&Arc<dyn crate::vad::VadDetector>>,
        audio_hybrid_handle_slot: &Arc<TokioMutex<Option<tauri::async_runtime::JoinHandle<()>>>>,
        // Feature 024 (backlog 0042) — streaming VAD factory + mirror.
        // Threaded through to `spawn_audio_hybrid_task` for Site D.
        streaming_vad_factory: Option<&crate::vad::StreamingVadFactory>,
        stream_hybrid_enabled: bool,
        audio_hybrid_speech_mirror: &Arc<TokioMutex<Vec<i16>>>,
        spawned: &mut bool,
    ) {
        // The coordination bundle flows straight through to
        // `spawn_audio_hybrid_task`; this helper only gates on the spawn
        // prerequisites (whisper client / secondary LID / key resolution).
        let Some(whisper) = whisper_client.cloned() else {
            log::debug!(
                target: "lid",
                "audio-LID hybrid skipped ({trigger_label}): whisper client unavailable"
            );
            return;
        };
        let Some(secondary) = text_lid_secondary.cloned() else {
            log::debug!(
                target: "lid",
                "audio-LID hybrid skipped ({trigger_label}): text_lid_secondary unavailable (MUNI_LID_AUDIO_HYBRID=false or boot init failed)"
            );
            return;
        };
        // Mid-press read on the hybrid spawn — cached to keep the press free
        // of a keychain IPC (plan 039 task 17). Env override stays live;
        // keychain layer is invalidated on `secrets://changed`.
        let whisper_api_key = match secrets::get_cached(secrets::GROQ_ACCOUNT) {
            Ok(k) => k,
            Err(err) => {
                log::error!(
                    target: "lid",
                    "audio-LID hybrid skipped ({trigger_label}): Groq key unavailable: {}",
                    err.user_message()
                );
                return;
            }
        };

        let initial_samples = initial_buffer.len();
        let handle = Self::spawn_audio_hybrid_task(
            audio_rx,
            initial_buffer,
            whisper,
            secondary,
            whisper_api_key,
            shared.clone(),
            deepgram_client.clone(),
            usage_tx.cloned(),
            vad_detector.cloned(),
            streaming_vad_factory.cloned(),
            stream_hybrid_enabled,
            audio_hybrid_speech_mirror.clone(),
        );
        *audio_hybrid_handle_slot.lock().await = Some(handle);
        *spawned = true;
        log::info!(
            target: "lid",
            "audio-LID hybrid spawned ({trigger_label}, initial_buffer={initial_samples} samples)"
        );
    }

    /// Feature 021 — one-shot cross-reference classify. Fired by the
    /// long-press trigger when audio-LID has already committed
    /// Whisper, so the rolling-classify task would only burn cost
    /// (the `whisper → deepgram` override direction was permanently
    /// disabled post-2026-05-18 dogfood). Spawns a single background
    /// task that transcribes the supplied slice via Groq Whisper and
    /// classifies the transcript via the secondary text-LID — same
    /// helpers the rolling task uses, so cost telemetry is written
    /// the same way. No mutation of the routing cell; the verdict is
    /// observation-only.
    ///
    /// Spawning the work as a detached task (rather than awaiting it
    /// inline) keeps the audio-LID windowing loop's cadence intact —
    /// a slow Groq classify must not starve the next window's
    /// classify. `committed` is not checked here because the press
    /// is already routed; the worst case if release lands mid-task is
    /// the result hits the log slightly after the paste, which is
    /// fine for a telemetry-only path. `finalize_auto_detect`'s LID
    /// handle abort doesn't reach this task (it's spawned bare), so
    /// callers should rely on the natural completion of the HTTP
    /// calls — both have their own request-side timeouts.
    async fn fire_audio_hybrid_cross_reference(
        slice: Vec<i16>,
        whisper_client: Option<&Arc<GroqWhisperClient>>,
        text_lid_secondary: Option<&Arc<dyn TextLidClassifier>>,
        usage_tx: Option<&mpsc::Sender<UsageRecord>>,
        fired: &mut bool,
    ) {
        let Some(whisper) = whisper_client.cloned() else {
            log::debug!(
                target: "lid",
                "audio-LID hybrid cross-reference skipped: whisper client unavailable"
            );
            return;
        };
        let Some(secondary) = text_lid_secondary.cloned() else {
            log::debug!(
                target: "lid",
                "audio-LID hybrid cross-reference skipped: text_lid_secondary unavailable"
            );
            return;
        };
        // Mid-press read on the hybrid cross-reference — cached to keep the
        // press free of a keychain IPC (plan 039 task 17). Env override stays
        // live; keychain layer is invalidated on `secrets://changed`.
        let whisper_api_key = match secrets::get_cached(secrets::GROQ_ACCOUNT) {
            Ok(k) => k,
            Err(err) => {
                log::error!(
                    target: "lid",
                    "audio-LID hybrid cross-reference skipped: Groq key unavailable: {}",
                    err.user_message()
                );
                return;
            }
        };

        let usage_for_task = usage_tx.cloned();
        log::info!(
            target: "lid",
            "audio-LID hybrid cross-reference fired (route already whisper; one-shot, no override)"
        );
        tauri::async_runtime::spawn(async move {
            let transcript = match Self::transcribe_for_lid(
                "audio-hybrid-xref",
                whisper.as_ref(),
                &slice,
                &whisper_api_key,
                usage_for_task.as_ref(),
            )
            .await
            {
                Ok(t) => t,
                Err(()) => return,
            };
            let _ = Self::classify_text_only(
                "audio-hybrid-xref",
                secondary.as_ref(),
                &transcript,
                usage_for_task.as_ref(),
            )
            .await;
            // No override path — this is observation-only. The
            // verdict shows up in the `audio-hybrid-xref text-LID = …`
            // log line written by `classify_text_only`.
        });
        *fired = true;
    }

    /// Feature 021 — rolling-window audio-LID-side parallel hybrid
    /// task. Spawned by [`Self::try_spawn_audio_hybrid`] when
    /// audio-LID's first window is uncertain, or by the long-press
    /// secondary trigger when the press has crossed
    /// [`MIN_PRESS_DURATION_FOR_LATE_TAGLISH_RECOVERY_SAMPLES`] AND
    /// the route is still None / Deepgram. (When the long-press
    /// trigger fires on a Whisper-committed route, the orchestrator
    /// fires a one-shot cross-reference classify via
    /// [`Self::fire_audio_hybrid_cross_reference`] instead — no
    /// rolling task.)
    ///
    /// **First-fire** spawns TWO parallel classify tasks (added
    /// 2026-05-18 round 4): one on the *leading* 3 s of buffered
    /// audio (catches "leading-Tagalog" presses like "Hindi ko alam
    /// exactly...") and one on the *trailing* 3 s (catches
    /// "late-Tagalog" presses like "So basically, mag-out tayo...").
    /// Trailing is deduped (skipped) when the buffer is exactly
    /// `AUDIO_HYBRID_SLICE_SAMPLES` long — both slices would be
    /// byte-identical and the second classify would just burn API
    /// quota for an identical Whisper transcript.
    ///
    /// **Subsequent fires** (every `AUDIO_HYBRID_CLASSIFY_INTERVAL_SAMPLES`
    /// of fresh audio) snapshot the most-recent `AUDIO_HYBRID_SLICE_SAMPLES`
    /// of the rolling buffer only. Each classify transcribes via Groq
    /// Whisper, classifies the transcript via the secondary text-LID,
    /// and on a `Tagalog`/`Taglish` verdict fires
    /// [`Self::override_or_commit_to_whisper_via_hybrid`] to flip
    /// (or pre-empt) the route to Whisper — handles both the
    /// already-committed-Deepgram and the still-uncommitted (`None`)
    /// cases atomically.
    ///
    /// Asymmetric override direction: only `deepgram → whisper` is
    /// supported (the inverse was permanently disabled in the
    /// post-2026-05-18 dogfood architectural decision — the Deepgram
    /// socket is torn down at Whisper-commit time, so flipping back
    /// leaves no audio producer for either backend). An `English`
    /// verdict against a Whisper-committed press is therefore a
    /// silent no-op.
    ///
    /// Nested-spawn pattern: each classify pass runs as its own
    /// inner task so a slow Whisper or text-LID reply doesn't
    /// starve the outer rolling-buffer loop. Inner tasks are
    /// fire-and-forget; the `committed` AtomicBool sentinel guards
    /// every mutation point so a late reply against a routed press
    /// is no-op'd.
    ///
    /// Spawn one nested classify task for a single slice. Used by
    /// [`Self::spawn_audio_hybrid_task`]'s main loop to fire the
    /// leading + trailing parallel pair on the first iteration and
    /// the rolling slice on subsequent iterations.
    ///
    /// `classify_label` is a `&'static str` so it can be cheaply
    /// captured by the spawned `async move` block. Use one of
    /// `"audio-hybrid-leading"`, `"audio-hybrid-trailing"`, or
    /// `"audio-hybrid-rolling"` so dogfood log scans can tell the
    /// three sources apart in the same press.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    async fn spawn_audio_hybrid_inner_classify(
        classify_label: &'static str,
        slice: Vec<i16>,
        whisper_client: Arc<GroqWhisperClient>,
        secondary_lid: Arc<dyn TextLidClassifier>,
        whisper_api_key: String,
        decision: Arc<TokioMutex<Option<RouterDecision>>>,
        decision_notify: Arc<Notify>,
        committed: Arc<AtomicBool>,
        forwarder_aborted: Arc<AtomicBool>,
        deepgram_client: Arc<DeepgramClient>,
        usage_tx: Option<mpsc::Sender<UsageRecord>>,
        vad_detector: Option<Arc<dyn crate::vad::VadDetector>>,
        audio_hybrid_inflight: Arc<AtomicUsize>,
        // Backlog 0052 — shared per-press bit. The English/Other arm
        // below stores `true` to arm the symmetric drift veto for the
        // rest of the press. Never cleared; mid-press readers in
        // `apply_audio_lid_verdict` and at-release readers in
        // `run_audio_lid_pass` / `finalize_auto_detect` consult it
        // (AND'd with the env knob) to downgrade drift fires.
        audio_hybrid_recent_text_lid_english: Arc<AtomicBool>,
    ) {
        // feat/022 Gate 2 — silent slice short-circuit. Pre-slice
        // amplitude gate so an ambient-noise-only press doesn't burn
        // a slice transcribe + classify pair (currently ~6 Groq calls
        // per silent press on the rolling fire cadence). Covers all
        // three callers (`audio-hybrid-leading`, `audio-hybrid-trailing`,
        // `audio-hybrid-rolling`). Sits ABOVE the inflight increment so
        // gated slices don't show up as in-flight to the release-path
        // wait — they were never going to land a verdict.
        if is_silent_slice(&slice) {
            let slice_peak: u16 = slice.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
            log::debug!(
                target: "lid",
                "audio-hybrid {classify_label}: slice peak {slice_peak} ≤ {} — skipping classify",
                SILENCED_PEAK_THRESHOLD
            );
            return;
        }

        // Feature 023 (backlog 0040) Gate 2.5 — content-aware VAD pass.
        // Catches ambient-silent slices whose peak slipped past the
        // amplitude gate. Sits ABOVE the inflight increment for the
        // same reason: gated slices were never going to land a verdict
        // and shouldn't show up as in-flight to the release-path wait.
        // Fails open per the trait contract.
        if let Some(vad) = vad_detector.as_ref() {
            if !vad.predict_speech(&slice).await {
                log::info!(
                    target: "lid",
                    "audio-hybrid {classify_label}: VAD detected no speech in slice (samples={}, vad={}) — skipping transcribe + classify",
                    slice.len(),
                    vad.provider_label()
                );
                return;
            }
        }

        // Round-6 inflight counter (B3): increment immediately so the
        // caller's release-path wait sees us in flight. The Drop
        // guard handles decrement + notify_waiters so panic, abort,
        // and normal-return all clear the counter cleanly. Without
        // this guard, an aborted inner task (e.g. via
        // `gemini_hybrid_handle.abort()` on release) would leak an
        // inflight slot and the next press's release wait would
        // either time out or short-circuit incorrectly.
        audio_hybrid_inflight.fetch_add(1, Ordering::SeqCst);
        tauri::async_runtime::spawn(async move {
            struct InflightGuard {
                counter: Arc<AtomicUsize>,
                notify: Arc<Notify>,
            }
            impl Drop for InflightGuard {
                fn drop(&mut self) {
                    self.counter.fetch_sub(1, Ordering::SeqCst);
                    // Wake any waiter on the release path. Idempotent
                    // against an already-woken waiter (Tokio's Notify
                    // semantics) and harmless when no waiter is
                    // registered.
                    self.notify.notify_waiters();
                }
            }
            let _inflight_guard = InflightGuard {
                counter: audio_hybrid_inflight.clone(),
                notify: decision_notify.clone(),
            };

            if committed.load(Ordering::SeqCst) {
                return;
            }
            // Feature 021 round-4 fix 2026-05-18 — pad the slice with
            // a short trailing silence so Whisper sees a
            // "complete utterance" boundary. Discourages Whisper from
            // truncating short multilingual slices to the leading
            // English prefix (e.g. "The thing is" from "The thing is,
            // hindi pa fully tested yung change.").
            //
            // Round 6 revert (2026-05-18): non-turbo `whisper-large-v3`
            // was tried as a complementary fix but its ~2× latency
            // (~700 ms median classify vs ~300 ms) pushed the verdict
            // past release on short presses, cancelling its accuracy
            // win. Reverted to turbo + padding; release-trigger wait
            // (the `audio_hybrid_inflight` mechanism) is the actual
            // fix for the timing race.
            let padded_slice =
                pad_with_trailing_silence(&slice, AUDIO_HYBRID_TRAILING_SILENCE_SAMPLES);
            let transcript = match Self::transcribe_for_lid(
                classify_label,
                whisper_client.as_ref(),
                &padded_slice,
                &whisper_api_key,
                usage_tx.as_ref(),
            )
            .await
            {
                Ok(t) => t,
                Err(()) => return,
            };
            // Feature 023 (backlog 0040) — defense-in-depth allowlist
            // for known Whisper hallucinations. Catches the case where
            // the amplitude + VAD gates both let a silent slice
            // through and Whisper hallucinates a confident outro
            // phrase (`Thank you.`, etc). Skipping classify saves one
            // Groq call per false-positive slice; the trailing audio
            // is observation-only at this layer (no override path).
            let trimmed = transcript.trim();
            if matches_known_hallucination(trimmed) {
                log::info!(
                    target: "lid",
                    "audio-hybrid {classify_label}: slice transcript matched known hallucination (\"{trimmed}\") — skipping classify"
                );
                return;
            }
            let result = Self::classify_text_only(
                classify_label,
                secondary_lid.as_ref(),
                &transcript,
                usage_tx.as_ref(),
            )
            .await;
            if committed.load(Ordering::SeqCst) {
                log::debug!(
                    target: "lid",
                    "audio-LID hybrid override late ({classify_label}) — orchestrator already routed"
                );
                return;
            }
            match result {
                Ok(LidLabel::Tagalog) | Ok(LidLabel::Taglish) => {
                    // Pre-commit-aware override: handles both the
                    // `*g == None` case (pre-empts audio-LID's
                    // still-pending commit) and the
                    // `*g == Some(Deepgram)` case (flips the
                    // already-committed route). See
                    // `override_or_commit_to_whisper_via_hybrid` for
                    // the full rationale.
                    let flipped = Self::override_or_commit_to_whisper_via_hybrid(
                        &decision,
                        &decision_notify,
                        &committed,
                    )
                    .await;
                    if flipped {
                        forwarder_aborted.store(true, Ordering::SeqCst);
                        deepgram_client.close().await;
                        log::info!(
                            target: "lid",
                            "audio-LID hybrid commit/override applied ({classify_label} slice): → whisper"
                        );
                    } else {
                        log::debug!(
                            target: "lid",
                            "audio-LID hybrid commit/override no-op ({classify_label}) — route already Whisper or finalize raced"
                        );
                    }
                }
                Ok(label @ (LidLabel::English | LidLabel::Other(_))) => {
                    // Backlog 0052 — symmetric veto write site. Arms the
                    // drift-override veto for the rest of this press. The
                    // direct-override direction is still disabled (the
                    // forwarder + Deepgram socket lifecycle invariant from
                    // feat/021 dogfood holds), but the veto-only direction
                    // is safe because it never flips the route — it just
                    // prevents whisper-tiny probability noise from
                    // overriding the content-aware verdict.
                    //
                    // Plan 039 task 20 — arm ONLY on an explicit English
                    // verdict. `Other` is treated identically to
                    // non-English (text_lid.rs `LidLabel::Other` contract),
                    // so it must not suppress a legitimate drift override to
                    // Whisper; pinning an ambiguous press to Deepgram on a
                    // catch-all token was the bug this closes.
                    if hybrid_verdict_arms_drift_veto(&label) {
                        audio_hybrid_recent_text_lid_english.store(true, Ordering::SeqCst);
                        log::debug!(
                            target: "lid",
                            "audio-LID hybrid ({classify_label}): english verdict — armed drift veto"
                        );
                    } else {
                        log::debug!(
                            target: "lid",
                            "audio-LID hybrid ({classify_label}): other verdict (\"{}\") — drift veto NOT armed (treated as non-english)",
                            label.as_log_str()
                        );
                    }
                }
                Err(()) => {
                    // already logged inside classify_text_only
                }
            }
        });
    }

    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    fn spawn_audio_hybrid_task(
        mut audio_rx: broadcast::Receiver<Vec<i16>>,
        initial_buffer: Vec<i16>,
        whisper_client: Arc<GroqWhisperClient>,
        secondary_lid: Arc<dyn TextLidClassifier>,
        whisper_api_key: String,
        // Plan 039 task 13 — per-press coordination bundle.
        shared: PressShared,
        deepgram_client: Arc<DeepgramClient>,
        usage_tx: Option<mpsc::Sender<UsageRecord>>,
        vad_detector: Option<Arc<dyn crate::vad::VadDetector>>,
        // Feature 024 (backlog 0042) — streaming VAD factory + mirror
        // slot. The factory is `Some` when at least one streaming-VAD
        // kill switch is on at boot; this task constructs a fresh
        // per-stream detector ONLY when the hybrid-side switch (passed
        // in via `stream_hybrid_enabled`) is also on. The mirror is
        // populated frame-by-frame by Site D as the press accumulates;
        // read at release by `resolve_trimmed_release_buffer`.
        streaming_vad_factory: Option<crate::vad::StreamingVadFactory>,
        stream_hybrid_enabled: bool,
        audio_hybrid_speech_mirror: Arc<TokioMutex<Vec<i16>>>,
    ) -> tauri::async_runtime::JoinHandle<()> {
        // Unpack the bundle into the owned locals the moved closure below uses.
        let PressShared {
            decision,
            decision_notify,
            release_tx,
            committed,
            aborted: forwarder_aborted,
            audio_hybrid_inflight,
            audio_hybrid_recent_text_lid_english,
            ..
        } = shared;
        tauri::async_runtime::spawn(async move {
            // Own release waiter (plan 039 task 13) — subscribed independently of
            // the audio-LID pass's receiver so one release fire wakes both.
            let mut release_rx = release_tx.subscribe();
            log::info!(
                target: "lid",
                "audio-LID hybrid armed (slice_samples={AUDIO_HYBRID_SLICE_SAMPLES}, interval_samples={AUDIO_HYBRID_CLASSIFY_INTERVAL_SAMPLES}, seed={} samples)",
                initial_buffer.len()
            );
            // Seed the hybrid buffer with the audio that was already
            // captured before this task spawned. This is what lets
            // the FIRST classify look at the *leading* audio of the
            // press (where leading-Tagalog like "Hindi ko alam exactly,
            // but I think..." lives) instead of only the most-recent
            // 3 s window. Subsequent classifies fall back to
            // rolling-most-recent via the cadence logic below.
            let mut hybrid_buffer: Vec<i16> = if initial_buffer.is_empty() {
                Vec::with_capacity(AUDIO_HYBRID_BUFFER_CAP_SAMPLES)
            } else {
                let mut buf =
                    Vec::with_capacity(AUDIO_HYBRID_BUFFER_CAP_SAMPLES.max(initial_buffer.len()));
                buf.extend_from_slice(&initial_buffer);
                // Cap immediately so the rolling cap invariant holds
                // before the first chunk-recv iteration.
                if buf.len() > AUDIO_HYBRID_BUFFER_CAP_SAMPLES {
                    let drop = buf.len() - AUDIO_HYBRID_BUFFER_CAP_SAMPLES;
                    buf.drain(..drop);
                }
                buf
            };
            // `accumulated_since_last_classify` is initialised to the
            // current buffer length so the cadence check
            // (`accumulated >= INTERVAL && buffer.len() >= SLICE`)
            // fires immediately if the seed already contained enough
            // audio for a classify. The interval cadence only matters
            // *between* classifies, not before the first one.
            let mut accumulated_since_last_classify: usize = hybrid_buffer.len();
            // Feature 024 (backlog 0042) Site D — per-stream streaming
            // VAD detector. Constructed once per task spawn so each
            // press gets a fresh LSTM state; instance ownership avoids
            // the shared-Mutex contention concern. `None` when the
            // hybrid-side kill switch (`MUNI_VAD_STREAM_HYBRID`) is off
            // OR the factory wasn't installed at boot (both kill
            // switches off).
            let mut streaming_vad: Option<Box<dyn crate::vad::StreamingVadDetector>> =
                if stream_hybrid_enabled {
                    streaming_vad_factory.as_ref().map(|f| f())
                } else {
                    None
                };
            if streaming_vad.is_some() {
                // Seed the mirror with the pre-spawn audio under the
                // same trust assumption the hybrid uses for its
                // classify seed: audio-LID already decided to arm the
                // hybrid on this audio, so it is by definition
                // load-bearing speech. Pass it through to the mirror
                // unchanged.
                let mut mirror = audio_hybrid_speech_mirror.lock().await;
                mirror.extend_from_slice(&hybrid_buffer);
                if mirror.len() > AUDIO_HYBRID_BUFFER_CAP_SAMPLES * 8 {
                    let drop = mirror.len() - AUDIO_HYBRID_BUFFER_CAP_SAMPLES * 8;
                    mirror.drain(..drop);
                }
            }
            // Feature 021 fix 2026-05-18: the **first** classify
            // pair samples BOTH the leading slice (oldest
            // `AUDIO_HYBRID_SLICE_SAMPLES` of the seed buffer) AND
            // the trailing slice (most-recent
            // `AUDIO_HYBRID_SLICE_SAMPLES`) in parallel. Reason:
            // - Leading catches "leading-Tagalog" presses ("Hindi ko
            //   alam exactly, but ...") where the rolling-most-recent
            //   logic would only see the trailing English audio.
            // - Trailing catches the inverse: "late-Tagalog" presses
            //   ("So basically what I want to do is, mag-out tayo
            //   ng kahit saan after work.") where the leading 3 s is
            //   correctly English and the Tagalog is in the trailing
            //   ~2 s.
            // Whisper occasionally truncates the leading transcript
            // ("The thing is" — only 12 chars from 3 s of audio),
            // and the trailing call's different start offset gives
            // Whisper a second chance to capture the Tagalog content.
            // Trailing is skipped (deduped) when buffer is exactly
            // `AUDIO_HYBRID_SLICE_SAMPLES` samples — both slices
            // would be byte-identical and the second call would just
            // burn API quota. Subsequent classifies (after first-fire
            // is done) use the rolling-most-recent slice only.
            let mut first_classify_done: bool = false;
            loop {
                // Early-exit if the route has already been routed
                // (drift detector fired, or orchestrator finalize
                // raced us). Aborts the rolling-classify loop so
                // we don't burn API quota on a press that's done.
                if committed.load(Ordering::SeqCst) {
                    log::debug!(
                        target: "lid",
                        "audio-LID hybrid exiting — route already committed by orchestrator"
                    );
                    return;
                }
                if accumulated_since_last_classify >= AUDIO_HYBRID_CLASSIFY_INTERVAL_SAMPLES
                    && hybrid_buffer.len() >= AUDIO_HYBRID_SLICE_SAMPLES
                {
                    if !first_classify_done {
                        // First-fire: spawn leading classify, plus
                        // a trailing classify when the buffer has
                        // grown past `AUDIO_HYBRID_SLICE_SAMPLES`
                        // (otherwise trailing == leading and would
                        // waste an API call).
                        let leading_slice: Vec<i16> =
                            hybrid_buffer[..AUDIO_HYBRID_SLICE_SAMPLES].to_vec();
                        Self::spawn_audio_hybrid_inner_classify(
                            "audio-hybrid-leading",
                            leading_slice,
                            whisper_client.clone(),
                            secondary_lid.clone(),
                            whisper_api_key.clone(),
                            decision.clone(),
                            decision_notify.clone(),
                            committed.clone(),
                            forwarder_aborted.clone(),
                            deepgram_client.clone(),
                            usage_tx.clone(),
                            vad_detector.clone(),
                            audio_hybrid_inflight.clone(),
                            audio_hybrid_recent_text_lid_english.clone(),
                        )
                        .await;
                        if hybrid_buffer.len() > AUDIO_HYBRID_SLICE_SAMPLES {
                            let trail_start = hybrid_buffer.len() - AUDIO_HYBRID_SLICE_SAMPLES;
                            let trailing_slice: Vec<i16> = hybrid_buffer[trail_start..].to_vec();
                            Self::spawn_audio_hybrid_inner_classify(
                                "audio-hybrid-trailing",
                                trailing_slice,
                                whisper_client.clone(),
                                secondary_lid.clone(),
                                whisper_api_key.clone(),
                                decision.clone(),
                                decision_notify.clone(),
                                committed.clone(),
                                forwarder_aborted.clone(),
                                deepgram_client.clone(),
                                usage_tx.clone(),
                                vad_detector.clone(),
                                audio_hybrid_inflight.clone(),
                                audio_hybrid_recent_text_lid_english.clone(),
                            )
                            .await;
                        } else {
                            log::debug!(
                                target: "lid",
                                "audio-LID hybrid first-fire: trailing slice deduped (buffer exactly {} samples = SLICE)",
                                hybrid_buffer.len()
                            );
                        }
                        first_classify_done = true;
                    } else {
                        // Subsequent fires: rolling-most-recent only.
                        let start = hybrid_buffer.len() - AUDIO_HYBRID_SLICE_SAMPLES;
                        let rolling_slice: Vec<i16> = hybrid_buffer[start..].to_vec();
                        Self::spawn_audio_hybrid_inner_classify(
                            "audio-hybrid-rolling",
                            rolling_slice,
                            whisper_client.clone(),
                            secondary_lid.clone(),
                            whisper_api_key.clone(),
                            decision.clone(),
                            decision_notify.clone(),
                            committed.clone(),
                            forwarder_aborted.clone(),
                            deepgram_client.clone(),
                            usage_tx.clone(),
                            vad_detector.clone(),
                            audio_hybrid_inflight.clone(),
                            audio_hybrid_recent_text_lid_english.clone(),
                        )
                        .await;
                    }
                    accumulated_since_last_classify = 0;
                }

                tokio::select! {
                    biased;
                    () = released(&mut release_rx) => {
                        log::debug!(
                            target: "lid",
                            "audio-LID hybrid: release signal fired — exiting outer loop"
                        );
                        return;
                    }
                    c = audio_rx.recv() => match c {
                        Ok(chunk) => {
                            // Feature 024 (backlog 0042) Site D — feed
                            // the chunk through the per-stream
                            // streaming VAD when armed; otherwise pass
                            // the chunk through unchanged. The
                            // speech-only bytes drive BOTH the
                            // hybrid_buffer (rolling classify cadence)
                            // and the speech mirror (release-path
                            // trim).
                            let speech_only: Vec<i16> = match streaming_vad.as_mut() {
                                Some(vad) => {
                                    let mut out = Vec::with_capacity(chunk.len());
                                    vad.process_chunk(&chunk, &mut out).await;
                                    out
                                }
                                None => chunk.clone(),
                            };
                            if speech_only.is_empty() {
                                // All-suppressed chunk — hybrid cadence
                                // does NOT advance. This is the
                                // mechanism that defers rolling fires
                                // through a silence stretch.
                                continue;
                            }
                            hybrid_buffer.extend_from_slice(&speech_only);
                            accumulated_since_last_classify += speech_only.len();
                            if hybrid_buffer.len() > AUDIO_HYBRID_BUFFER_CAP_SAMPLES {
                                let drop = hybrid_buffer.len() - AUDIO_HYBRID_BUFFER_CAP_SAMPLES;
                                hybrid_buffer.drain(..drop);
                            }
                            if streaming_vad.is_some() {
                                let mut mirror = audio_hybrid_speech_mirror.lock().await;
                                mirror.extend_from_slice(&speech_only);
                                if mirror.len() > AUDIO_HYBRID_BUFFER_CAP_SAMPLES * 8 {
                                    let drop = mirror.len() - AUDIO_HYBRID_BUFFER_CAP_SAMPLES * 8;
                                    mirror.drain(..drop);
                                }
                            }
                        }
                        Err(RecvError::Lagged(n)) => {
                            log::warn!(
                                target: "lid",
                                "audio-LID hybrid audio lagged by {n} — slice freshness compromised"
                            );
                            continue;
                        }
                        Err(RecvError::Closed) => {
                            log::debug!(
                                target: "lid",
                                "audio-LID hybrid: audio channel closed — exiting outer loop"
                            );
                            return;
                        }
                    }
                }
            }
        })
    }
}
