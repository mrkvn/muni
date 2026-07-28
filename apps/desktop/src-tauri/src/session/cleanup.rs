//! Groq cleanup pass, in-order delivery, completion telemetry, and history
//! persistence for [`DictationSession`] (plan 039 slice 25).
//!
//! Extracted verbatim from `session.rs` as a child module so it can reach the
//! orchestrator's private fields/helpers through `use super::*` — no behavior
//! change, no widened visibility beyond `pub(super)` on the one method the
//! parent driver loop still calls (`run_groq_cleanup`).

use super::*;

impl DictationSession {
    /// Drive the Groq cleanup pass for a non-empty raw transcript.
    ///
    /// Mirrors Swift `DictationSession`'s raw-fallback semantics: any error
    /// path (missing client/prompt/key, server error, transport error) still
    /// surfaces the raw transcript via [`EVENT_TRANSCRIPT_FINAL`] and pastes
    /// it into the focused app, so a failed cleanup never feels like a
    /// black hole.
    ///
    /// `press_duration` drives the dynamic cleanup-config routing
    /// (`.claude/learned/005_*.md`): short presses use the
    /// `MUNI_CLEANUP_MODEL` / `MUNI_CLEANUP_REASONING_EFFORT` pair (short
    /// profile); long presses use the `MUNI_CLEANUP_LONG_*` pair, with
    /// the boundary set by `MUNI_CLEANUP_LONG_PRESS_THRESHOLD_S` (default
    /// 5.0 s). The resolved model is also recorded on the usage row so
    /// cost analytics see the actual model used, not the client's
    /// boot-time snapshot.
    pub(super) async fn run_groq_cleanup(
        &self,
        raw_transcript: &str,
        served_by: &'static str,
        press_duration: Duration,
        auto_submit: bool,
        mut ctx: DeliveryContext,
    ) {
        // A recovered partial is a degraded success even when cleanup itself
        // succeeds — the upstream transcript may be truncated.
        let is_partial = served_by_is_partial(served_by);

        // Feature 029 — runtime self-correction marker cleanup. Strip
        // markers (`scratch that`, `actually no`, `I mean`, ...) and the
        // cancelled portion of the user's thought BEFORE the LLM sees
        // the text. The LLM does its remaining jobs (filler removal,
        // grammar, punctuation, capitalization, list/URL handling) on a
        // marker-free input. Borrowed unchanged when no markers fire.
        //
        // Computed up front — ahead of the three cleanup-unavailable guards
        // below (plan 039 slice 2, finding 2) — so that even a missing
        // CleanupPrompt, uninitialized GroqClient, or keychain/secrets error
        // still pastes the marker-stripped text, never a raw transcript with a
        // live `scratch that` the user already cancelled. `stripped` == raw
        // when no marker fired, so this is zero-cost in the common case (the
        // call runs unconditionally on every press regardless).
        let stripped = crate::self_correction::apply(raw_transcript);

        // Feature 033 (Phase 3, task 23) — feature-usage signal: a
        // self-correction marker fired iff `apply` had to rewrite the text
        // (`Cow::Owned`). A bare COUNT event, no marker text, no transcript
        // content. Fire-and-forget; a no-op when analytics is off.
        if matches!(stripped, std::borrow::Cow::Owned(_)) {
            crate::telemetry::emit_event(crate::telemetry::events::scratch_that_used());
        }

        let Some(prompt) = self.deps.prompt.as_deref() else {
            log::error!(target: "session", "CleanupPrompt unavailable — falling back to marker-stripped");
            self.deliver_emit_error(ctx.epoch, &MuniError::CleanupPromptMissing);
            self.deliver_final(
                stripped.as_ref(),
                raw_transcript,
                served_by,
                CompletionMetrics::raw_fallback(press_duration),
                auto_submit,
                ctx,
            )
            .await;
            return;
        };
        let Some(client) = self.deps.groq.as_deref() else {
            log::error!(target: "groq", "GroqClient unavailable — falling back to marker-stripped");
            self.deliver_emit_error(
                ctx.epoch,
                &MuniError::GroqConnectionFailed {
                    reason: "client not initialized".into(),
                },
            );
            self.deliver_final(
                stripped.as_ref(),
                raw_transcript,
                served_by,
                CompletionMetrics::raw_fallback(press_duration),
                auto_submit,
                ctx,
            )
            .await;
            return;
        };

        // Hot release-path read — cached to skip a per-press keychain IPC
        // (plan 039 task 17). Env override stays live; keychain layer is cached
        // and invalidated on `secrets://changed`.
        let api_key = match secrets::get_cached(secrets::GROQ_ACCOUNT) {
            Ok(k) => k,
            Err(err) => {
                log::error!(target: "groq", "{}", err.user_message());
                self.deliver_emit_error(ctx.epoch, &err);
                self.deliver_final(
                    stripped.as_ref(),
                    raw_transcript,
                    served_by,
                    CompletionMetrics::raw_fallback(press_duration),
                    auto_submit,
                    ctx,
                )
                .await;
                return;
            }
        };

        // Snapshot the About Me body and the rendered vocabulary
        // block under their read locks and clone them before any await
        // — neither lock may be held across the Groq HTTP call. Empty
        // body / empty list produces a byte-identical prompt to the
        // pre-feature behaviour (see `prompt::make_messages_with_context`).
        let about_me_text = self.deps.about_me.text();
        let vocabulary_block = self.deps.vocabulary.render_block();
        let user_prompt_text = self.deps.user_prompt.text();
        let messages = match prompt.make_messages_with_context(
            stripped.as_ref(),
            &about_me_text,
            &vocabulary_block,
            &user_prompt_text,
        ) {
            Ok(m) => m,
            Err(err) => {
                log::error!(target: "groq", "prompt load failed: {}", err.user_message());
                self.deliver_emit_error(ctx.epoch, &err);
                // Cleanup can't run, but self-correction already applied
                // deterministically before this point — paste the
                // marker-stripped text, not the raw transcript, so a
                // failed cleanup never re-surfaces a `scratch that` the
                // user already cancelled. `stripped` == raw when no marker
                // fired, so this is a no-op in the common case. History
                // still records the raw transcript (second arg).
                self.deliver_final(
                    stripped.as_ref(),
                    raw_transcript,
                    served_by,
                    CompletionMetrics::raw_fallback(press_duration),
                    auto_submit,
                    ctx,
                )
                .await;
                return;
            }
        };

        // Resolve (model, effort) per-press from the press's wall-clock
        // duration. Short presses → short profile (existing env);
        // long presses → long profile (`MUNI_CLEANUP_LONG_*`). Keep the
        // resolved values local so the usage row records the actual
        // model used (not the client's boot-time snapshot, which is
        // always the short-profile model).
        let press_duration_s = press_duration.as_secs_f64();
        let (cleanup_model, cleanup_effort) =
            groq::resolve_cleanup_config_for_duration(press_duration_s);
        let profile = if press_duration_s > groq::resolve_cleanup_long_press_threshold_s() {
            "long"
        } else {
            "short"
        };
        let primary_timeout = groq::resolve_cleanup_timeout_for_duration(press_duration_s);
        log::info!(
            target: "groq",
            "cleanup: press={:.2}s → profile={} model={} effort={} timeout={}ms",
            press_duration_s,
            profile,
            cleanup_model,
            cleanup_effort,
            primary_timeout.as_millis()
        );

        // Plan 041 (wave 1) — `cleanup_ms` spans the whole Groq cleanup
        // phase: the primary attempt plus any retry. Stamped here, AFTER
        // the cleanup-unavailable guards above (which leave `cleanup_ms`
        // NULL because the Groq call never ran), and read at whichever
        // terminal arm hands off to `deliver_final`.
        let cleanup_started = Instant::now();

        // Primary attempt.
        let primary_err = match self
            .attempt_cleanup(
                client,
                messages.clone(),
                &api_key,
                &cleanup_model,
                &cleanup_effort,
                primary_timeout,
            )
            .await
        {
            Ok((cleaned, usage, elapsed)) => {
                let final_text = self.finalize_cleanup_text(&cleaned, stripped.as_ref());
                self.log_cleanup_usage(usage.as_ref(), elapsed);
                self.record_cleanup_usage(&cleanup_model, usage.as_ref(), elapsed);
                ctx.timing
                    .set_cleanup_ms(cleanup_started.elapsed().as_millis() as i64);
                self.deliver_final(
                    &final_text,
                    raw_transcript,
                    served_by,
                    CompletionMetrics {
                        press_duration_ms: press_duration.as_millis() as u64,
                        cleanup_latency_ms: Some(elapsed.as_millis() as u64),
                        cleanup_model: cleanup_model.clone(),
                        degraded: is_partial,
                    },
                    auto_submit,
                    ctx,
                )
                .await;
                return;
            }
            Err(err) => err,
        };

        // Retry attempt — pinned to `openai/gpt-oss-120b` + `low` effort
        // under the long-profile timeout. The retry runs on a different
        // model than the short-profile primary so the recovery has a
        // meaningfully different shot at success when the primary failed
        // on a model/region-specific Groq hiccup. The cap is one retry:
        // if the larger model also fails we paste raw and let the user
        // re-press, rather than compounding latency further.
        let retry_timeout = groq::resolve_cleanup_long_timeout();
        log::warn!(
            target: "groq",
            "cleanup primary failed ({}) — retrying with model={} effort={} timeout={}ms",
            primary_err.user_message(),
            groq::CLEANUP_RETRY_MODEL,
            groq::CLEANUP_RETRY_EFFORT,
            retry_timeout.as_millis()
        );
        match self
            .attempt_cleanup(
                client,
                messages,
                &api_key,
                groq::CLEANUP_RETRY_MODEL,
                groq::CLEANUP_RETRY_EFFORT,
                retry_timeout,
            )
            .await
        {
            Ok((cleaned, usage, elapsed)) => {
                log::info!(
                    target: "groq",
                    "cleanup retry succeeded in {} ms",
                    elapsed.as_millis()
                );
                let final_text = self.finalize_cleanup_text(&cleaned, stripped.as_ref());
                self.log_cleanup_usage(usage.as_ref(), elapsed);
                self.record_cleanup_usage(groq::CLEANUP_RETRY_MODEL, usage.as_ref(), elapsed);
                ctx.timing
                    .set_cleanup_ms(cleanup_started.elapsed().as_millis() as i64);
                self.deliver_final(
                    &final_text,
                    raw_transcript,
                    served_by,
                    CompletionMetrics {
                        press_duration_ms: press_duration.as_millis() as u64,
                        cleanup_latency_ms: Some(elapsed.as_millis() as u64),
                        cleanup_model: groq::CLEANUP_RETRY_MODEL.to_string(),
                        // The primary attempt failed; the press only landed via
                        // the retry — count it as degraded.
                        degraded: true,
                    },
                    auto_submit,
                    ctx,
                )
                .await;
            }
            Err(retry_err) => {
                log::warn!(
                    target: "groq",
                    "cleanup retry failed: {}",
                    retry_err.user_message()
                );
                // Surface the *retry* error to the user — that's the last
                // word from Groq and matches what they'd see if they
                // re-pressed and hit the same condition. The primary
                // error is already in the log line above.
                self.deliver_emit_error(ctx.epoch, &retry_err);
                // Cleanup ran (both attempts) but neither served — record the
                // wall-clock spent trying, so cold/slow cleanup outliers still
                // show a `cleanup_ms` even on the raw-fallback path.
                ctx.timing
                    .set_cleanup_ms(cleanup_started.elapsed().as_millis() as i64);
                // Primary + retry both failed — paste the marker-stripped
                // text (self-correction ran deterministically before the
                // cleanup attempts), not the raw transcript. `stripped` ==
                // raw when no marker fired. History records the raw
                // transcript (second arg).
                self.deliver_final(
                    stripped.as_ref(),
                    raw_transcript,
                    served_by,
                    CompletionMetrics::raw_fallback(press_duration),
                    auto_submit,
                    ctx,
                )
                .await;
            }
        }
    }

