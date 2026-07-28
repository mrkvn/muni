//! Plan 041 (task 7) — shared Groq activity tracking.
//!
//! Two process-lifetime timestamps, `app.manage`d as
//! `Arc<GroqActivity>`, that feed the two plan-041 background timers:
//!
//! - `last_any_call` — the last time *any* real Groq HTTP request went
//!   out (cleanup, Whisper transcribe, LID classify). The pool
//!   keepalive ([`crate::groq_keepalive`]) reads it to skip a ping when
//!   real traffic already kept the TCP/TLS pool warm inside the tick
//!   window. NOT touched by the keepalive itself — a keepalive GET
//!   warms the socket but is not "real traffic" for this gate.
//! - `last_prefix_touch` — the last time Groq's per-account
//!   prompt-prefix cache was warmed by a real cleanup-shaped request
//!   (a real press cleanup, or a warm-up). The periodic re-warm
//!   ([`crate::groq_warmup::spawn_periodic_rewarm`], plan slice 4)
//!   reads it to decide whether the cache has gone stale (≥ 90 min).
//!
//! Both are unix seconds; `0` means "never" and reads as
//! "infinitely stale" so a fresh boot (before the first press / warm-up)
//! never suppresses the keepalive and always looks re-warm-eligible.
//!
//! ## Ordering
//!
//! All loads/stores use `Ordering::Relaxed`. These timestamps gate a
//! best-effort keepalive ping and a best-effort cache warm-up — neither
//! is a correctness invariant. A stale read on one tick at worst fires
//! one redundant GET (or defers a warm-up by one 4–5 min tick); no data
//! races that matter, no happens-before requirement with other state.
//! Relaxed is the cheapest ordering that still gives atomic,
//! tear-free 64-bit reads/writes.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Shared last-call / last-prefix-touch timestamps (unix secs; `0` =
/// never). See the module docs for the two consumers.
#[derive(Debug)]
pub struct GroqActivity {
    last_any_call: AtomicI64,
    last_prefix_touch: AtomicI64,
}

impl Default for GroqActivity {
    fn default() -> Self {
        Self::new()
    }
}

impl GroqActivity {
    /// Fresh tracker: both timestamps at `0` ("never").
    pub fn new() -> Self {
        Self {
            last_any_call: AtomicI64::new(0),
            last_prefix_touch: AtomicI64::new(0),
        }
    }

    /// Record that a real Groq HTTP call just completed (Whisper
    /// transcribe, Groq LID classify). Bumps `last_any_call` only —
    /// these calls do NOT warm the prompt-prefix cache the way a
    /// cleanup-shaped request does, so they must not reset the re-warm
    /// staleness clock.
    pub fn note_call(&self) {
        self.last_any_call.store(now_unix(), Ordering::Relaxed);
    }

    /// Record that a real cleanup-shaped request just warmed Groq's
    /// prompt-prefix cache (a successful press cleanup, or a warm-up).
    /// Bumps `last_prefix_touch` AND `last_any_call` — such a request
    /// also kept the connection pool warm, so it counts for the
    /// keepalive gate too.
    pub fn note_prefix_touch(&self) {
        let now = now_unix();
        self.last_prefix_touch.store(now, Ordering::Relaxed);
        self.last_any_call.store(now, Ordering::Relaxed);
    }

    /// Seconds since the last real Groq call, or [`i64::MAX`] when no
    /// call has ever landed (`0`). Used by the keepalive skip-gate.
    pub fn secs_since_call(&self) -> i64 {
        secs_since(self.last_any_call.load(Ordering::Relaxed))
    }

    /// Seconds since the prompt-prefix cache was last warmed, or
    /// [`i64::MAX`] when never (`0`). Used by the periodic re-warm gate.
    pub fn secs_since_prefix_touch(&self) -> i64 {
        secs_since(self.last_prefix_touch.load(Ordering::Relaxed))
    }

    /// Test-only: seed `last_any_call` so keepalive skip-gate tests can
    /// simulate "a real call just landed" without going through the HTTP
    /// path. Gated so it never compiles into release bundles.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn seed_last_call_unix(&self, unix_secs: i64) {
        self.last_any_call.store(unix_secs, Ordering::Relaxed);
    }

    /// Test-only: seed `last_prefix_touch` so re-warm staleness tests
    /// can simulate an aged (or fresh) prompt-prefix cache.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn seed_last_prefix_touch_unix(&self, unix_secs: i64) {
        self.last_prefix_touch.store(unix_secs, Ordering::Relaxed);
    }
}

/// Seconds elapsed since a stored unix-seconds timestamp. `0` ("never")
/// reads as [`i64::MAX`] so gates that test "stale enough?" always
/// treat a never-touched clock as maximally stale. Clamped at `0` so a
/// backwards wall-clock step (NTP correction) can't yield a negative
/// age.
fn secs_since(stored: i64) -> i64 {
    if stored == 0 {
        return i64::MAX;
    }
    (now_unix() - stored).max(0)
}

/// Wall-clock unix seconds (UTC), saturating to `0` on the impossible
/// pre-1970 case. Mirrors the same 3-line helper in `groq_warmup` /
/// `session` — repeated rather than shared to avoid leaking a module's
/// internals across boundaries (CLAUDE.md §2).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_tracker_reads_as_never() {
        let a = GroqActivity::new();
        assert_eq!(a.secs_since_call(), i64::MAX);
        assert_eq!(a.secs_since_prefix_touch(), i64::MAX);
    }

    #[test]
    fn note_call_makes_recent_and_leaves_prefix_untouched() {
        let a = GroqActivity::new();
        a.note_call();
        assert!(
            a.secs_since_call() < 5,
            "a just-noted call must read as seconds-fresh"
        );
        // note_call must NOT warm the prefix clock — that would let
        // ASR/LID traffic wrongly suppress the cache re-warm.
        assert_eq!(
            a.secs_since_prefix_touch(),
            i64::MAX,
            "note_call must not touch last_prefix_touch"
        );
    }

    #[test]
    fn note_prefix_touch_bumps_both_clocks() {
        let a = GroqActivity::new();
        a.note_prefix_touch();
        assert!(a.secs_since_prefix_touch() < 5);
        assert!(
            a.secs_since_call() < 5,
            "a prefix touch also counts as recent Groq traffic"
        );
    }

    #[test]
    fn seeded_timestamps_drive_the_gates() {
        let a = GroqActivity::new();
        // A call 10 min ago → not recent for the 240 s keepalive gate.
        a.seed_last_call_unix(now_unix() - 600);
        assert!(a.secs_since_call() >= 600);
        assert!(a.secs_since_call() < 601 + 5);

        // A prefix touch 2 h ago → stale for the 90 min re-warm gate.
        a.seed_last_prefix_touch_unix(now_unix() - 7_200);
        assert!(a.secs_since_prefix_touch() >= 7_200);
    }

    #[test]
    fn backwards_clock_step_clamps_to_zero() {
        let a = GroqActivity::new();
        // Timestamp in the future (wall clock stepped back) → age must
        // clamp to 0, never go negative.
        a.seed_last_call_unix(now_unix() + 3_600);
        assert_eq!(a.secs_since_call(), 0);
    }
}
