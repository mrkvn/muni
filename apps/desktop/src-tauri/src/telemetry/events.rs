//! Typed PostHog operational-health events (Feature 033, Phase 2).
//!
//! Every event the desktop app sends to PostHog is built HERE, through a typed
//! constructor that returns a [`PostHogEvent`] whose `properties` are drawn from
//! an explicit metadata allowlist. No caller can attach a free-form property
//! map, so transcript content, audio, or anything derived from what the user
//! said can never reach the wire by construction.
//!
//! ## The privacy line (non-negotiable)
//!
//! Properties carry operational metadata ONLY — latency, error codes, ASR path,
//! model ids, app/OS version, and feature-usage COUNTS. A bare char *count*
//! (a number) is fine; characters never are. Continuous values (latency,
//! audio duration, char count) are **bucketed** so a single event can't
//! fingerprint a specific utterance by its exact shape, and so dashboard
//! cardinality stays low.
//!
//! ## Single-project segmentation (NOTES requirement)
//!
//! Muni uses ONE shared PostHog project (id 462047) for desktop + website +
//! dev/prod. The split is done with two properties present on **every** event:
//! [`PROP_SOURCE`] (always `muni-desktop` here) and [`PROP_ENVIRONMENT`]
//! (`prod` | `dev`, from the build flavor). [`PostHogEvent::new`] stamps both,
//! plus `$process_person_profile: false` (anonymous events are ~4× cheaper —
//! person profiles are only ever set on a low-frequency Phase 3 event).

use serde_json::{Map, Value};

use super::flavor;

// --- Mandatory properties (single-project segmentation) ---------------------

/// Property naming the ingestion surface. Always `muni-desktop` from this app;
/// the website reuses the same project key with `muni-web`.
pub const PROP_SOURCE: &str = "source";
/// The fixed `source` value for the desktop app.
pub const SOURCE_DESKTOP: &str = "muni-desktop";

/// Property naming the build environment (`prod` | `dev`). Dashboards filter on
/// `environment:prod`; without it the single-project split can't separate
/// dogfood noise from real users.
pub const PROP_ENVIRONMENT: &str = "environment";

/// PostHog reserved property: when `false`, the event is processed without
/// creating/updating a person profile (the ~4× cheaper anonymous path). All
/// high-frequency operational events set this; only a dedicated low-frequency
/// retention event (Phase 3) opts back in.
pub const PROP_PROCESS_PERSON_PROFILE: &str = "$process_person_profile";

/// PostHog reserved property: when `true`, PostHog skips server-side GeoIP
/// enrichment, so the ingestion IP is never turned into country / city /
/// latitude / longitude / timezone on the person profile. We POST events from
/// the Rust backend (the real client IP is visible to PostHog) and the consent
/// card promises *anonymous* metadata — location is never wanted.
pub const PROP_GEOIP_DISABLE: &str = "$geoip_disable";

/// PostHog reserved property: overrides the client IP PostHog would otherwise
/// read from the ingestion connection. A *null* `$ip` does NOT work — PostHog
/// falls back to the real connection IP — so we send a non-routable placeholder
/// ([`IP_PLACEHOLDER`]) to ensure the user's actual IP is never stored.
pub const PROP_IP: &str = "$ip";

/// Non-routable placeholder sent as `$ip` so PostHog never records the real
/// client IP (from which location could be re-derived). Belt-and-suspenders
/// alongside [`PROP_GEOIP_DISABLE`].
pub const IP_PLACEHOLDER: &str = "0.0.0.0";

/// PostHog reserved property: a per-event idempotency key. PostHog dedupes
/// events that share an `$insert_id`, so the durable queue's at-least-once
/// retry (a flush whose 2xx response is lost on a flaky reconnect) delivers
/// effectively-once instead of double-counting. Generated at construction and
/// frozen into the queued payload, so every retry of the same event reuses it.
pub const PROP_INSERT_ID: &str = "$insert_id";

// --- Event names ------------------------------------------------------------

pub const EVENT_DICTATION_COMPLETED: &str = "dictation_completed";
pub const EVENT_FALLBACK_FIRED: &str = "fallback_fired";
pub const EVENT_DICTATION_FAILED: &str = "dictation_failed";
pub const EVENT_DICTATION_EMPTY: &str = "dictation_empty";
/// Deferred to Phase 3 (task 17): the `rapid_retry` frustration signal needs
/// cheaply-available prior-press timing, which the session doesn't track today.
/// The constructor is kept (and tested) so the wiring is a one-liner once a
/// last-press timestamp exists.
#[allow(dead_code)]
pub const EVENT_RAPID_RETRY: &str = "rapid_retry";