    /// One cleanup HTTP attempt. Returns the trimmed cleaned text, the
    /// optional `usage` block, and the wall-clock elapsed. Pure I/O —
    /// no logging, no usage-record emission; callers handle both so the
    /// primary and retry paths can tag the records differently.
    async fn attempt_cleanup(
        &self,
        client: &GroqClient,
        messages: Vec<groq::GroqMessage>,
        api_key: &str,
        model: &str,
        effort: &str,
        timeout: Duration,
    ) -> Result<(String, Option<groq::UsageBlock>, Duration), MuniError> {
        let started = Instant::now();
        let (cleaned, usage) = client
            .complete_with_timeout(
                GroqRequest::cleanup_with_effort(messages, model, effort),
                api_key,
                timeout,
            )
            .await?;
        Ok((cleaned, usage, started.elapsed()))
    }

    /// Decide what to paste given the cleaned text returned by Groq. An
    /// empty cleaned body (e.g. reasoning exhausted the completion cap
    /// before any `delta.content` arrived) falls back to `fallback` — the
    /// marker-stripped self-correction output, NOT the raw transcript, so an
    /// empty-cleanup failure never re-surfaces a `scratch that` the user
    /// already cancelled (plan 039 slice 2). `fallback` == raw when no
    /// marker fired, so this is a no-op in the common case; the caller still
    /// records the raw transcript as history's raw field.
    fn finalize_cleanup_text(&self, cleaned: &str, fallback: &str) -> String {
        if cleaned.is_empty() {
            log::warn!(target: "groq", "empty cleanup — falling back to marker-stripped text");
            fallback.to_string()
        } else {
            log::info!(target: "groq", "cleaned transcript: {} chars", cleaned.len());
            cleaned.to_string()
        }
    }

