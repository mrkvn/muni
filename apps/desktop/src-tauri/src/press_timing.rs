//! Per-press phase-timing ledger (plan 041, wave 1).
//!
//! One [`PressTiming`] is built at hotkey release, filled phase-by-phase
//! as the release path progresses (ASR → LID settle → cleanup → paste),
//! and finally both logged (one `target: "press_timing"` line) and
//! persisted to the `press_timings` SQLite table. Every future latency
//! optimization is provable with p50/p95 SQL over real rows instead of
//! grepping two log lines and subtracting.
//!
//! ## Absent phases are `None`, never `0`
//!
//! Different release routes skip different phases (an english-fast press
//! never pays a LID-settle wait; a cleanup-unavailable press never runs
//! the Groq call). Those columns stay `None` so a query can tell "phase
//! did not occur" apart from "phase took ~0 ms". `route` and `total_ms`
//! are the only always-present data fields.
//!
//! ## Send + 'static, plain owned data
//!
//! A `PressTiming` crosses an `await` boundary and moves into the
//! `persist_history` `spawn_blocking` closure, so it holds only owned
//! values (no borrows, no `Instant`) and is cheap to `Clone`.

use crate::usage_store::NewPressTiming;

/// Accumulating phase-timing record for a single press. Constructed with
/// the fields known at release ([`PressTiming::new`]); the remaining
/// phases are filled by the delivery tail via the setters as each
/// completes.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PressTiming {
    /// `served_by` label committed for the press (deepgram / whisper /
    /// rescue / parakeet-local …). Always present.
    route: String,
    /// Press audio duration in ms (mirrors the telemetry `audio_duration_ms`).
    audio_ms: Option<i64>,
    /// Release → raw transcript ready.
    asr_ms: Option<i64>,
    /// Summed release-path LID-settle waits; `None` on routes with none.
    lid_wait_ms: Option<i64>,
    /// Groq cleanup call, spanning the primary attempt and any retry;
    /// `None` when cleanup never ran (e.g. missing prompt/client/key).
    cleanup_ms: Option<i64>,
    /// Cleanup done → paste delivered (incl. `await_turn`); `None` on a
    /// held delivery (no paste occurred).
    inject_ms: Option<i64>,
    /// Release → paste delivered. Always present once the press completes.
    total_ms: Option<i64>,
}

impl PressTiming {
    /// Build the release-time skeleton with the phases already known at
    /// finalize: the committed route, press audio duration, the ASR span,
    /// and the summed LID-settle wait (`None` on routes that skipped it).
    pub fn new(
        route: impl Into<String>,
        audio_ms: Option<i64>,
        asr_ms: Option<i64>,
        lid_wait_ms: Option<i64>,
    ) -> Self {
        Self {
            route: route.into(),
            audio_ms,
            asr_ms,
            lid_wait_ms,
            cleanup_ms: None,
            inject_ms: None,
            total_ms: None,
        }
    }

    /// Groq cleanup elapsed (primary + retry). Left `None` when cleanup
    /// never ran.
    pub fn set_cleanup_ms(&mut self, ms: i64) {
        self.cleanup_ms = Some(ms);
    }

    /// Cleanup-done → paste-delivered span. Left `None` on held deliveries.
    pub fn set_inject_ms(&mut self, ms: i64) {
        self.inject_ms = Some(ms);
    }

    /// Release → paste-delivered span. Set once at the delivery tail.
    pub fn set_total_ms(&mut self, ms: i64) {
        self.total_ms = Some(ms);
    }

    /// Emit the single structured ledger line. Absent phases render as
    /// `-` so a log scan can tell them apart from a measured `0`.
    pub fn log_line(&self) {
        log::info!(
            target: "press_timing",
            "press: route={} total_ms={} asr_ms={} lid_wait_ms={} cleanup_ms={} inject_ms={} audio_ms={}",
            self.route,
            fmt_opt(self.total_ms),
            fmt_opt(self.asr_ms),
            fmt_opt(self.lid_wait_ms),
            fmt_opt(self.cleanup_ms),
            fmt_opt(self.inject_ms),
            fmt_opt(self.audio_ms),
        );
    }

    /// Consume into the SQLite insert payload, stamping the press's
    /// `session_id` (the `dictation_records` id, or `None` when history
    /// didn't persist) and `created_at` (unix seconds). `total_ms`
    /// defaults to 0 only in the impossible case it was never set — the
    /// column is NOT NULL and the delivery tail always sets it before
    /// persisting.
    pub fn into_new_press_timing(
        self,
        session_id: Option<i64>,
        created_at_unix: i64,
    ) -> NewPressTiming {
        NewPressTiming {
            created_at: created_at_unix,
            session_id,
            route: self.route,
            audio_ms: self.audio_ms,
            asr_ms: self.asr_ms,
            lid_wait_ms: self.lid_wait_ms,
            cleanup_ms: self.cleanup_ms,
            inject_ms: self.inject_ms,
            total_ms: self.total_ms.unwrap_or(0),
        }
    }
}

/// Render an optional phase measurement for the log line: the number, or
/// `-` when the phase did not occur.
fn fmt_opt(v: Option<i64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "-".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_opt_renders_dash_for_none() {
        assert_eq!(fmt_opt(Some(0)), "0");
        assert_eq!(fmt_opt(Some(1234)), "1234");
        assert_eq!(fmt_opt(None), "-");
    }

    #[test]
    fn into_new_press_timing_maps_every_field() {
        let mut t = PressTiming::new("deepgram", Some(4200), Some(380), None);
        t.set_cleanup_ms(420);
        t.set_inject_ms(35);
        t.set_total_ms(850);

        let row = t.into_new_press_timing(Some(7), 1_779_192_000);
        assert_eq!(
            row,
            NewPressTiming {
                created_at: 1_779_192_000,
                session_id: Some(7),
                route: "deepgram".to_string(),
                audio_ms: Some(4200),
                asr_ms: Some(380),
                lid_wait_ms: None,
                cleanup_ms: Some(420),
                inject_ms: Some(35),
                total_ms: 850,
            }
        );
    }

    /// A route that skipped the LID-settle and cleanup phases keeps those
    /// columns NULL (not 0), and a never-set `total_ms` degrades to 0 for
    /// the NOT NULL column rather than panicking.
    #[test]
    fn absent_phases_stay_none_and_total_defaults_to_zero() {
        let t = PressTiming::new("parakeet-local", Some(1500), Some(210), None);
        let row = t.into_new_press_timing(None, 42);
        assert_eq!(row.session_id, None);
        assert_eq!(row.lid_wait_ms, None);
        assert_eq!(row.cleanup_ms, None);
        assert_eq!(row.inject_ms, None);
        assert_eq!(row.total_ms, 0);
    }

    /// The log line must survive an all-`None` timing (the skeleton
    /// straight out of `new`) without panicking, rendering every absent
    /// phase as `-`. Formatting is asserted indirectly via `fmt_opt`;
    /// this guards the `log_line` call path itself.
    #[test]
    fn log_line_does_not_panic_on_sparse_timing() {
        let t = PressTiming::new("rescue", None, None, None);
        t.log_line();
    }
}