// --- Phase 3 event names ----------------------------------------------------

/// Activation funnel: the first-run wizard mounted (task 21). Fired from the
/// frontend through the [`crate::telemetry::emit_event`] queue via the
/// `telemetry_track` command, so it shares the one durable transport + distinct_id.
pub const EVENT_ONBOARDING_STARTED: &str = "onboarding_started";
/// Activation funnel: a macOS permission was granted, one event per permission
/// (task 21). The `permission` property names which one.
pub const EVENT_PERMISSION_GRANTED: &str = "permission_granted";
/// Activation funnel: the first successful dictation on this install (task 21).
///
/// NOT currently emitted — both "first" and "nth" dictation are derived in
/// PostHog from `dictation_completed` ordinality per the stable install-UUID
/// distinct_id (the plan explicitly allows "rely on `dictation_completed`
/// ordinality"). A dedicated emit would need a non-racy "have I ever pasted?"
/// check against the just-inserted (fire-and-forget) history row; the funnel
/// doesn't need it, so we don't take that risk. The constructor is kept + tested
/// so wiring it is trivial if a discrete event is ever wanted.
#[allow(dead_code)]
pub const EVENT_FIRST_DICTATION: &str = "first_dictation";

/// Retention identity: the single low-frequency, person-profiled event emitted
/// once per launch (task 22). The ONLY event that carries `$set` person props
/// and opts back into a person profile — everything else stays anonymous.
pub const EVENT_APP_LAUNCHED: &str = "app_launched";

// --- Phase 3 feature-usage event names --------------------------------------

/// Feature-usage: a self-correction marker (`scratch that`, `actually no`, …)
/// fired and rewrote the transcript before cleanup (task 23). A bare COUNT
/// signal — no marker text, no transcript content.
pub const EVENT_SCRATCH_THAT_USED: &str = "scratch_that_used";
/// Feature-usage: the user saved their My Words list (task 23). Carries only the
/// new entry COUNT (a number), never any word text.
pub const EVENT_MY_WORDS_CHANGED: &str = "my_words_changed";

// --- Operational: auto-updater health ---------------------------------------

/// Operational health: a background update download was aborted by the stall
/// watchdog or hard cap (or failed outright) instead of installing. Without this
/// signal a degraded CDN edge produces a *silent* stuck install — the app keeps
/// working on the old version while update delivery quietly stops, with nothing
/// in our telemetry to show it. The `stall_reason` property names which guard
/// fired; the event carries no URL, version, or any user content.
pub const EVENT_UPDATER_DOWNLOAD_STALLED: &str = "updater_download_stalled";

// --- Allowlisted property keys ----------------------------------------------
//
// Every key that may appear in an event's `properties` is named here. The
// constructors below only ever insert these — there is no path to attach an
// arbitrary key, which is the structural guarantee that no transcript content
// leaks.

/// Bucketed end-to-end / cleanup latency (see [`latency_bucket`]).
pub const PROP_LATENCY_BUCKET: &str = "latency_bucket";
/// Which backend served the transcript (`served_by` tag from history_store).
pub const PROP_SERVED_BY: &str = "served_by";
/// Bucketed audio/press duration (see [`duration_bucket`]).
pub const PROP_AUDIO_DURATION_BUCKET: &str = "audio_duration_bucket";
/// Bucketed cleaned-text character count (see [`char_count_bucket`]).
pub const PROP_CHAR_COUNT_BUCKET: &str = "char_count_bucket";
/// Cleanup model id actually used for the press (operational, not content).
pub const PROP_CLEANUP_MODEL: &str = "cleanup_model";
/// `true` when the press completed on the primary path, `false` when it only
/// completed after a fallback/recovery (degraded but successful).
pub const PROP_DEGRADED: &str = "degraded";
/// Target app bundle id, bucketed through [`bundle_id_bucket`] (allowlist + `other`).
pub const PROP_TARGET_APP_BUNDLE_ID: &str = "target_app_bundle_id";
/// Feature 037 — how the completed dictation was delivered: [`DELIVERY_PASTED`]
/// (landed in a focused field, the steady state) or [`DELIVERY_HELD`] (no
/// editable field focused, so it was persisted and surfaced for the re-paste
/// hotkey instead of pasted). A two-value operational bucket — never content.
pub const PROP_DELIVERY: &str = "delivery";
/// The fallback provider that was attempted (operational label).
pub const PROP_FALLBACK_PROVIDER: &str = "fallback_provider";
/// Stable error discriminant (the `MuniError` camelCase kind). Never a message.
pub const PROP_ERROR_KIND: &str = "error_kind";
/// Why a press produced no paste (a small fixed reason vocabulary).
pub const PROP_EMPTY_REASON: &str = "empty_reason";

