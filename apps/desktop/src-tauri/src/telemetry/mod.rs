//! Post-launch monitoring subsystem (Feature 033).
//!
//! This module owns the Sentry crash/error spine: config resolution (a baked
//! DSN, prod-only), the `before_send` privacy + free-tier pipeline (PII scrub →
//! per-fingerprint throttle → non-fatal sampling), and [`init_sentry`], which
//! wires `sentry::init` together with `sentry-rust-minidump` for abort-safe
//! native crash capture.
//!
//! ## The privacy line (non-negotiable)
//!
//! Telemetry carries operational metadata ONLY — never transcript content,
//! audio, or anything derived from what the user said. The crash spine enforces
//! this two ways: (1) [`scrub_path`] strips the macOS home directory (which
//! contains the OS username) out of every frame path and string field, and
//! (2) `MuniError` is only ever captured via its user-facing message, which is
//! built to omit provider response bodies (see `error.rs` — `GroqServerError`'s
//! `user_message` surfaces the status code, not the body). The Sentry default
//! PII scrubbers (`send_default_pii = false`) are left on as a backstop.
//!
//! ## Gating
//!
//! Capture is OFF (no `sentry::init`) unless ALL of: a DSN is resolvable (prod
//! bakes one; dev/staging do not), the user's crash-reporting toggle is on, and
//! the app is not in crash-loop safe mode. Once a crash loop trips
//! ([`crate::boot_health`]), we stop firing so a self-healing update isn't
//! drowned out by repeated capture of the same boot crash.

pub mod events;
pub mod install_id;
pub mod posthog;
pub mod throttle;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;

use sentry::{ClientInitGuard, ClientOptions, Level};

use events::PostHogEvent;
use posthog::{PostHogConfig, TelemetryQueue};
use throttle::Throttle;

/// Runtime override env var for the Sentry DSN. Used in dev to point a build at
/// the `muni-dev` Sentry project (`export MUNI_SENTRY_DSN="$MUNI_SENTRY_DSN_DEV"`)
/// — dev/staging builds bake NO DSN, so this is the only way they light up.
const SENTRY_DSN_ENV: &str = "MUNI_SENTRY_DSN";

/// Runtime override env var for the PostHog ingestion key. Mirrors the DSN
/// override; consumed by the Phase 2 analytics queue, resolved here so both
/// telemetry identities share one resolution convention.
const POSTHOG_KEY_ENV: &str = "MUNI_POSTHOG_KEY";

/// Fraction of *non-fatal* (handled) errors kept. Fatal events (crashes,
/// panics) are always kept. Ten percent keeps the free-tier error quota safe
/// against a chatty handled-error path while preserving a representative sample.
const NON_FATAL_SAMPLE_RATE: f64 = 0.10;

// --- Config resolution (env → baked → default) ------------------------------

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

/// A value baked into the binary at build time (`build.rs` forwards
/// `MUNI_BAKED_*` into the crate via `rustc-env`). Trims and drops empties so an
/// absent/blank bake falls through to the next layer. A Finder-launched `.app`
/// inherits no shell env, so the baked layer is the only way prod learns its DSN.
fn baked(value: Option<&'static str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Resolve the Sentry DSN: runtime env override → build-time bake → `None`.
/// `None` (the dev/staging default) disables crash capture entirely.
pub fn resolve_sentry_dsn() -> Option<String> {
    env_nonempty(SENTRY_DSN_ENV).or_else(|| baked(option_env!("MUNI_BAKED_SENTRY_DSN")))
}

/// Resolve the PostHog ingestion key: runtime env override → build-time bake →
/// `None`. `None` (dev/staging default) disables analytics.
pub fn resolve_posthog_key() -> Option<String> {
    env_nonempty(POSTHOG_KEY_ENV).or_else(|| baked(option_env!("MUNI_BAKED_POSTHOG_KEY")))
}

/// Coarse build flavor for the `flavor` Sentry tag. Derived from the same baked
/// keychain suffix `secrets.rs` uses to isolate staging, plus `debug_assertions`
/// for dev. Kept coarse on purpose (no machine-identifying detail).
fn flavor() -> &'static str {
    if cfg!(debug_assertions) {
        return "dev";
    }
    match option_env!("MUNI_BAKED_KEYCHAIN_SUFFIX") {
        Some(s) if !s.trim().is_empty() => "staging",
        _ => "prod",
    }
}

// --- Pre-runtime boot reads -------------------------------------------------
//
// Sentry must be initialised BEFORE the Tauri runtime starts (Sentry registers
// global state before threads spawn, and `sentry-rust-minidump` re-launches the
// exe as a monitor — so the less boot code that runs before its init, the
// better). At that point there is no `AppHandle` yet, so we cannot ask Tauri for
// `app_local_data_dir()` or load the settings store. We instead resolve the
// macOS app-data dir from `$HOME` + the compile-time bundle identifier (which
// matches Tauri's resolution), and read the crash toggle straight out of the
// `settings.json` store file. The boot-health read fails OPEN, but the crash
// toggle fails CLOSED on a present-but-unreadable/corrupt store — see
// `read_consent_toggle` for that contract.