    /// Emit the `cleanup usage:` log line. Shared between the primary
    /// and retry success paths so the prompt/cached/completion/latency
    /// breakdown stays formatted identically regardless of which
    /// attempt won.
    fn log_cleanup_usage(&self, usage: Option<&groq::UsageBlock>, elapsed: Duration) {
        let Some(u) = usage else { return };
        let cached = u
            .cached_tokens
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".into());
        let pct = u
            .cached_tokens
            .filter(|_| u.prompt_tokens > 0)
            .map(|c| (c as f64 / u.prompt_tokens as f64) * 100.0);
        log::info!(
            target: "groq",
            "cleanup usage: prompt={} cached={} ({}) completion={} latency={}ms",
            u.prompt_tokens,
            cached,
            pct.map(|p| format!("{p:.0}% hit"))
                .unwrap_or_else(|| "no cache field".into()),
            u.completion_tokens,
            elapsed.as_millis()
        );
    }

    /// Push a `cleanup` UsageRecord (status=ok) onto the usage channel.
    /// Shared between primary and retry success paths so the `model`
    /// column in `api_calls` always reflects which attempt actually
    /// served the press. Failed attempts are not recorded — matches the
    /// pre-retry behaviour and keeps Muni.log as the source of truth
    /// for failure inspection.
    fn record_cleanup_usage(
        &self,
        model: &str,
        usage: Option<&groq::UsageBlock>,
        elapsed: Duration,
    ) {
        // Plan 041 (task 7) — a successful real cleanup just warmed
        // Groq's prompt-prefix cache (and the connection pool). Stamp
        // `note_prefix_touch` so the periodic re-warm knows the cache is
        // fresh (and the keepalive knows the pool is warm). Done before
        // the `usage_tx` guard so a disabled usage channel doesn't
        // suppress the staleness reset.
        if let Some(activity) = self.deps.groq_activity.as_ref() {
            activity.note_prefix_touch();
        }
        let Some(tx) = self.deps.usage_tx.as_ref() else {
            return;
        };
        try_send_drop_oldest(
            tx,
            UsageRecord {
                provider: crate::pricing::PROVIDER_GROQ.into(),
                model: model.to_string(),
                call_kind: crate::pricing::CALL_KIND_CLEANUP.into(),
                audio_seconds: None,
                input_tokens: usage.map(|u| u.prompt_tokens),
                output_tokens: usage.map(|u| u.completion_tokens),
                latency_ms: Some(elapsed.as_millis() as i64),
                status: "ok".into(),
                request_id: None,
                session_id: None,
                created_at_unix: unix_seconds_now(),
            },
        );
    }