// --- Phase 3 allowlisted property keys --------------------------------------

/// Which macOS permission was granted (`microphone` | `accessibility` |
/// `input_monitoring`). A fixed vocabulary, validated at the command boundary.
pub const PROP_PERMISSION: &str = "permission";
/// App version string (`CARGO_PKG_VERSION`) — operational, for retention cohorts.
pub const PROP_APP_VERSION: &str = "app_version";
/// OS family (`std::env::consts::OS`). Coarse, non-identifying.
pub const PROP_OS: &str = "os";
/// Language routing the press took (`english` | `tagalog_lid`), derived from the
/// backend that served it. A two-value bucket — never any transcript content.
pub const PROP_LANGUAGE_PATH: &str = "language_path";
/// Number of entries in the saved My Words list (a COUNT, never the words).
pub const PROP_MY_WORDS_COUNT: &str = "my_words_count";

/// Which updater guard aborted a download (`no_progress` | `timeout` | `failed`).
/// A fixed, low-cardinality vocabulary set at the call site — never a message,
/// URL, or version. See [`EVENT_UPDATER_DOWNLOAD_STALLED`].
pub const PROP_STALL_REASON: &str = "stall_reason";

/// PostHog reserved `$set` map: person properties merged onto the profile. Only
/// the [`app_launched`] retention event carries this (task 22) — it's the one
/// event that opts into a person profile.
pub const PROP_SET: &str = "$set";

// --- Sentinel values --------------------------------------------------------

/// Bundle-id bucket for any app not on the allowlist. Keeps cardinality bounded
/// and avoids turning the target app into a fingerprint.
pub const BUNDLE_ID_OTHER: &str = "other";
/// Bundle-id bucket when the frontmost app couldn't be resolved at all.
pub const BUNDLE_ID_UNKNOWN: &str = "unknown";

/// [`PROP_DELIVERY`] value: the dictation pasted into a focused field (the
/// steady state — probe returned Editable/Unknown or the text was non-empty).
pub const DELIVERY_PASTED: &str = "pasted";
/// [`PROP_DELIVERY`] value: no editable field was focused, so the dictation was
/// persisted and surfaced for the re-paste hotkey instead of pasted (Feature
/// 037 confident-negative branch).
pub const DELIVERY_HELD: &str = "held";

/// A handful of high-traffic editors/IDEs/browsers/chat apps we explicitly want
/// to distinguish in dashboards. Everything else buckets to [`BUNDLE_ID_OTHER`]
/// so a niche app can't be used to fingerprint a small population of installs.
const ALLOWED_BUNDLE_IDS: &[&str] = &[
    "com.apple.Safari",
    "com.google.Chrome",
    "company.thebrowser.Browser", // Arc
    "com.apple.dt.Xcode",
    "com.microsoft.VSCode",
    "com.todesktop.230313mzl4w4u92", // Cursor
    "com.apple.Notes",
    "com.apple.mail",
    "com.tinyspeck.slackmacgap", // Slack
    "com.hnc.Discord",
    "notion.id",
    "md.obsidian",
    "com.apple.TextEdit",
    "com.googlecode.iterm2",
    "com.mitchellh.ghostty",
    "com.apple.Terminal",
];

// --- Typed event payload ----------------------------------------------------

/// One PostHog event ready to enqueue. Construct only through the typed
/// constructors in this module — `properties` is private so no caller can splice
/// in an off-allowlist key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostHogEvent {
    /// PostHog `event` name (one of the `EVENT_*` consts).
    pub name: &'static str,
    /// Allowlisted property map. Always contains `source`, `environment`, and
    /// `$process_person_profile` (stamped by [`Self::new`]).
    properties: Map<String, Value>,
}

impl PostHogEvent {
    /// Start an event with the three mandatory properties already set:
    /// `source: muni-desktop`, `environment: <flavor>`, and
    /// `$process_person_profile: false`. Constructors add allowlisted keys on
    /// top via [`Self::insert`].
    fn new(name: &'static str) -> Self {
        let mut properties = Map::new();
        properties.insert(PROP_SOURCE.into(), Value::from(SOURCE_DESKTOP));
        properties.insert(PROP_ENVIRONMENT.into(), Value::from(environment()));
        // Anonymous, cheaper path; only Phase 3's retention event opts back in.
        properties.insert(PROP_PROCESS_PERSON_PROFILE.into(), Value::Bool(false));
        // Privacy: never let PostHog derive or store location/IP from the
        // ingestion request. The consent card promises anonymous metadata only.
        properties.insert(PROP_GEOIP_DISABLE.into(), Value::Bool(true));
        properties.insert(PROP_IP.into(), Value::from(IP_PLACEHOLDER));
        // Idempotency: a stable per-event key, frozen into the queued payload so
        // a flush retry (lost 2xx on a flaky reconnect) can't double-count —
        // PostHog dedupes on $insert_id.
        properties.insert(
            PROP_INSERT_ID.into(),
            Value::from(uuid::Uuid::new_v4().to_string()),
        );
        Self { name, properties }
    }