/// macOS bundle identifier per flavor, matching the `identifier` field in each
/// `tauri.*.conf.json`. Compile-time-known because the conf is chosen at build.
/// `app_local_data_dir()` on macOS is `$HOME/Library/Application Support/<id>`.
fn bundle_identifier() -> &'static str {
    match flavor() {
        "dev" => "com.muni.app.dev",
        "staging" => "com.muni.app.staging",
        _ => "com.muni.app",
    }
}

/// Resolve `app_local_data_dir` without an `AppHandle` (pre-runtime). Mirrors
/// Tauri's macOS path. Returns `None` if `$HOME` is unset (then the caller
/// falls back to an ephemeral install id and skips file-backed reads).
pub fn app_local_data_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        std::path::PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join(bundle_identifier()),
    )
}

/// Read a boolean consent toggle directly from the `settings.json` store file,
/// distinguishing two failure classes on purpose (plan 039 task 40a):
///
/// - **Key/store absent** (no settings file yet — a fresh install — or the key
///   simply isn't present): honour the setting's default-on value, so a new
///   user still gets telemetry. No opt-out could have been recorded when
///   nothing was ever persisted, so defaulting on is safe.
/// - **Store present but unreadable** (an I/O/permission error reading an
///   existing file, or corrupt JSON): fail **CLOSED** — telemetry off for this
///   boot. The user may have opted out and we simply can't see it; re-enrolling
///   them silently would violate consent. This is the conservative direction.
///
/// This deliberately reads the raw file rather than going through
/// `tauri-plugin-store`: that plugin silently discards file-load errors and
/// hands back an EMPTY store for a corrupt/unreadable-but-present settings file,
/// which would collapse the "store unreadable" case into "key absent" and
/// re-enrol an opted-out user (the exact consent violation this guards against).
pub fn read_consent_toggle(app_local_data_dir: &std::path::Path, key: &str) -> bool {
    const DEFAULT_ON: bool = true;
    let path = app_local_data_dir.join(crate::settings::SETTINGS_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        // No settings file yet (fresh install): key-absent, not a store failure.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return DEFAULT_ON,
        // The store EXISTS but can't be read (permissions, I/O). Fail closed.
        Err(err) => {
            log::warn!(
                target: "telemetry",
                "consent toggle store unreadable for {key} ({err}) — failing closed, telemetry off this boot"
            );
            return false;
        }
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        // Corrupt store: same reasoning as an unreadable one — fail closed
        // rather than re-enrolling a user who may have opted out.
        log::warn!(
            target: "telemetry",
            "consent toggle store corrupt (invalid JSON) for {key} — failing closed, telemetry off this boot"
        );
        return false;
    };
    json.get(key)
        .and_then(|v| v.as_bool())
        .unwrap_or(DEFAULT_ON)
}

/// Read the crash-reporting toggle directly from the `settings.json` store file.
/// Thin wrapper over [`read_consent_toggle`] pinned to the crash-reporting key;
/// see that function for the fail-open/fail-closed contract.
pub fn read_crash_toggle(app_local_data_dir: &std::path::Path) -> bool {
    read_consent_toggle(
        app_local_data_dir,
        crate::settings::KEY_CRASH_REPORTING_ENABLED,
    )
}