    /// Emit the final text for the debug overlay and paste it into the
    /// focused app. Each side fails independently — a paste error does not
    /// suppress the final-text event, and a missing debug overlay does not
    /// block the paste.
    ///
    /// `raw_for_history` carries the un-cleaned Deepgram transcript so a
    /// successful paste can persist both the raw and cleaned text into
    /// the history store. We resolve the frontmost-app bundle id at the
    /// moment of paste (not at press time) so the recorded target
    /// matches the app the cleaned text actually landed in.
    pub(super) async fn deliver_final(
        &self,
        text: &str,
        raw_for_history: &str,
        served_by: &'static str,
        metrics: CompletionMetrics,
        auto_submit: bool,
        mut ctx: DeliveryContext,
    ) {
        // Plan 041 (wave 1) — timing anchors. `inject_started` opens the
        // inject phase (cleanup-done → paste-delivered, INCLUDING the
        // `await_turn` below); `press_t0` anchors `total_ms`. Captured
        // before any await so both spans are measured whole.
        let inject_started = Instant::now();
        let press_t0 = ctx.press_t0;

        // Plan 039 task 25 — pastes must land in press order even though the
        // (slow) Groq cleanup that precedes this ran concurrently across
        // deliveries. Block until the previous press's delivery has
        // completed before doing anything user-visible (final-text event,
        // focus probe, paste). A dropped predecessor resolves immediately,
        // so the chain never deadlocks.
        ctx.await_turn().await;

        // A recovered partial may be truncated mid-thought — never auto-press
        // Enter on it, even in Enter-to-finish mode, so a half-finished message
        // is never silently submitted.
        let is_partial = served_by_is_partial(served_by);
        self.emit(EVENT_TRANSCRIPT_FINAL, text.to_string());

        // Feature 037 — before the automatic paste, ask the OS whether an
        // editable field is focused. A *confident* negative (and only that)
        // means a blind Cmd+V would land nowhere, so we hold the dictation for
        // the re-paste hotkey instead of firing a paste into the void. Every
        // other probe result (Editable / Unknown / permission-denied / AX
        // error) falls through to the existing paste path unchanged — never
        // worse than status quo. Empty text never probes: there's nothing to
        // re-paste, so it stays on the quiet `NothingToPaste` path below.
        if !text.is_empty()
            && self.deps.injector.has_editable_focus().await == FocusProbe::NoEditableField
        {
            log::info!(target: "paste", "no editable focus — dictation held for re-paste");
            // Still record the completion so the re-paste hotkey has a row to
            // reinject and the telemetry funnel counts the press — tagged
            // "held" so held vs pasted is distinguishable without a new event.
            // The notice we're about to show promises exactly this row.
            let bundle_id = frontmost_app_bundle_id();
            // Plan 041 (wave 1) — held delivery: no paste happened, so
            // `inject_ms` stays NULL; `total_ms` still closes at the hold
            // decision. Log the ledger line, then persist the row.
            let mut timing = std::mem::take(&mut ctx.timing);
            timing.set_total_ms(press_t0.elapsed().as_millis() as i64);
            timing.log_line();
            self.record_completion(
                self.deps.history.clone(),
                raw_for_history,
                text,
                served_by,
                &metrics,
                bundle_id,
                crate::telemetry::events::DELIVERY_HELD,
                timing,
            );
            // Surface the dynamic "Press <hotkey> to insert your dictation"
            // notice. No paste, and never an auto-Enter.
            (self.deps.show_repaste_notice)();
            self.deliver_notify_state(ctx.epoch, SessionState::Idle);
            return;
        }

        match self.deps.injector.paste(text).await {
            Ok(()) => {
                // Explicit "text visible" marker for §10 latency analysis
                // — combined with the ms-precision log format from
                // `lib::run`, the gap between `Hotkey released` and this
                // line is the user-perceived end-to-end delay.
                log::info!(target: "paste", "paste delivered: {} chars", text.len());
                // "Press Enter to finish" → submit the pasted text by
                // injecting Enter (send the chat message, etc.). Only
                // after a *successful* paste, so an empty/failed paste
                // never fires a stray newline. The text already landed, so
                // a failed Enter is a soft degradation (user hits Enter
                // themselves) — log it, don't surface a scary error.
                if auto_submit && !is_partial {
                    match self.deps.injector.press_enter().await {
                        Ok(()) => log::info!(
                            target: "paste",
                            "auto-submit: Enter posted after paste"
                        ),
                        Err(err) => log::warn!(
                            target: "paste",
                            "auto-submit (Enter) failed after paste: {} ({:?})",
                            err.user_message(),
                            err.severity()
                        ),
                    }
                }
                // Resolve the frontmost app once, shared by the history row
                // and the telemetry event so both attribute the same target.
                let bundle_id = frontmost_app_bundle_id();
                // Plan 041 (wave 1) — paste landed: close the inject phase
                // (cleanup-done → here, incl. `await_turn`) and `total_ms`,
                // log the ledger line, then persist the row.
                let mut timing = std::mem::take(&mut ctx.timing);
                timing.set_inject_ms(inject_started.elapsed().as_millis() as i64);
                timing.set_total_ms(press_t0.elapsed().as_millis() as i64);
                timing.log_line();
                self.record_completion(
                    self.deps.history.clone(),
                    raw_for_history,
                    text,
                    served_by,
                    &metrics,
                    bundle_id,
                    crate::telemetry::events::DELIVERY_PASTED,
                    timing,
                );
                self.deliver_notify_state(ctx.epoch, SessionState::Idle);
            }
            // NothingToPaste is the only "error" that's actually expected
            // steady state — the user pressed and released without speaking,
            // or every word came back blank. Skip the user-facing event for
            // it AND treat the cycle as a clean idle return.
            Err(MuniError::NothingToPaste) => {
                self.deliver_notify_state(ctx.epoch, SessionState::Idle)
            }
            Err(err) => {
                log::warn!(
                    target: "paste",
                    "paste failed: {} ({:?})",
                    err.user_message(),
                    err.severity()
                );
                self.deliver_emit_error(ctx.epoch, &err);
            }
        }
    }