    /// Insert an allowlisted key. The `key` MUST be one of the `PROP_*` consts
    /// in this module — that's how the no-transcript guarantee is structural
    /// rather than aspirational. Callers outside this module can't reach it
    /// (private), so the allowlist is the set of keys actually used below.
    fn insert(&mut self, key: &'static str, value: impl Into<Value>) {
        self.properties.insert(key.into(), value.into());
    }

    /// Read-only view of the property map (for the queue serializer + tests).
    pub fn properties(&self) -> &Map<String, Value> {
        &self.properties
    }
}

// --- Constructors -----------------------------------------------------------

/// A successful press landed a paste. The richest event — carries latency,
/// served-by path, bucketed audio duration + char count, cleanup model, the
/// degraded flag, and the bucketed target app.
#[allow(clippy::too_many_arguments)]
pub fn dictation_completed(
    cleanup_latency_ms: u64,
    served_by: &str,
    audio_duration_ms: u64,
    char_count: usize,
    cleanup_model: &str,
    degraded: bool,
    target_app_bundle_id: Option<&str>,
    delivery: &str,
) -> PostHogEvent {
    let mut e = PostHogEvent::new(EVENT_DICTATION_COMPLETED);
    e.insert(PROP_LATENCY_BUCKET, latency_bucket(cleanup_latency_ms));
    e.insert(PROP_SERVED_BY, served_by.to_string());
    e.insert(PROP_DELIVERY, delivery.to_string());
    // Feature-usage (task 23): coarse English vs Tagalog/LID routing, derived
    // from the serving backend. A two-value bucket — never transcript content.
    e.insert(PROP_LANGUAGE_PATH, language_path(served_by));
    e.insert(
        PROP_AUDIO_DURATION_BUCKET,
        duration_bucket(audio_duration_ms),
    );
    e.insert(PROP_CHAR_COUNT_BUCKET, char_count_bucket(char_count));
    e.insert(PROP_CLEANUP_MODEL, cleanup_model.to_string());
    e.insert(PROP_DEGRADED, degraded);
    e.insert(
        PROP_TARGET_APP_BUNDLE_ID,
        bundle_id_bucket(target_app_bundle_id),
    );
    e
}

/// The primary ASR path failed and a cross-provider fallback was attempted.
/// `provider` names the fallback that ran (operational label only).
pub fn fallback_fired(provider: &str, original_error_kind: &str) -> PostHogEvent {
    let mut e = PostHogEvent::new(EVENT_FALLBACK_FIRED);
    e.insert(PROP_FALLBACK_PROVIDER, provider.to_string());
    e.insert(PROP_ERROR_KIND, original_error_kind.to_string());
    e
}

/// Terminal transcription failure — no paste landed for this press.
pub fn dictation_failed(error_kind: &str) -> PostHogEvent {
    let mut e = PostHogEvent::new(EVENT_DICTATION_FAILED);
    e.insert(PROP_ERROR_KIND, error_kind.to_string());
    e
}

/// A press produced no output (silent press, noise-only, hallucination gate,
/// too-short buffer). `reason` is a small fixed vocabulary, never content.
pub fn dictation_empty(reason: &str) -> PostHogEvent {
    let mut e = PostHogEvent::new(EVENT_DICTATION_EMPTY);
    e.insert(PROP_EMPTY_REASON, reason.to_string());
    e
}

/// Two presses in quick succession over a similar span — a frustration signal.
/// Carries only the bucketed span (no content).
///
/// Deferred to Phase 3 (task 17): the emit site needs prior-press timing the
/// session doesn't track yet. Kept + tested so wiring is trivial later.
#[allow(dead_code)]
pub fn rapid_retry(audio_duration_ms: u64) -> PostHogEvent {
    let mut e = PostHogEvent::new(EVENT_RAPID_RETRY);
    e.insert(
        PROP_AUDIO_DURATION_BUCKET,
        duration_bucket(audio_duration_ms),
    );
    e
}

// --- Phase 3 constructors: activation funnel (task 21) ----------------------

