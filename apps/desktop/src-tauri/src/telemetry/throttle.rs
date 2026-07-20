//! Per-fingerprint daily throttle for Sentry `before_send` (Feature 033).
//!
//! Free-tier discipline: a single recurring crash (e.g. a hot-path panic that
//! fires every press) must not burn the whole monthly error quota in one bad
//! session. [`Throttle`] caps each distinct event *fingerprint* to
//! [`MAX_PER_FINGERPRINT_PER_DAY`] captures per UTC day; the count resets when
//! the day rolls.
//!
//! `before_send` is invoked from whatever thread captured the event, so the
//! state lives behind a `Mutex`. An in-memory cap is acceptable for crashes —
//! a hard crash usually ends the process anyway, and the cross-launch
//! crash-loop guard ([`crate::boot_health`]) handles the "every launch dies"
//! case. Disk persistence is deliberately omitted (it would add I/O to the
//! pre-abort path for marginal benefit).

use std::collections::HashMap;
use std::sync::Mutex;

use sentry::protocol::Event;

/// Max captures per distinct fingerprint per UTC day. Five lets a genuinely
/// novel crash through a few times (enough to confirm it's real and gather a
/// couple of variants) while clamping a tight crash loop.
pub const MAX_PER_FINGERPRINT_PER_DAY: u32 = 5;

/// Day-keyed per-fingerprint counters behind a `Mutex`.
struct ThrottleState {
    /// UTC day number (unix-day) the `counts` belong to. A roll past this day
    /// clears `counts` on the next [`allow`] call.
    day: u64,
    counts: HashMap<String, u32>,
}

/// Thread-safe daily throttle. One instance is held for the program lifetime
/// inside the Sentry `before_send` closure.
pub struct Throttle {
    state: Mutex<ThrottleState>,
}

impl Throttle {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ThrottleState {
                day: 0,
                counts: HashMap::new(),
            }),
        }
    }

    /// Returns `true` if an event with `fingerprint_key` may be sent on
    /// `now_day` (a UTC day number), incrementing its counter. Returns `false`
    /// once the key has already hit [`MAX_PER_FINGERPRINT_PER_DAY`] that day.
    /// A day-roll (`now_day` past the stored day) resets all counters first.
    ///
    /// Fails OPEN on a poisoned mutex (a panic mid-`before_send`) — telemetry
    /// throttling must never itself drop a crash report by panicking.
    pub fn allow(&self, fingerprint_key: &str, now_day: u64) -> bool {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if now_day != state.day {
            state.day = now_day;
            state.counts.clear();
        }

        let count = state.counts.entry(fingerprint_key.to_string()).or_insert(0);
        if *count >= MAX_PER_FINGERPRINT_PER_DAY {
            return false;
        }
        *count += 1;
        true
    }
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new()
    }
}

/// Sentry's sentinel for "no explicit fingerprint — use server-side default
/// grouping". `Event::new()` seeds the fingerprint with exactly this, so we
/// treat it as absent and fall through to the exception/message keys.
const DEFAULT_FINGERPRINT: &str = "{{ default }}";

/// Derive a stable throttle key for `event`. Priority mirrors how Sentry itself
/// groups issues: an explicit `fingerprint` if set, else the top exception's
/// `type:value`, else the event message, else a catch-all so an event with
/// none of the above still counts against a single bucket.
pub fn fingerprint_key(event: &Event<'_>) -> String {
    let is_default_fingerprint =
        event.fingerprint.len() == 1 && event.fingerprint[0] == DEFAULT_FINGERPRINT;
    if !event.fingerprint.is_empty() && !is_default_fingerprint {
        return event.fingerprint.join("|");
    }
    if let Some(exc) = event.exception.values.last() {
        let value = exc.value.as_deref().unwrap_or("");
        return format!("{}:{}", exc.ty, value);
    }
    if let Some(message) = &event.message {
        return message.clone();
    }
    "<no-fingerprint>".to_string()
}

/// Current UTC day number (days since the unix epoch). Pure modulo on
/// wall-clock seconds; callers pass the result into [`Throttle::allow`] so the
/// throttle stays testable with synthetic day numbers.
pub fn current_utc_day() -> u64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs / 86_400
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn under_cap_allows() {
        let t = Throttle::new();
        for _ in 0..MAX_PER_FINGERPRINT_PER_DAY {
            assert!(t.allow("panic:boom", 100));
        }
    }

    #[test]
    fn at_cap_drops() {
        let t = Throttle::new();
        for _ in 0..MAX_PER_FINGERPRINT_PER_DAY {
            assert!(t.allow("panic:boom", 100));
        }
        // The (MAX+1)th is dropped, and stays dropped.
        assert!(!t.allow("panic:boom", 100));
        assert!(!t.allow("panic:boom", 100));
    }

    #[test]
    fn distinct_fingerprints_have_independent_budgets() {
        let t = Throttle::new();
        for _ in 0..MAX_PER_FINGERPRINT_PER_DAY {
            assert!(t.allow("panic:a", 100));
        }
        assert!(!t.allow("panic:a", 100));
        // A different fingerprint still has its full budget.
        assert!(t.allow("panic:b", 100));
    }

    #[test]
    fn day_roll_resets_counts() {
        let t = Throttle::new();
        for _ in 0..MAX_PER_FINGERPRINT_PER_DAY {
            assert!(t.allow("panic:boom", 100));
        }
        assert!(!t.allow("panic:boom", 100));
        // New UTC day → budget restored.
        assert!(t.allow("panic:boom", 101));
    }

    #[test]
    fn fingerprint_key_prefers_explicit_fingerprint() {
        let mut event = Event::new();
        event.fingerprint = vec![Cow::Borrowed("custom"), Cow::Borrowed("group")].into();
        assert_eq!(fingerprint_key(&event), "custom|group");
    }

    #[test]
    fn fingerprint_key_falls_back_to_message() {
        let mut event = Event::new();
        event.message = Some("a deliberate test message".to_string());
        assert_eq!(fingerprint_key(&event), "a deliberate test message");
    }

    #[test]
    fn fingerprint_key_catch_all_when_empty() {
        let event = Event::new();
        assert_eq!(fingerprint_key(&event), "<no-fingerprint>");
    }
}