/// Read the `boot_health` unclean-launch counter directly (pre-runtime), so the
/// caller can gate Sentry on crash-loop safe mode. Mirrors `boot_health`'s
/// fail-open read: a missing/garbage/unreadable marker reads as 0.
pub fn read_boot_health_count(app_local_data_dir: &std::path::Path) -> u32 {
    let path = app_local_data_dir.join("boot_health");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

// --- PII scrubbing ----------------------------------------------------------

/// Replace any macOS home path (`/Users/<name>/…`) with `/Users/<redacted>/…`.
///
/// Structure-based, not prefix-based: it matches `/Users/` followed by ANY
/// single path segment (username can be any shape — a UUID, a hex string, an
/// email local-part), so a random username is caught just as reliably as a
/// known one. Returns the input unchanged when no such path is present, so it's
/// cheap to call on every string field.
pub fn scrub_path(input: &str) -> String {
    const PREFIX: &str = "/Users/";
    if !input.contains(PREFIX) {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(idx) = rest.find(PREFIX) {
        // Copy everything up to and including the `/Users/` marker.
        let after_prefix_start = idx + PREFIX.len();
        out.push_str(&rest[..after_prefix_start]);
        let tail = &rest[after_prefix_start..];
        // The username segment runs until the next path separator (or end).
        let seg_end = tail.find('/').unwrap_or(tail.len());
        out.push_str("<redacted>");
        rest = &tail[seg_end..];
    }
    out.push_str(rest);
    out
}

/// Apply [`scrub_path`] across EVERY place a home path can surface in an event.
///
/// A native event built by `capture_message`/`capture_error` with
/// `attach_stacktrace` carries `/Users/<name>/…` in far more than the message
/// and exception frames: the current thread's stacktrace, each frame's
/// `package`/`module` (not just `abs_path`/`filename`), and the absolute dylib
/// paths in `debug_meta` images. The macOS hostname (`server_name`) is also
/// `<username>.local`, itself PII. Miss any one and the username leaks — so we
/// scrub all of them, plus drop the hostname outright.
fn scrub_event(mut event: sentry::protocol::Event<'static>) -> sentry::protocol::Event<'static> {
    if let Some(message) = event.message.take() {
        event.message = Some(scrub_path(&message));
    }
    // The hostname is `<username>.local` on macOS — drop it entirely; we never
    // need per-machine identity (the anonymous install UUID is the only id).
    event.server_name = None;

    for exception in &mut event.exception.values {
        if let Some(value) = exception.value.take() {
            exception.value = Some(scrub_path(&value));
        }
        if let Some(stacktrace) = &mut exception.stacktrace {
            scrub_frames(&mut stacktrace.frames);
        }
    }
    if let Some(stacktrace) = &mut event.stacktrace {
        scrub_frames(&mut stacktrace.frames);
    }
    // `capture_message` + `attach_stacktrace` attaches frames under THREADS,
    // not `exception` — the original miss that leaked the home path in test
    // events and would leak it in any non-panic capture.
    for thread in &mut event.threads.values {
        if let Some(stacktrace) = &mut thread.stacktrace {
            scrub_frames(&mut stacktrace.frames);
        }
    }
    // Native debug images carry the absolute dylib path; the `debug_id`/`uuid`
    // is what matches the uploaded dSYM, so scrubbing the path leaves
    // symbolication intact.
    for image in &mut event.debug_meta.to_mut().images {
        scrub_debug_image(image);
    }
    // Breadcrumbs are the log-trail attached to an event. Their `message` and
    // arbitrary `data` values can carry a home path (a future log integration
    // could funnel path-bearing context here), so scrub both — same PII pass,
    // no exception (plan 039 task 40c).
    for breadcrumb in &mut event.breadcrumbs.values {
        if let Some(message) = breadcrumb.message.take() {
            breadcrumb.message = Some(scrub_path(&message));
        }
        for value in breadcrumb.data.values_mut() {
            scrub_json_value(value);
        }
    }
    event
}

/// Recursively scrub `/Users/<name>/…` home paths out of every string inside a
/// JSON value. Breadcrumb `data` is arbitrary structured JSON, so we walk
/// nested objects and arrays in full rather than only its top-level strings.
fn scrub_json_value(value: &mut serde_json::Value) {
    match value {
        // `scrub_path` is a no-op (bar a clone) when there's no home path, so
        // scrub unconditionally rather than pre-checking `contains`.
        serde_json::Value::String(s) => *s = scrub_path(s),
        serde_json::Value::Array(items) => {
            for item in items {
                scrub_json_value(item);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                scrub_json_value(v);
            }
        }
        // Numbers/bools/null carry no path.
        _ => {}
    }
}

fn scrub_frames(frames: &mut [sentry::protocol::Frame]) {
    for frame in frames {
        if let Some(abs_path) = frame.abs_path.take() {
            frame.abs_path = Some(scrub_path(&abs_path));
        }
        if let Some(filename) = frame.filename.take() {
            frame.filename = Some(scrub_path(&filename));
        }
        if let Some(package) = frame.package.take() {
            frame.package = Some(scrub_path(&package));
        }
        if let Some(module) = frame.module.take() {
            frame.module = Some(scrub_path(&module));
        }
    }
}

/// Scrub the absolute path fields of a debug image (the `name`, a.k.a.
/// `code_file`, plus any `debug_file`). The `uuid`/`debug_id` is untouched so
/// Sentry can still match the uploaded dSYM.
fn scrub_debug_image(image: &mut sentry::protocol::DebugImage) {
    use sentry::protocol::DebugImage;
    match image {
        DebugImage::Apple(img) => img.name = scrub_path(&img.name),
        DebugImage::Symbolic(img) => {
            img.name = scrub_path(&img.name);
            if let Some(debug_file) = img.debug_file.take() {
                img.debug_file = Some(scrub_path(&debug_file));
            }
        }
        DebugImage::Wasm(img) => {
            img.name = scrub_path(&img.name);
            img.code_file = scrub_path(&img.code_file);
            if let Some(debug_file) = img.debug_file.take() {
                img.debug_file = Some(scrub_path(&debug_file));
            }
        }
        DebugImage::Proguard(_) => {}
    }
}

// --- Init -------------------------------------------------------------------

/// Initialise Sentry crash + error capture, returning a guard the caller must
/// hold for the whole program lifetime (dropping it flushes + disables Sentry).
///
/// Returns `None` (a no-op — nothing is initialised, no monitor process is
/// spawned) when ANY of:
/// - the DSN can't be resolved (dev/staging bake none),
/// - the user's crash-reporting toggle is off,
/// - the app booted into crash-loop safe mode.
///
/// On success it also spins up the `sentry-rust-minidump` monitor process
/// (abort-safe native crash capture, required because `panic = "abort"` means
/// the in-process guard never flushes on a crash) and intentionally **leaks**
/// its handle so it lives for the whole program — there is no later point to
/// hold it, and the process exiting reaps the monitor regardless.
pub fn init_sentry(
    install_id: &str,
    crash_enabled: bool,
    in_safe_mode: bool,
) -> Option<ClientInitGuard> {
    if !crash_enabled {
        log::info!(target: "telemetry", "crash reporting disabled by setting — Sentry not initialised");
        return None;
    }
    if in_safe_mode {
        log::info!(target: "telemetry", "crash-loop safe mode — Sentry not initialised");
        return None;
    }
    let dsn = resolve_sentry_dsn()?;

    let throttle = Arc::new(Throttle::new());
    let before_send =
        Arc::new(move |event: sentry::protocol::Event<'static>| before_send(event, &throttle));

    let guard = sentry::init((
        dsn,
        ClientOptions {
            release: sentry::release_name!(),
            // Errors: keep all (we differentiate fatal/non-fatal in
            // `before_send`, since Sentry has no per-event error sample rate).
            sample_rate: 1.0,
            // Perf tracing off — operational analytics go through PostHog.
            traces_sample_rate: 0.0,
            // Never send the Sentry-default PII (IP/cookies/etc); our own
            // path-scrub is the primary defence, this is the backstop.
            send_default_pii: false,
            attach_stacktrace: true,
            before_send: Some(before_send),
            ..Default::default()
        },
    ));

    // Tag the scope with coarse, non-identifying context. `user.id` is the
    // anonymous install UUID — the ONLY identifier ever sent.
    let install_id = install_id.to_string();
    sentry::configure_scope(|scope| {
        scope.set_user(Some(sentry::User {
            id: Some(install_id),
            ..Default::default()
        }));
        scope.set_tag("os", std::env::consts::OS);
        scope.set_tag("app_version", env!("CARGO_PKG_VERSION"));
        scope.set_tag("flavor", flavor());
    });

    // Arm the live gate BEFORE the minidump init. In the MONITOR process,
    // `sentry_rust_minidump::init` never returns (it becomes the crash server),
    // so anything after it runs only in the app process. Arming the gate after
    // it left `CRASH_REPORTING_LIVE = false` in the monitor, whose `before_send`
    // then silently dropped every native crash envelope — found via a real
    // SIGSEGV test against the v0.1.8 release (zero envelopes reached Sentry).
    // Runtime opt-out still works where it can: the app-process capture paths
    // re-check the flag on every event; the monitor is launch-consent-gated by
    // construction (it never sees toggle changes), which matches the documented
    // minidump asymmetry.
    CRASH_REPORTING_LIVE.store(true, Ordering::Relaxed);

    // Abort-safe native crash capture. Must come AFTER `sentry::init`. The
    // handle must outlive the program; there's no later owner, so leak it (the
    // process exit reaps the monitor). Failure here is non-fatal — the
    // in-hook `client.flush` (lib.rs panic hook) still covers pure-Rust panics.
    // `ClientInitGuard` derefs to the `sentry::Client` the monitor needs.
    match sentry_rust_minidump::init(&guard) {
        Ok(handle) => {
            std::mem::forget(handle);
        }
        Err(err) => {
            log::warn!(target: "telemetry", "minidump monitor init failed: {err}");
        }
    }

    log::info!(target: "telemetry", "Sentry crash reporting initialised (flavor={})", flavor());
    Some(guard)
}

// --- Live consent gating ----------------------------------------------------
//
// Both subsystems are gated at boot (Sentry init / PostHog queue start), but a
// privacy *consent* toggle must honour an opt-OUT immediately, not at next
// launch. These atomics are the runtime mirror: set true when the subsystem
// actually starts, flipped by [`apply_consent_change`] from `settings_set`.
// `before_send` and `emit_event` consult them on every event, so turning a
// toggle OFF stops sends at once.
//
// Asymmetry by design: turning a toggle back ON when it was off at launch still
// needs a restart (there's no Sentry client or queue to revive). Native crashes
// captured by the separate `sentry-rust-minidump` monitor process also remain
// launch-gated — that process can't be reconfigured at runtime.
static CRASH_REPORTING_LIVE: AtomicBool = AtomicBool::new(false);
static ANALYTICS_LIVE: AtomicBool = AtomicBool::new(false);

/// Update the live consent mirror when a telemetry toggle changes. Called from
/// [`crate::commands::settings_set`] so an opt-OUT takes effect without a
/// relaunch. Unknown keys are ignored.
pub fn apply_consent_change(key: &str, enabled: bool) {
    match key {
        crate::settings::KEY_CRASH_REPORTING_ENABLED => {
            CRASH_REPORTING_LIVE.store(enabled, Ordering::Relaxed);
        }
        crate::settings::KEY_ANALYTICS_ENABLED => {
            ANALYTICS_LIVE.store(enabled, Ordering::Relaxed);
        }
        _ => {}
    }
}

/// The `before_send` pipeline: live opt-out → PII scrub → non-fatal sampling →
/// per-fingerprint daily throttle. Returns `None` to drop the event. The live
/// consent gate is checked here; the scrub/sample/throttle core lives in
/// [`filter_event`] (pure over an injected sampler, so it's unit-testable
/// without touching the process-global consent atomic).
fn before_send(
    event: sentry::protocol::Event<'static>,
    throttle: &Throttle,
) -> Option<sentry::protocol::Event<'static>> {
    // Honour a runtime opt-out immediately: the client exists (reporting was on
    // at launch) but the user has since turned crash reporting off.
    if !CRASH_REPORTING_LIVE.load(Ordering::Relaxed) {
        return None;
    }
    filter_event(event, throttle, sample_non_fatal)
}

/// Scrub → non-fatal sample → per-fingerprint daily throttle.
///
/// Sampling runs **before** the throttle (plan 039 task 40b): a sampled-out
/// handled error is dropped without ever calling [`Throttle::allow`], so it
/// can't consume the fingerprint's scarce daily budget. Fatal events
/// (crashes/panics) skip the sampler and always reach the throttle. The consent
/// gate is the caller's concern.
fn filter_event(
    event: sentry::protocol::Event<'static>,
    throttle: &Throttle,
    sample_non_fatal: impl Fn() -> bool,
) -> Option<sentry::protocol::Event<'static>> {
    let event = scrub_event(event);

    // Sample noisy handled errors FIRST so a sampled-out event never burns the
    // per-fingerprint budget; fatal events always go.
    if event.level != Level::Fatal && !sample_non_fatal() {
        return None;
    }

    // Free-tier discipline: cap each distinct fingerprint per UTC day.
    let key = throttle::fingerprint_key(&event);
    if !throttle.allow(&key, throttle::current_utc_day()) {
        return None;
    }

    Some(event)
}

/// Keep a non-fatal event with probability [`NON_FATAL_SAMPLE_RATE`].
fn sample_non_fatal() -> bool {
    rand::random::<f64>() < NON_FATAL_SAMPLE_RATE
}

/// DEV-ONLY test affordance (Feature 033, task 13). Emits exactly one tagged
/// (`test:true`) Sentry message and one captured error through the ACTIVE DSN,
/// so capture + PII scrub + symbolication can be validated post-release. Both
/// events are `Level::Fatal` so they bypass non-fatal sampling and reliably
/// land; the `test:true` tag makes them trivial to resolve/ignore in the
/// dashboard. No-op when Sentry isn't initialised (DSN absent / toggle off /
/// safe mode), since there's no client bound to the hub.
///
/// Gated to debug builds at the call site (`commands.rs`) so users can't trigger
/// it; the `#[cfg(debug_assertions)]` keeps it out of the release binary
/// entirely.
#[cfg(debug_assertions)]
pub fn fire_test_event() {
    sentry::with_scope(
        |scope| scope.set_tag("test", "true"),
        || {
            sentry::capture_message("muni telemetry test message (task 13)", Level::Fatal);
        },
    );

    // A captured error (distinct from the message) to exercise the
    // exception/stacktrace path the scrubber and symbolication operate on.
    sentry::with_scope(
        |scope| scope.set_tag("test", "true"),
        || {
            let err = std::io::Error::other("muni telemetry test error (task 13)");
            sentry::capture_error(&err);
        },
    );
}

// --- PostHog analytics: public fire-and-forget API (Phase 2) ----------------
//
// The analytics queue lives behind a process-global handle rather than threaded
// through `SessionDeps`: emit sites are scattered (session hot path, error
// presenter) and a global keeps the call to a single `emit_event(...)` that's
// trivially fire-and-forget. The handle bundles the durable queue with the
// analytics-toggle decision read ONCE at init, so the hot path never touches the
// settings store.

/// Process-global analytics handle. `None`/unset until [`init_posthog`] runs (or
/// when analytics is disabled — no key, toggle off — in which case
/// [`emit_event`] is a no-op).
static ANALYTICS: OnceLock<Option<AnalyticsHandle>> = OnceLock::new();

/// What [`emit_event`] needs: the durable queue. Existence of the handle implies
/// the analytics toggle was ON and a key resolved at init time (the toggle is
/// read once and cached here — flipping it later takes effect on next launch,
/// matching how the crash toggle is read pre-runtime).
struct AnalyticsHandle {
    queue: Arc<TelemetryQueue>,
}

/// Initialise the PostHog analytics queue, returning the shared queue handle so
/// the caller can register an on-exit flush (the queue is also stored in a
/// global for [`emit_event`]). Returns `None` — analytics fully off, nothing
/// spawned — when ANY of: the analytics toggle is off, no PostHog key resolves
/// (dev/staging default), or the queue can't be opened.
///
/// `db_path` is the shared `history.sqlite3` file; `install_id` is the per-event
/// `distinct_id`. Idempotent-ish: a second call is ignored (the `OnceLock` keeps
/// the first decision), so boot wires it exactly once.
pub fn init_posthog(
    db_path: &std::path::Path,
    install_id: &str,
    analytics_enabled: bool,
) -> Option<Arc<TelemetryQueue>> {
    if !analytics_enabled {
        log::info!(target: "telemetry", "analytics disabled by setting — PostHog queue not started");
        let _ = ANALYTICS.set(None);
        return None;
    }
    let Some(config) = PostHogConfig::resolve() else {
        log::info!(target: "telemetry", "no PostHog key — analytics queue not started");
        let _ = ANALYTICS.set(None);
        return None;
    };
    let queue = match TelemetryQueue::open(db_path, install_id.to_string(), config) {
        Ok(q) => Arc::new(q),
        Err(err) => {
            log::warn!(target: "telemetry", "open telemetry queue failed: {err} — analytics off");
            let _ = ANALYTICS.set(None);
            return None;
        }
    };
    posthog::spawn_flush_task(Arc::clone(&queue));
    let _ = ANALYTICS.set(Some(AnalyticsHandle {
        queue: Arc::clone(&queue),
    }));
    // Arm the live gate so `emit_event` sends until a runtime opt-out.
    ANALYTICS_LIVE.store(true, Ordering::Relaxed);
    log::info!(target: "telemetry", "PostHog analytics queue started (flavor={})", flavor());
    Some(queue)
}

/// Fire-and-forget: enqueue one operational-health event. Cheap (a single
/// sub-millisecond SQLite insert) and called only off the paste-blocking path,
/// but it intentionally NEVER awaits — delivery happens on the background flush
/// task. A no-op when analytics is off (no handle), so call sites stay
/// unconditional and the gating lives here.
pub fn emit_event(event: PostHogEvent) {
    // Honour a runtime opt-out immediately (the queue exists but the user has
    // since turned analytics off).
    if !ANALYTICS_LIVE.load(Ordering::Relaxed) {
        return;
    }
    let Some(Some(handle)) = ANALYTICS.get() else {
        return;
    };
    handle.queue.enqueue(&event);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_path_redacts_known_username() {
        assert_eq!(
            scrub_path("/Users/alice/code/muni/src/session.rs"),
            "/Users/<redacted>/code/muni/src/session.rs"
        );
    }

    #[test]
    fn scrub_path_redacts_random_uuid_username() {
        // Structure-based: a UUID username is redacted just like a name.
        let input = "/Users/3f2504e0-4f89-41d3-9a0c-0305e82c3301/Library/x.log";
        assert_eq!(scrub_path(input), "/Users/<redacted>/Library/x.log");
    }

    #[test]
    fn scrub_path_redacts_email_localpart_username() {
        assert_eq!(
            scrub_path("/Users/jane.doe/Desktop"),
            "/Users/<redacted>/Desktop"
        );
    }

    #[test]
    fn scrub_path_handles_trailing_username_with_no_slash() {
        assert_eq!(scrub_path("/Users/bob"), "/Users/<redacted>");
    }

    #[test]
    fn scrub_path_redacts_multiple_occurrences() {
        let input = "from /Users/a/x to /Users/b/y";
        assert_eq!(
            scrub_path(input),
            "from /Users/<redacted>/x to /Users/<redacted>/y"
        );
    }

    #[test]
    fn scrub_path_leaves_unrelated_strings_untouched() {
        let input = "DeepgramConnectionFailed: dns error";
        assert_eq!(scrub_path(input), input);
    }

    #[test]
    fn dev_flavor_under_debug_assertions() {
        // The test binary is built with debug_assertions on.
        assert_eq!(flavor(), "dev");
    }

    #[test]
    fn off_toggle_gates_capture_live_without_restart() {
        // Regression guard for the dogfood S10 finding: both telemetry gates
        // were decided only at boot, so toggling OFF at runtime kept sending
        // until restart. These atomics + apply_consent_change make opt-out live.
        use crate::settings::{KEY_ANALYTICS_ENABLED, KEY_CRASH_REPORTING_ENABLED};

        // Simulate "on at launch" (both subsystems armed).
        CRASH_REPORTING_LIVE.store(true, Ordering::Relaxed);
        ANALYTICS_LIVE.store(true, Ordering::Relaxed);

        // A fatal event is kept while crash reporting is live-on (fatal bypasses
        // the non-fatal sampler, so this is deterministic).
        let throttle = Throttle::new();
        let ev = sentry::protocol::Event {
            level: Level::Fatal,
            ..Default::default()
        };
        assert!(before_send(ev, &throttle).is_some());

        // User turns crash reporting OFF at runtime -> the very next event drops,
        // no restart needed.
        apply_consent_change(KEY_CRASH_REPORTING_ENABLED, false);
        assert!(!CRASH_REPORTING_LIVE.load(Ordering::Relaxed));
        let ev2 = sentry::protocol::Event {
            level: Level::Fatal,
            ..Default::default()
        };
        assert!(before_send(ev2, &throttle).is_none());

        // The analytics gate is independent and flips on its own key.
        assert!(ANALYTICS_LIVE.load(Ordering::Relaxed));
        apply_consent_change(KEY_ANALYTICS_ENABLED, false);
        assert!(!ANALYTICS_LIVE.load(Ordering::Relaxed));

        // An unrelated setting never touches the telemetry gates.
        apply_consent_change("hotkey.dictation_binding", true);
        assert!(!CRASH_REPORTING_LIVE.load(Ordering::Relaxed));
        assert!(!ANALYTICS_LIVE.load(Ordering::Relaxed));
    }

    #[test]
    fn init_sentry_gated_off_when_disabled_or_safe_mode() {
        // S11 gate. Both checks return None BEFORE `resolve_sentry_dsn` /
        // `sentry::init`, so this is side-effect-free even if a DSN happens to
        // be in the env. (The full crash-loop → safe-mode path is exercised by
        // boot_health; see backlog 0057.)
        assert!(
            init_sentry("test-install", false, false).is_none(),
            "crash reporting off ⇒ no Sentry client"
        );
        assert!(
            init_sentry("test-install", true, true).is_none(),
            "safe mode ⇒ no Sentry client even with the toggle on"
        );
    }

    #[test]
    fn scrub_event_redacts_every_home_path_surface_and_drops_hostname() {
        // Regression guard for the dogfood finding: a `capture_message` +
        // `attach_stacktrace` test event leaked `/Users/<name>/` 150+ times via
        // thread frames' `package`/`abs_path` and debug-image paths, plus the
        // `<username>.local` hostname — none of which the original scrubber
        // touched. This builds that exact shape and asserts nothing leaks.
        use sentry::protocol::{AppleDebugImage, DebugImage, Event, Frame, Stacktrace, Thread};

        let frame = Frame {
            abs_path: Some("/Users/alice/code/muni/src/session.rs".into()),
            filename: Some("session.rs".into()),
            package: Some("/Users/alice/code/muni/target/debug/libmuni.dylib".into()),
            module: Some("/Users/alice/weird/module".into()),
            ..Default::default()
        };
        let thread = Thread {
            stacktrace: Some(Stacktrace {
                frames: vec![frame],
                ..Default::default()
            }),
            ..Default::default()
        };

        let mut event = Event {
            message: Some("crash at /Users/alice/x".into()),
            server_name: Some("alice.local".into()),
            ..Default::default()
        };
        event.threads.values.push(thread);
        event
            .debug_meta
            .to_mut()
            .images
            .push(DebugImage::Apple(AppleDebugImage {
                name: "/Users/alice/code/muni/target/debug/Muni-dev.app/Contents/MacOS/muni".into(),
                arch: None,
                cpu_type: None,
                cpu_subtype: None,
                image_addr: Default::default(),
                image_size: 0,
                image_vmaddr: Default::default(),
                uuid: Default::default(),
            }));

        let scrubbed = scrub_event(event);
        let json = serde_json::to_string(&scrubbed).expect("event serializes");

        assert!(!json.contains("/Users/alice"), "home path leaked: {json}");
        assert!(!json.contains("alice.local"), "hostname leaked: {json}");
        assert!(
            json.contains("/Users/<redacted>"),
            "expected redaction marker in {json}"
        );
        assert!(scrubbed.server_name.is_none(), "hostname must be dropped");
    }

    // --- Task 40a: consent fail-closed vs key-absent ------------------------

    #[test]
    fn crash_toggle_missing_file_defaults_on() {
        // Fresh install: no settings file yet. Nothing was ever persisted, so no
        // opt-out could exist — default ON (key-absent, not a store failure).
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_crash_toggle(dir.path()));
    }

    #[test]
    fn crash_toggle_key_absent_defaults_on() {
        // Readable store, valid JSON, but our key isn't present → default ON.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(crate::settings::SETTINGS_FILE);
        std::fs::write(&path, r#"{"some.other.key": true}"#).expect("write settings");
        assert!(read_crash_toggle(dir.path()));
    }

    #[test]
    fn crash_toggle_reads_explicit_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(crate::settings::SETTINGS_FILE);
        let key = crate::settings::KEY_CRASH_REPORTING_ENABLED;
        std::fs::write(&path, format!(r#"{{"{key}": false}}"#)).expect("write false");
        assert!(!read_crash_toggle(dir.path()));
        std::fs::write(&path, format!(r#"{{"{key}": true}}"#)).expect("write true");
        assert!(read_crash_toggle(dir.path()));
    }

    #[test]
    fn crash_toggle_corrupt_store_fails_closed() {
        // Acceptance (plan 039 slice 13): a corrupt store must NOT re-enrol a
        // user who may have opted out — fail CLOSED (crash reporting off).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(crate::settings::SETTINGS_FILE);
        std::fs::write(&path, "{ this is not valid json").expect("write corrupt");
        assert!(
            !read_crash_toggle(dir.path()),
            "corrupt store must fail closed, not default on"
        );
    }

    // The analytics toggle shares the same fail-closed contract via
    // `read_consent_toggle`. lib.rs's `read_analytics_toggle` is a thin wrapper
    // over this helper pinned to `KEY_ANALYTICS_ENABLED`, so exercising the
    // helper with that key covers the analytics twin (task 40a) end to end —
    // and, unlike the old `app.store()`-based read, is unit-testable.

    #[test]
    fn analytics_toggle_missing_file_defaults_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_consent_toggle(
            dir.path(),
            crate::settings::KEY_ANALYTICS_ENABLED
        ));
    }

    #[test]
    fn analytics_toggle_reads_explicit_values() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(crate::settings::SETTINGS_FILE);
        let key = crate::settings::KEY_ANALYTICS_ENABLED;
        std::fs::write(&path, format!(r#"{{"{key}": false}}"#)).expect("write false");
        assert!(!read_consent_toggle(dir.path(), key));
        std::fs::write(&path, format!(r#"{{"{key}": true}}"#)).expect("write true");
        assert!(read_consent_toggle(dir.path(), key));
    }

    #[test]
    fn analytics_toggle_corrupt_store_fails_closed() {
        // Acceptance (plan 039 slice 13): a corrupt store must NOT re-enrol an
        // opted-out user into PostHog — fail CLOSED (analytics off). This is the
        // exact scenario the old `app.store()` read regressed on, since the
        // plugin hands back an empty store for a corrupt file.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(crate::settings::SETTINGS_FILE);
        std::fs::write(&path, "{ this is not valid json").expect("write corrupt");
        assert!(
            !read_consent_toggle(dir.path(), crate::settings::KEY_ANALYTICS_ENABLED),
            "corrupt store must fail closed for analytics, not default on"
        );
    }

    // --- Task 40b: non-fatal sampling runs before the throttle --------------

    #[test]
    fn non_fatal_sampling_precedes_throttle_and_spares_budget() {
        let throttle = Throttle::new();
        // Empty error events all share one fingerprint bucket.
        let make = || sentry::protocol::Event {
            level: Level::Error,
            ..Default::default()
        };

        // Many non-fatal events the sampler drops. If sampling ran AFTER the
        // throttle (the old order), these would each burn the daily budget.
        for _ in 0..50 {
            assert!(filter_event(make(), &throttle, || false).is_none());
        }

        // The fingerprint's full daily budget is still intact for kept events —
        // proof the sampled-out events never called `Throttle::allow`.
        for _ in 0..throttle::MAX_PER_FINGERPRINT_PER_DAY {
            assert!(filter_event(make(), &throttle, || true).is_some());
        }
        // Budget now genuinely exhausted → the next kept event is throttled.
        assert!(filter_event(make(), &throttle, || true).is_none());
    }

    #[test]
    fn fatal_events_bypass_the_sampler() {
        let throttle = Throttle::new();
        let fatal = || sentry::protocol::Event {
            level: Level::Fatal,
            ..Default::default()
        };
        // Even with a sampler that always drops, a fatal event is kept.
        assert!(filter_event(fatal(), &throttle, || false).is_some());
    }

    // --- Task 40c: breadcrumbs are scrubbed ---------------------------------

    #[test]
    fn scrub_event_redacts_breadcrumb_message_and_nested_data() {
        use sentry::protocol::{Breadcrumb, Event, Value};

        let mut crumb = Breadcrumb {
            message: Some("loaded model from /Users/alice/Library/muni/model.bin".into()),
            ..Default::default()
        };
        crumb
            .data
            .insert("path".into(), Value::from("/Users/alice/x.log"));
        crumb.data.insert(
            "ctx".into(),
            serde_json::json!({ "abs": "/Users/alice/y", "list": ["/Users/alice/z"] }),
        );

        let mut event = Event::default();
        event.breadcrumbs.values.push(crumb);

        let scrubbed = scrub_event(event);
        let json = serde_json::to_string(&scrubbed).expect("event serializes");
        assert!(
            !json.contains("/Users/alice"),
            "breadcrumb leaked home path: {json}"
        );
        assert!(
            json.contains("/Users/<redacted>"),
            "expected redaction marker in {json}"
        );
    }
}