/// The first-run wizard opened. The top of the activation funnel.
pub fn onboarding_started() -> PostHogEvent {
    PostHogEvent::new(EVENT_ONBOARDING_STARTED)
}

/// A macOS permission was granted. `permission` MUST be one of the values in
/// [`is_known_permission`] — the command boundary validates it, so an
/// unrecognised value is dropped before it ever reaches a constructor.
pub fn permission_granted(permission: &str) -> PostHogEvent {
    let mut e = PostHogEvent::new(EVENT_PERMISSION_GRANTED);
    e.insert(PROP_PERMISSION, permission.to_string());
    e
}

/// The first successful dictation on this install. Reserved — see
/// [`EVENT_FIRST_DICTATION`]; the funnel derives first/nth from
/// `dictation_completed` ordinality, so this isn't wired today. Kept + tested.
#[allow(dead_code)]
pub fn first_dictation() -> PostHogEvent {
    PostHogEvent::new(EVENT_FIRST_DICTATION)
}

/// `true` when `permission` is one of the three macOS permissions we track.
/// The single source of truth for what `permission_granted` accepts; the
/// `telemetry_track` command rejects anything else so a frontend typo (or a
/// hostile caller) can't widen the property's cardinality.
pub fn is_known_permission(permission: &str) -> bool {
    matches!(
        permission,
        "microphone" | "accessibility" | "input_monitoring"
    )
}

// --- Phase 3 constructor: retention identity (task 22) ----------------------

/// The single low-frequency, person-profiled event emitted once per launch.
/// Sets the `$set` person props (`app_version`, `os`) so retention cohorts fall
/// out of the stable install-UUID distinct_id, and flips
/// `$process_person_profile` back to `true` — the ONLY event that does, since a
/// person profile costs ~4× an anonymous event (NOTES).
pub fn app_launched() -> PostHogEvent {
    let mut e = PostHogEvent::new(EVENT_APP_LAUNCHED);
    // Opt this one event back into a person profile (overrides the `false` that
    // `new` stamps) so the `$set` props attach to a durable person.
    e.insert(PROP_PROCESS_PERSON_PROFILE, true);

    // Person properties merged onto the profile. Built here so the retention
    // identity is allowlisted exactly like every flat property.
    let mut set = Map::new();
    set.insert(
        PROP_APP_VERSION.into(),
        Value::from(env!("CARGO_PKG_VERSION")),
    );
    set.insert(PROP_OS.into(), Value::from(std::env::consts::OS));
    e.insert(PROP_SET, Value::Object(set));
    e
}

// --- Phase 3 constructors: feature usage (task 23) --------------------------

/// A self-correction marker fired and rewrote the transcript before cleanup.
/// A bare COUNT signal (one event per occurrence) — no marker text, no content.
pub fn scratch_that_used() -> PostHogEvent {
    PostHogEvent::new(EVENT_SCRATCH_THAT_USED)
}

/// The user saved their My Words list. Carries only the resulting entry COUNT
/// (a number), never any word text — the COUNT is the feature-usage signal.
pub fn my_words_changed(entry_count: usize) -> PostHogEvent {
    let mut e = PostHogEvent::new(EVENT_MY_WORDS_CHANGED);
    e.insert(PROP_MY_WORDS_COUNT, entry_count as u64);
    e
}

// --- Operational constructor: auto-updater health ---------------------------

/// A background update download was aborted before installing. `reason` is the
/// guard that fired (`no_progress` | `timeout` | `failed`) — a fixed vocabulary
/// owned by the updater, never a free-form message. No version/URL/content.
pub fn updater_download_stalled(reason: &str) -> PostHogEvent {
    let mut e = PostHogEvent::new(EVENT_UPDATER_DOWNLOAD_STALLED);
    e.insert(PROP_STALL_REASON, reason.to_string());
    e
}

// --- Bucketing --------------------------------------------------------------

/// Coarse English vs Tagalog/LID routing, derived from the **serving backend**
/// label. Whisper/Gladia serve the Tagalog/LID path; Deepgram/Parakeet serve
/// the English path. Two values only — keeps cardinality minimal and never
/// reflects what was said, only which pipeline ran.
///
/// Convention: this is bucketed by *who served the press*, not by which route
/// was originally chosen. So a Deepgram-route (English) press that Deepgram
/// dropped and Groq Whisper / Gladia rescued (plan 039 slice 4, task 10) counts
/// as `tagalog_lid` here — it was, in fact, served by that pipeline. Because the
/// rescue only fires when Deepgram dies mid-press (rare) and `served_by` alone
/// can't distinguish "LID chose Whisper" from "Deepgram died, Whisper rescued"
/// (both carry `whisper-fallback`), we accept the small skew rather than thread
/// the route decision through the delivery event.
fn language_path(served_by: &str) -> &'static str {
    let lower = served_by.to_ascii_lowercase();
    if lower.contains("whisper") || lower.contains("gladia") {
        "tagalog_lid"
    } else {
        "english"
    }
}