    /// Shared completion tail for a delivered dictation: persist the history row
    /// and emit the metadata-only `dictation_completed` telemetry event. Called
    /// by both the real-paste arm (`delivery = "pasted"`) and the
    /// no-editable-field hold arm (`delivery = "held"`) so the two never drift
    /// on what they record.
    ///
    /// `store` is the persistence target chosen by the caller — both arms pass
    /// `history` (the full store), so `None` only in tests. The telemetry event
    /// fires regardless of `store`.
    ///
    /// Auto-Enter is deliberately NOT here — it stays strictly on the
    /// successful-real-paste arm, since a held dictation was never pasted and a
    /// recovered partial must never be auto-submitted.
    #[allow(clippy::too_many_arguments)]
    fn record_completion(
        &self,
        store: Option<Arc<HistoryStore>>,
        raw_for_history: &str,
        text: &str,
        served_by: &'static str,
        metrics: &CompletionMetrics,
        bundle_id: Option<String>,
        delivery: &'static str,
        timing: PressTiming,
    ) {
        self.persist_history(
            store,
            raw_for_history,
            text,
            served_by,
            bundle_id.clone(),
            timing,
        );
        // Feature 033 — fire-and-forget operational-health event. Built from
        // metadata only (timings, model, buckets, served_by, the bucketed
        // target app, the delivery outcome); the char COUNT is taken here,
        // never the characters. No-op when analytics is off. Must not block the
        // hot path (enqueue is a sub-ms SQLite insert; emit never awaits).
        crate::telemetry::emit_event(crate::telemetry::events::dictation_completed(
            metrics.cleanup_latency_ms.unwrap_or(0),
            served_by,
            metrics.press_duration_ms,
            text.chars().count(),
            &metrics.cleanup_model,
            metrics.degraded,
            bundle_id.as_deref(),
            delivery,
        ));
    }