/// Coarse build environment for the mandatory `environment` property. Maps the
/// telemetry [`flavor`] onto PostHog's two-value split: dev/staging both report
/// `dev` (neither is a real user), prod reports `prod`.
fn environment() -> &'static str {
    match flavor() {
        "prod" => "prod",
        _ => "dev",
    }
}

/// Bucket a latency in milliseconds into a coarse band. Keeps cardinality low
/// and prevents the exact ms from being an utterance fingerprint.
fn latency_bucket(ms: u64) -> &'static str {
    match ms {
        0..=499 => "<500ms",
        500..=999 => "500-1000ms",
        1000..=1999 => "1-2s",
        2000..=3999 => "2-4s",
        4000..=7999 => "4-8s",
        _ => ">8s",
    }
}

/// Bucket an audio/press duration in milliseconds into a coarse band.
fn duration_bucket(ms: u64) -> &'static str {
    match ms {
        0..=999 => "<1s",
        1000..=2999 => "1-3s",
        3000..=9999 => "3-10s",
        10000..=29999 => "10-30s",
        _ => ">30s",
    }
}

/// Bucket a cleaned-text character count into a coarse band. The COUNT is fine
/// to know; the exact figure is bucketed so it can't shadow a specific phrase.
fn char_count_bucket(chars: usize) -> &'static str {
    match chars {
        0..=49 => "0-50",
        50..=149 => "50-150",
        150..=399 => "150-400",
        400..=999 => "400-1000",
        _ => ">1000",
    }
}

/// Map a target app bundle id onto the allowlist + `other`/`unknown` buckets.
/// An unrecognised id never reaches the wire verbatim — it buckets to `other`,
/// so a niche app can't be used to fingerprint a small set of installs.
fn bundle_id_bucket(bundle_id: Option<&str>) -> &'static str {
    match bundle_id {
        None => BUNDLE_ID_UNKNOWN,
        Some(id) => ALLOWED_BUNDLE_IDS
            .iter()
            .find(|allowed| **allowed == id)
            .copied()
            .unwrap_or(BUNDLE_ID_OTHER),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Every key any constructor is allowed to emit. A constructor that inserts
    /// a key outside this set fails the allowlist test below — that's the
    /// regression guard against a future edit smuggling transcript content in.
    const ALLOWLIST: &[&str] = &[
        PROP_SOURCE,
        PROP_ENVIRONMENT,
        PROP_PROCESS_PERSON_PROFILE,
        PROP_GEOIP_DISABLE,
        PROP_IP,
        PROP_INSERT_ID,
        PROP_LATENCY_BUCKET,
        PROP_SERVED_BY,
        PROP_AUDIO_DURATION_BUCKET,
        PROP_CHAR_COUNT_BUCKET,
        PROP_CLEANUP_MODEL,
        PROP_DEGRADED,
        PROP_TARGET_APP_BUNDLE_ID,
        PROP_DELIVERY,
        PROP_FALLBACK_PROVIDER,
        PROP_ERROR_KIND,
        PROP_EMPTY_REASON,
        PROP_PERMISSION,
        PROP_LANGUAGE_PATH,
        PROP_MY_WORDS_COUNT,
        PROP_STALL_REASON,
        PROP_SET,
    ];

    /// Keys allowed INSIDE the `$set` person-property map (retention event only).
    const SET_ALLOWLIST: &[&str] = &[PROP_APP_VERSION, PROP_OS];

    fn assert_only_allowlisted(event: &PostHogEvent) {
        let allowed: HashSet<&str> = ALLOWLIST.iter().copied().collect();
        let set_allowed: HashSet<&str> = SET_ALLOWLIST.iter().copied().collect();
        for (key, value) in event.properties() {
            assert!(
                allowed.contains(key.as_str()),
                "event {} carried non-allowlisted property {key:?}",
                event.name
            );
            // The `$set` map is the one nested object; its keys are allowlisted
            // separately so a future person-prop can't smuggle content either.
            if key == PROP_SET {
                let map = value
                    .as_object()
                    .unwrap_or_else(|| panic!("$set must be an object on {}", event.name));
                for set_key in map.keys() {
                    assert!(
                        set_allowed.contains(set_key.as_str()),
                        "event {} $set carried non-allowlisted prop {set_key:?}",
                        event.name
                    );
                }
            }
        }
    }

    /// Mandatory props common to every event: `source` and `environment`. The
    /// person-profile flag differs (retention opts in), so it's checked per-event.
    fn assert_segmentation_present(event: &PostHogEvent) {
        let props = event.properties();
        assert_eq!(
            props.get(PROP_SOURCE).and_then(|v| v.as_str()),
            Some(SOURCE_DESKTOP),
            "every event must carry source:muni-desktop"
        );
        let env = props.get(PROP_ENVIRONMENT).and_then(|v| v.as_str());
        assert!(
            env == Some("prod") || env == Some("dev"),
            "every event must carry environment prod|dev, got {env:?}"
        );
        // Regression guard for the dogfood finding: PostHog was deriving
        // country/lat-long/timezone from the ingestion IP. Every event must
        // suppress GeoIP and IP capture so no location data is ever stored.
        assert_eq!(
            props.get(PROP_GEOIP_DISABLE),
            Some(&Value::Bool(true)),
            "every event must disable GeoIP"
        );
        assert_eq!(
            props.get(PROP_IP).and_then(|v| v.as_str()),
            Some(IP_PLACEHOLDER),
            "every event must override $ip with the non-routable placeholder"
        );
        // Idempotency key present + non-empty so PostHog can dedupe retries.
        let insert_id = props.get(PROP_INSERT_ID).and_then(|v| v.as_str());
        assert!(
            insert_id.is_some_and(|s| !s.is_empty()),
            "every event must carry a non-empty $insert_id"
        );
    }

    fn assert_mandatory_present(event: &PostHogEvent) {
        assert_segmentation_present(event);
        assert_eq!(
            event.properties().get(PROP_PROCESS_PERSON_PROFILE),
            Some(&Value::Bool(false)),
            "high-frequency events must opt out of person profiles"
        );
    }

    #[test]
    fn every_constructor_emits_only_allowlisted_keys_with_mandatory_props() {
        let events = [
            dictation_completed(
                850,
                "parakeet-local",
                4200,
                123,
                "openai/gpt-oss-20b",
                false,
                Some("com.apple.Safari"),
                DELIVERY_PASTED,
            ),
            fallback_fired("gladia", "groqServerError"),
            dictation_failed("transcriptionUnavailable"),
            dictation_empty("silent_press"),
            rapid_retry(1500),
        ];
        for event in &events {
            assert_only_allowlisted(event);
            assert_mandatory_present(event);
        }
    }

    #[test]
    fn unknown_bundle_id_buckets_to_other() {
        assert_eq!(bundle_id_bucket(Some("com.evil.nicheapp")), BUNDLE_ID_OTHER);
    }

    #[test]
    fn known_bundle_id_passes_through() {
        assert_eq!(
            bundle_id_bucket(Some("com.microsoft.VSCode")),
            "com.microsoft.VSCode"
        );
    }

    #[test]
    fn missing_bundle_id_buckets_to_unknown() {
        assert_eq!(bundle_id_bucket(None), BUNDLE_ID_UNKNOWN);
    }

    #[test]
    fn completed_event_carries_no_raw_text_field() {
        // Belt-and-suspenders: even if a bucket value were mis-wired, there is
        // no "raw_text"/"cleaned_text"/"text" property anywhere on the event.
        let e = dictation_completed(100, "deepgram", 1000, 10, "m", false, None, DELIVERY_PASTED);
        for forbidden in ["raw_text", "cleaned_text", "text", "transcript", "audio"] {
            assert!(
                !e.properties().contains_key(forbidden),
                "found forbidden content key {forbidden:?}"
            );
        }
    }

    #[test]
    fn latency_buckets_cover_boundaries() {
        assert_eq!(latency_bucket(0), "<500ms");
        assert_eq!(latency_bucket(499), "<500ms");
        assert_eq!(latency_bucket(500), "500-1000ms");
        assert_eq!(latency_bucket(1500), "1-2s");
        assert_eq!(latency_bucket(9000), ">8s");
    }

    #[test]
    fn char_count_buckets_cover_boundaries() {
        assert_eq!(char_count_bucket(0), "0-50");
        assert_eq!(char_count_bucket(49), "0-50");
        assert_eq!(char_count_bucket(50), "50-150");
        assert_eq!(char_count_bucket(5000), ">1000");
    }

    #[test]
    fn duration_buckets_cover_boundaries() {
        assert_eq!(duration_bucket(0), "<1s");
        assert_eq!(duration_bucket(999), "<1s");
        assert_eq!(duration_bucket(1000), "1-3s");
        assert_eq!(duration_bucket(60000), ">30s");
    }

    #[test]
    fn environment_is_dev_under_debug_assertions() {
        // The test binary builds with debug_assertions → flavor() == "dev".
        assert_eq!(environment(), "dev");
    }

    // --- Phase 3 (Slice 6) ---------------------------------------------------

    #[test]
    fn phase3_constructors_emit_only_allowlisted_keys() {
        // Anonymous Phase 3 events keep the cheap no-person-profile path.
        let anonymous = [
            onboarding_started(),
            permission_granted("microphone"),
            first_dictation(),
            scratch_that_used(),
            my_words_changed(7),
        ];
        for event in &anonymous {
            assert_only_allowlisted(event);
            assert_mandatory_present(event);
        }
    }

    #[test]
    fn app_launched_is_the_only_person_profiled_event() {
        let e = app_launched();
        assert_only_allowlisted(&e);
        assert_segmentation_present(&e);
        // The single exception to the anonymous default: retention opts in.
        assert_eq!(
            e.properties().get(PROP_PROCESS_PERSON_PROFILE),
            Some(&Value::Bool(true)),
            "the retention event must carry a person profile"
        );
        // `$set` carries exactly the two retention props, no content.
        let set = e
            .properties()
            .get(PROP_SET)
            .and_then(|v| v.as_object())
            .expect("$set present");
        assert_eq!(
            set.get(PROP_OS).and_then(|v| v.as_str()),
            Some(std::env::consts::OS)
        );
        assert!(set.contains_key(PROP_APP_VERSION));
    }

    #[test]
    fn permission_allowlist_rejects_unknown_values() {
        assert!(is_known_permission("microphone"));
        assert!(is_known_permission("accessibility"));
        assert!(is_known_permission("input_monitoring"));
        assert!(!is_known_permission("camera"));
        assert!(!is_known_permission(""));
    }

    #[test]
    fn language_path_buckets_to_two_values() {
        assert_eq!(language_path("deepgram"), "english");
        assert_eq!(language_path(SERVED_BY_PARAKEET_LOCAL_FOR_TEST), "english");
        assert_eq!(language_path("whisper-fallback"), "tagalog_lid");
        assert_eq!(language_path("gladia-primary"), "tagalog_lid");
        // Unknown labels fall back to English (the primary path) rather than
        // inventing a third bucket.
        assert_eq!(language_path("some-future-backend"), "english");
    }

    // Local mirror of history_store's parakeet label so this test doesn't reach
    // across modules just to assert the bucket mapping.
    const SERVED_BY_PARAKEET_LOCAL_FOR_TEST: &str = "parakeet-local";

    #[test]
    fn dictation_completed_carries_language_path() {
        let e = dictation_completed(
            100,
            "whisper-fallback",
            1000,
            10,
            "m",
            true,
            None,
            DELIVERY_PASTED,
        );
        assert_eq!(
            e.properties()
                .get(PROP_LANGUAGE_PATH)
                .and_then(|v| v.as_str()),
            Some("tagalog_lid")
        );
    }

    #[test]
    fn dictation_completed_carries_delivery_outcome() {
        // Feature 037 — the delivery tag distinguishes a normal paste from a
        // "held for re-paste" no-field completion without inventing a new event.
        let held = dictation_completed(100, "deepgram", 1000, 10, "m", false, None, DELIVERY_HELD);
        assert_eq!(
            held.properties()
                .get(PROP_DELIVERY)
                .and_then(|v| v.as_str()),
            Some(DELIVERY_HELD)
        );
        assert_only_allowlisted(&held);
        assert_mandatory_present(&held);
    }

    #[test]
    fn updater_download_stalled_carries_only_reason() {
        for reason in ["no_progress", "timeout", "failed"] {
            let e = updater_download_stalled(reason);
            assert_only_allowlisted(&e);
            assert_mandatory_present(&e);
            assert_eq!(
                e.properties()
                    .get(PROP_STALL_REASON)
                    .and_then(|v| v.as_str()),
                Some(reason)
            );
            // Belt-and-suspenders: never a URL, version, or message on this event.
            for forbidden in ["url", "version", "message", "error", "path"] {
                assert!(!e.properties().contains_key(forbidden));
            }
        }
    }

    #[test]
    fn my_words_changed_carries_count_not_words() {
        let e = my_words_changed(3);
        assert_eq!(
            e.properties()
                .get(PROP_MY_WORDS_COUNT)
                .and_then(|v| v.as_u64()),
            Some(3)
        );
        // Belt-and-suspenders: no word-bearing property anywhere.
        for forbidden in ["trigger", "replacement", "word", "words", "text"] {
            assert!(!e.properties().contains_key(forbidden));
        }
    }
}