    /// Persist a successful press to the history store. Runs the SQLite
    /// insert on a blocking task so the orchestrator's tokio runtime
    /// stays free for the next press. Best-effort: a failed insert is
    /// logged but does NOT propagate a user-visible error — the paste
    /// already landed and the user has their text.
    ///
    /// On success, fires [`EVENT_HISTORY_CHANGED`] so the React History
    /// tab can refresh without racing the writer (the broader
    /// `transcript://final` event fires *before* this insert is queued
    /// and would refetch stale data).
    fn persist_history(
        &self,
        store: Option<Arc<HistoryStore>>,
        raw: &str,
        cleaned: &str,
        served_by: &str,
        bundle_id: Option<String>,
        timing: PressTiming,
    ) {
        let Some(history) = store else {
            return;
        };
        let record = NewDictationRecord {
            raw_text: raw.to_string(),
            cleaned_text: cleaned.to_string(),
            target_app_bundle_id: bundle_id,
            served_by: served_by.to_string(),
        };
        // Clone the emitter so the blocking task can announce the
        // insert without going through &self (which doesn't outlive
        // the spawned closure). EventEmitter is Arc-internally so the
        // clone is cheap.
        let emitter = self.deps.emitter.clone();
        // Plan 041 (wave 1) — persist the press-timing row in the SAME
        // blocking task as the history insert. This is the only place the
        // `dictation_records` id exists, keeps both SQLite writes off the
        // async runtime, and runs strictly AFTER paste (nothing here is on
        // the hot path). `None` usage store (failed to open at boot, or
        // tests) → skip the row; the ledger log line already fired in
        // `deliver_final`, so the press is never invisible.
        let usage_store = self.deps.usage_store.clone();
        let created_at = unix_seconds_now();
        // Spawn a blocking task so the SQLite write doesn't park a
        // tokio worker. The store itself is `Send + Sync`; we only
        // need the blocking spawn for the sync rusqlite call.
        tauri::async_runtime::spawn_blocking(move || match history.insert(record) {
            Ok(id) => {
                log::debug!(target: "history", "inserted record id={id}");
                emitter(EVENT_HISTORY_CHANGED, String::new());
                persist_press_timing(usage_store.as_ref(), timing, Some(id), created_at);
            }
            Err(err) => {
                log::warn!(target: "history", "insert failed: {err}");
                // History failed, but the timing data stands on its own —
                // persist it with a NULL `session_id` rather than losing it.
                persist_press_timing(usage_store.as_ref(), timing, None, created_at);
            }
        });
    }
}

/// Insert one `press_timings` row via the cost-tracking store (plan 041).
/// A `None` store (failed to open at boot / tests) is a silent no-op — the
/// ledger log line already fired at the call site. Runs inside the
/// `persist_history` `spawn_blocking` closure, never on the hot path.
fn persist_press_timing(
    usage_store: Option<&Arc<UsageStore>>,
    timing: PressTiming,
    session_id: Option<i64>,
    created_at: i64,
) {
    let Some(store) = usage_store else {
        return;
    };
    if let Err(err) =
        store.insert_press_timing(timing.into_new_press_timing(session_id, created_at))
    {
        log::warn!(target: "press_timing", "insert failed: {}", err.user_message());
    }
}
