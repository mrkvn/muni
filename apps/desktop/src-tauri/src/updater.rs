//! Silent auto-update delivery.
//!
//! Phase 10 scaffolded *detection* (the tray "Check for Updates…" item and a
//! pubkey/endpoint config); this module adds the actual *delivery* and the
//! non-intrusive surfacing of a pending update.
//!
//! ## Design (agreed with product)
//!
//! - **Background checks (launch + recurring).** A first check after a short
//!   delay (so the speed-critical boot + first-hotkey path is never contended
//!   — Priority #1), then a re-check every [`DEFAULT_CHECK_INTERVAL_SECS`]
//!   while the app runs — so a long-running session downloads a new release on
//!   its own, without the user relaunching. PROD-only: dev/staging ship a
//!   placeholder endpoint and are treated as "not configured", so they never
//!   self-update from the production feed. See [`is_configured`].
//!
//! - **Silent install, never a forced restart.** If an update exists we
//!   download AND install it in the background. On macOS "install" swaps the
//!   `.app` bundle on disk while the running process keeps the old code in
//!   memory — so the new version is simply used on the next launch. We
//!   deliberately do **not** call `restart()`: the user may be mid-dictation,
//!   and yanking the app out from under them is the exact bad UX we avoid.
//!
//! - **Surface it, don't block.** A pending update is telegraphed three
//!   non-blocking ways, all driven from [`set_phase`] so they never drift:
//!   a mono-dot badge on the tray icon, a state-driven tray menu item
//!   ("Update ready — Restart to apply"), and a dismissible in-app banner
//!   (the frontend listens for [`EVENT_UPDATER_STATE`]). Restarting is always
//!   user-initiated, from the tray item or the banner.
//!
//! State machine: `Idle → Downloading → Ready`. Any failure logs and falls
//! back to `Idle`, so a transient network/notarization hiccup just retries on
//! the next launch.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_updater::UpdaterExt;
// `tokio::time::Instant` (not `std::time::Instant`) so the stall watchdog reads
// the same clock `tokio::time::sleep` does — that's what makes the watchdog
// deterministically testable under `#[tokio::test(start_paused = true)]`.
use tokio::time::Instant;

use crate::telemetry;
use crate::tray;

/// Event emitted to the frontend on every phase change. The main window
/// listens and shows/clears the non-blocking "update ready" banner. Payload
/// is a serialised [`UpdaterPhase`] (internally tagged on `phase`).
pub const EVENT_UPDATER_STATE: &str = "updater://state";

/// Delay before the first check so it never competes with boot or the first
/// hotkey press. The whole point of the deferral is to keep the latency-critical
/// path uncontended; the exact value is not load-bearing.
const STARTUP_CHECK_DELAY: Duration = Duration::from_secs(8);

/// How often to re-check the feed *while the app keeps running*, so a
/// long-running session picks up a new release on its own — no relaunch
/// needed. Overridable via `MUNI_UPDATE_CHECK_INTERVAL_SECS` (dev/QA), floored
/// at [`MIN_CHECK_INTERVAL_SECS`] so a typo can't hammer the feed.
const DEFAULT_CHECK_INTERVAL_SECS: u64 = 15 * 60;
const MIN_CHECK_INTERVAL_SECS: u64 = 60;

/// Resolve the recurring-check interval from the env override (if any),
/// floored at the minimum. Pure for testability.
fn resolve_interval(env_value: Option<&str>) -> Duration {
    let secs = env_value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|v| v.max(MIN_CHECK_INTERVAL_SECS))
        .unwrap_or(DEFAULT_CHECK_INTERVAL_SECS);
    Duration::from_secs(secs)
}

/// Cap on the feed *check* HTTP request. The check fetches a tiny `latest.json`,
/// so anything beyond this is a hung/degraded connection, not a slow-but-working
/// one. Without it, a stalled check (the plugin sets no default reqwest timeout)
/// blocks the whole background loop — the same wedge the download guard prevents,
/// one layer up.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the download may receive **zero** bytes before we treat the
/// connection as frozen and abort. This catches a hung/half-open socket fast;
/// it does NOT catch a slow-but-steady edge (bytes keep trickling) — that's what
/// [`resolve_download_timeout`]'s hard cap is for. The two are complementary.
const DOWNLOAD_STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the watchdog wakes to evaluate the stall condition. Small relative
/// to [`DOWNLOAD_STALL_TIMEOUT`] so the abort lands promptly once frozen.
const DOWNLOAD_PROGRESS_POLL: Duration = Duration::from_secs(5);

/// Default hard cap on the *total* download wall-time. This is the mechanism
/// that recovers from a degraded-but-trickling CDN edge (the real-world failure:
/// one GitHub Fastly edge serving ~27 KB/s while siblings ran at 1–2 MB/s). A
/// stall watchdog can't see that — bytes are arriving — so without a total cap a
/// ~44 MB bundle would crawl for ~27 min with the tray wedged on "Downloading…"
/// and every retry blocked by the in-flight guard.
///
/// The Tauri updater download is **not** resumable (it restarts from zero), so
/// this is deliberately a coarse backstop, not fine-grained throttling: cross it
/// and we abort, drop back to `Idle`, and the next scheduled check retries —
/// crucially getting a *fresh* CDN edge, which is what actually unsticks the
/// degraded-edge case. 10 min ≈ a 75 KB/s floor for the current bundle; faster
/// than that and even a slow home connection completes, slower and a retry's
/// edge-rotation is the better bet anyway. Overridable for QA via
/// `MUNI_UPDATE_DOWNLOAD_TIMEOUT_SECS`, floored at [`MIN_DOWNLOAD_HARD_CAP_SECS`].
const DEFAULT_DOWNLOAD_HARD_CAP_SECS: u64 = 10 * 60;
const MIN_DOWNLOAD_HARD_CAP_SECS: u64 = 60;

/// Resolve the download hard cap from the env override (if any), floored at the
/// minimum so a typo can't set a cap so short no download could ever finish.
/// Pure for testability.
fn resolve_download_timeout(env_value: Option<&str>) -> Duration {
    let secs = env_value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|v| v.max(MIN_DOWNLOAD_HARD_CAP_SECS))
        .unwrap_or(DEFAULT_DOWNLOAD_HARD_CAP_SECS);
    Duration::from_secs(secs)
}

/// Why a guarded download ended without installing. Maps to the `stall_reason`
/// telemetry property so the field can tell a frozen socket apart from a
/// degraded-but-trickling edge.
#[derive(Debug)]
enum DownloadGuardError {
    /// No bytes for [`DOWNLOAD_STALL_TIMEOUT`] — a frozen/half-open connection.
    Stalled,
    /// Total wall-time exceeded the hard cap — a degraded-but-trickling edge.
    TimedOut,
    /// The plugin's own download/verify/install failed (network, signature, IO).
    Failed(String),
}

impl DownloadGuardError {
    /// Stable, low-cardinality discriminant for the telemetry `stall_reason`
    /// property. Never carries a message or any user content.
    fn reason(&self) -> &'static str {
        match self {
            DownloadGuardError::Stalled => "no_progress",
            DownloadGuardError::TimedOut => "timeout",
            DownloadGuardError::Failed(_) => "failed",
        }
    }
}

impl std::fmt::Display for DownloadGuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadGuardError::Stalled => write!(
                f,
                "no bytes received for {}s",
                DOWNLOAD_STALL_TIMEOUT.as_secs()
            ),
            DownloadGuardError::TimedOut => {
                write!(f, "exceeded the download hard cap")
            }
            DownloadGuardError::Failed(msg) => write!(f, "{msg}"),
        }
    }
}

/// Why a feed check failed. Separates our own timeout from the plugin's errors so
/// the log line is honest about which one fired.
enum CheckError {
    Timeout,
    Plugin(tauri_plugin_updater::Error),
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckError::Timeout => {
                write!(f, "check timed out after {}s", CHECK_TIMEOUT.as_secs())
            }
            CheckError::Plugin(err) => write!(f, "{err}"),
        }
    }
}

/// Updater lifecycle phase. Serialised camelCase and internally tagged on
/// `phase`, so the wire shape the frontend matches on is
/// `{"phase":"idle"}` / `{"phase":"downloading"}` /
/// `{"phase":"ready","version":"1.2.0"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum UpdaterPhase {
    /// No update in flight. The tray item offers a manual "Check for Updates…".
    Idle,
    /// An update was found and is downloading/installing in the background.
    /// Carries the version so a manual check mid-download can report which
    /// version is in flight instead of a blank string (task 30b).
    Downloading { version: String },
    /// Installed to disk; applies on next launch. The tray offers
    /// "Restart to apply" and the in-app banner appears.
    Ready { version: String },
}

/// Managed handle to the current phase. The tray click dispatcher reads it to
/// decide whether a click on the update menu item means "check" or "restart",
/// and the manual check reads it to avoid starting a second download.
pub struct UpdaterState(pub Mutex<UpdaterPhase>);

impl Default for UpdaterState {
    fn default() -> Self {
        Self(Mutex::new(UpdaterPhase::Idle))
    }
}

/// Outcome of an interactive "Check for updates…" click. The JS layer maps
/// this onto a toast.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum UpdaterCheckResult {
    UpToDate,
    UpdateAvailable { version: String },
    NotConfigured,
    Error { message: String },
}

/// Snapshot the current phase (defaults to `Idle` if the state isn't managed
/// yet or the mutex is poisoned). The unmanaged case is only reachable before
/// the tray is built AND before `spawn_background_check` (which now ensures the
/// state itself — task 33); once the background loop is armed the state always
/// exists, so the loop's `Ready` break reads a real phase.
pub fn current_phase<R: Runtime>(app: &AppHandle<R>) -> UpdaterPhase {
    app.try_state::<UpdaterState>()
        .and_then(|s| s.0.lock().ok().map(|g| g.clone()))
        .unwrap_or(UpdaterPhase::Idle)
}

/// The single phase-transition point: persist the phase, reflect it onto the
/// tray (menu label/enabled + icon badge), and notify the frontend. Every
/// surface flows through here so none can drift out of sync.
fn set_phase<R: Runtime>(app: &AppHandle<R>, phase: UpdaterPhase) {
    if let Some(state) = app.try_state::<UpdaterState>() {
        // Recover a poisoned lock rather than dropping the write — a stale
        // phase would leave the tray/banner out of sync with reality.
        let mut guard = state.0.lock().unwrap_or_else(|p| p.into_inner());
        *guard = phase.clone();
    }
    reflect_phase(app, &phase);
}

/// Reflect a phase onto the tray (menu label/enabled + icon badge) and notify
/// the frontend, WITHOUT touching the phase mutex. Split out of [`set_phase`]
/// so the atomic check-and-set in [`begin_download`] can claim the phase under
/// a single lock and then reflect the result here — no second lock, no gap.
fn reflect_phase<R: Runtime>(app: &AppHandle<R>, phase: &UpdaterPhase) {
    let (label, enabled, badged) = match phase {
        UpdaterPhase::Idle => (tray::UPDATE_ITEM_IDLE_LABEL, true, false),
        UpdaterPhase::Downloading { .. } => (tray::UPDATE_ITEM_DOWNLOADING_LABEL, false, false),
        UpdaterPhase::Ready { .. } => (tray::UPDATE_ITEM_READY_LABEL, true, true),
    };
    tray::set_update_indicator(app, label, enabled, badged);

    if let Err(err) = app.emit(EVENT_UPDATER_STATE, phase) {
        log::warn!(target: "updater", "emit {EVENT_UPDATER_STATE} failed: {err}");
    }
}

/// Atomically claim the `Downloading` phase from `Idle` under a single lock.
/// Returns `true` iff this caller performed the `Idle → Downloading`
/// transition (and so must proceed with the download); `false` if a download
/// was already in flight or ready. The single lock closes the check-then-set
/// gap that let a manual click race the launch check into two concurrent
/// downloads. A poisoned lock is recovered (never panics the caller).
fn claim_idle_to_downloading(phase: &Mutex<UpdaterPhase>, version: &str) -> bool {
    let mut guard = phase.lock().unwrap_or_else(|p| p.into_inner());
    if !matches!(*guard, UpdaterPhase::Idle) {
        return false;
    }
    *guard = UpdaterPhase::Downloading {
        version: version.to_string(),
    };
    true
}

/// App-level wrapper over [`claim_idle_to_downloading`]. When [`UpdaterState`]
/// isn't managed there's no lock to guard on, so we preserve the prior "proceed"
/// behavior. In practice `begin_download` only runs from the background loop,
/// which now ensures the state is managed before it starts (task 33) — so this
/// `None` branch is purely defensive and no longer reachable via that path.
fn try_claim_downloading<R: Runtime>(app: &AppHandle<R>, version: &str) -> bool {
    match app.try_state::<UpdaterState>() {
        Some(state) => claim_idle_to_downloading(&state.0, version),
        None => true,
    }
}

/// True only when the build carries a real updater endpoint + pubkey (i.e.
/// the production config). Dev/staging keep the `example.com` placeholder and
/// the `REPLACE_WITH…` pubkey, so this returns false and the app never tries
/// to self-update from the production feed. This is the prod-only gate.
pub fn is_configured<R: Runtime>(app: &AppHandle<R>) -> bool {
    let plugins = match serde_json::to_value(&app.config().plugins) {
        Ok(value) => value,
        Err(err) => {
            log::warn!(target: "updater", "config plugin block unreadable: {err}");
            return false;
        }
    };
    let Some(updater) = plugins.get("updater") else {
        return false;
    };
    let pubkey = updater.get("pubkey").and_then(|v| v.as_str()).unwrap_or("");
    if pubkey.is_empty() || pubkey.starts_with("REPLACE_WITH") {
        return false;
    }
    match updater.get("endpoints").and_then(|v| v.as_array()) {
        Some(arr) if !arr.is_empty() => arr.iter().any(|v| {
            v.as_str()
                .map(|s| !s.is_empty() && !s.contains("example.com"))
                .unwrap_or(false)
        }),
        _ => false,
    }
}

/// Spawn the background updater: a first check after [`STARTUP_CHECK_DELAY`],
/// then a re-check every [`resolve_interval`] so a running app downloads a new
/// release on its own — the user never has to relaunch to *get* an update
/// (only to *apply* one). No-ops on unconfigured builds so we don't arm a timer
/// on dev/staging. Safe to call once per launch after the tray is built.
pub fn spawn_background_check<R: Runtime>(app: &AppHandle<R>) {
    if !is_configured(app) {
        log::info!(target: "updater", "background check skipped: updater not configured");
        return;
    }
    // Task 33 — the background loop is now armed UNCONDITIONALLY of the tray
    // build, but `UpdaterState` is normally managed inside `tray::build`. If the
    // tray failed before managing it, the phase mutex would be absent: every
    // check would re-claim `Idle → Downloading` (no dedupe lock), `set_phase`
    // would silently no-op, `current_phase` would forever read `Idle`, and the
    // loop's `Ready` break would never fire — re-downloading + re-installing the
    // same update every interval. Guarantee the state exists so the phase
    // machine works regardless of tray outcome. `manage` is a no-op if the tray
    // already managed it (this runs once, on the setup thread — no race).
    if app.try_state::<UpdaterState>().is_none() {
        app.manage(UpdaterState::default());
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(STARTUP_CHECK_DELAY).await;
        let interval = resolve_interval(
            std::env::var("MUNI_UPDATE_CHECK_INTERVAL_SECS")
                .ok()
                .as_deref(),
        );
        log::info!(target: "updater", "background checks every {}s", interval.as_secs());
        loop {
            match check(&app).await {
                Ok(Some(update)) => {
                    log::info!(target: "updater", "check: update v{} available", update.version);
                    begin_download(&app, update);
                }
                Ok(None) => log::info!(target: "updater", "check: up to date"),
                Err(err) => log::warn!(target: "updater", "check failed: {err}"),
            }
            // Once an update is downloaded it just waits for the next restart —
            // nothing more to fetch, so stop polling until then.
            if matches!(current_phase(&app), UpdaterPhase::Ready { .. }) {
                log::info!(target: "updater", "update downloaded; pausing checks until restart");
                break;
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// Thin wrapper over the plugin's check so both the launch path and the manual
/// command share one call site — and both inherit the [`CHECK_TIMEOUT`] bound so
/// a hung feed fetch can't wedge the background loop (the plugin sets no default
/// reqwest timeout of its own).
async fn check<R: Runtime>(
    app: &AppHandle<R>,
) -> Result<Option<tauri_plugin_updater::Update>, CheckError> {
    let updater = app.updater().map_err(CheckError::Plugin)?;
    match tokio::time::timeout(CHECK_TIMEOUT, updater.check()).await {
        Ok(Ok(update)) => Ok(update),
        Ok(Err(err)) => Err(CheckError::Plugin(err)),
        Err(_elapsed) => Err(CheckError::Timeout),
    }
}

/// Move to `Downloading`, then download + install the bundle on a background
/// task. On success → `Ready` (applies next launch); on failure → back to
/// `Idle` (retried next launch). Never restarts the app.
fn begin_download<R: Runtime>(app: &AppHandle<R>, update: tauri_plugin_updater::Update) {
    // Atomic check-and-set: claim `Idle → Downloading` under a single lock so a
    // manual click racing the launch check (or vice versa) can't both observe
    // `Idle` and start two concurrent downloads. Only the winner proceeds.
    let version = update.version.clone();
    if !try_claim_downloading(app, &version) {
        log::info!(target: "updater", "download already in progress or ready; ignoring");
        return;
    }
    // Phase is already `Downloading` in managed state — reflect it onto the
    // tray + frontend (no second lock, no gap).
    reflect_phase(app, &UpdaterPhase::Downloading { version });

    let app = app.clone();
    let hard_cap = resolve_download_timeout(
        std::env::var("MUNI_UPDATE_DOWNLOAD_TIMEOUT_SECS")
            .ok()
            .as_deref(),
    );
    tauri::async_runtime::spawn(async move {
        let version = update.version.clone();
        match download_with_guard(&update, DOWNLOAD_STALL_TIMEOUT, hard_cap).await {
            Ok(()) => {
                log::info!(
                    target: "updater",
                    "installed v{version}; will apply on next launch (no forced restart)"
                );
                set_phase(&app, UpdaterPhase::Ready { version });
            }
            Err(err) => {
                // A stall/timeout is recoverable: drop to `Idle` so the next
                // scheduled check retries — and crucially gets a fresh CDN edge,
                // which is what unsticks a degraded-but-trickling edge. Emit a
                // low-cardinality signal first so we can actually SEE this in the
                // field instead of guessing at silent stuck installs. The event
                // is a no-op when analytics is off, so the call stays unconditional.
                telemetry::emit_event(telemetry::events::updater_download_stalled(err.reason()));
                log::warn!(
                    target: "updater",
                    "download/install failed ({}): {err}; staying on current version, retry next check",
                    err.reason()
                );
                set_phase(&app, UpdaterPhase::Idle);
            }
        }
    });
}

/// Outcome of a future run under the stall+hard-cap guard. Generic over the
/// wrapped future so the timing logic is unit-testable with a synthetic future
/// under a paused clock — no real network needed.
enum GuardOutcome<T, E> {
    /// The wrapped future finished on its own (with its own Ok/Err).
    Completed(Result<T, E>),
    /// No progress for the stall threshold.
    Stalled,
    /// Total wall-time crossed the hard cap.
    TimedOut,
}

/// Drive `fut` to completion while enforcing two independent deadlines:
/// a **stall** deadline (no progress recorded in `last_progress` for
/// `stall_timeout`) and a **hard cap** on total wall-time. Whichever fires first
/// wins; the future is dropped (cancelled) on either deadline.
///
/// `last_progress` is updated externally — in production by the download's
/// per-chunk callback. It uses [`tokio::time::Instant`] so `.elapsed()` tracks
/// the same clock as the sleeps, which is what makes this deterministic under a
/// paused test clock.
async fn run_with_guard<T, E, F>(
    fut: F,
    last_progress: Arc<Mutex<Instant>>,
    stall_timeout: Duration,
    hard_cap: Duration,
    poll_interval: Duration,
) -> GuardOutcome<T, E>
where
    F: Future<Output = Result<T, E>>,
{
    tokio::pin!(fut);
    let hard_deadline = tokio::time::sleep(hard_cap);
    tokio::pin!(hard_deadline);
    loop {
        let tick = tokio::time::sleep(poll_interval);
        tokio::select! {
            // Prefer real completion over a deadline that matures the same tick.
            biased;
            result = &mut fut => return GuardOutcome::Completed(result),
            _ = &mut hard_deadline => return GuardOutcome::TimedOut,
            _ = tick => {
                let stalled = last_progress
                    .lock()
                    .map(|since| since.elapsed() >= stall_timeout)
                    .unwrap_or(false);
                if stalled {
                    return GuardOutcome::Stalled;
                }
            }
        }
    }
}

/// Download + install `update` under the stall watchdog and hard cap. The plugin
/// streams the body and invokes our per-chunk callback for every chunk, so each
/// chunk refreshes `last_progress` and the watchdog only fires on a genuine
/// freeze. Signature verification + the on-disk bundle swap happen inside the
/// plugin on success — same as the unguarded path, just bounded.
async fn download_with_guard(
    update: &tauri_plugin_updater::Update,
    stall_timeout: Duration,
    hard_cap: Duration,
) -> Result<(), DownloadGuardError> {
    let last_progress = Arc::new(Mutex::new(Instant::now()));
    let on_chunk_progress = Arc::clone(&last_progress);
    let on_chunk = move |_chunk_len: usize, _content_len: Option<u64>| {
        if let Ok(mut since) = on_chunk_progress.lock() {
            *since = Instant::now();
        }
    };
    let download = update.download_and_install(
        on_chunk,
        || log::info!(target: "updater", "download complete; swapping bundle on disk"),
    );

    match run_with_guard(
        download,
        last_progress,
        stall_timeout,
        hard_cap,
        DOWNLOAD_PROGRESS_POLL,
    )
    .await
    {
        GuardOutcome::Completed(Ok(())) => Ok(()),
        GuardOutcome::Completed(Err(err)) => Err(DownloadGuardError::Failed(err.to_string())),
        GuardOutcome::Stalled => Err(DownloadGuardError::Stalled),
        GuardOutcome::TimedOut => Err(DownloadGuardError::TimedOut),
    }
}

/// Interactive "Check for Updates…" handler. Reports the current state if a
/// download is already in flight / ready; otherwise checks the feed and, when
/// an update exists, kicks off the same silent background download used at
/// launch. The returned result drives the toast only — the download proceeds
/// regardless via [`EVENT_UPDATER_STATE`].
pub async fn run_manual_check<R: Runtime>(app: &AppHandle<R>) -> UpdaterCheckResult {
    if !is_configured(app) {
        log::info!(target: "updater", "manual check: updater not configured");
        return UpdaterCheckResult::NotConfigured;
    }
    match current_phase(app) {
        // A download is already in flight — report the version being fetched
        // (task 30b) so the toast reads "Update X found" instead of a blank.
        UpdaterPhase::Downloading { version } => {
            return UpdaterCheckResult::UpdateAvailable { version }
        }
        UpdaterPhase::Ready { version } => return UpdaterCheckResult::UpdateAvailable { version },
        UpdaterPhase::Idle => {}
    }
    match check(app).await {
        Ok(Some(update)) => {
            let version = update.version.clone();
            log::info!(target: "updater", "manual check: update v{version} available; downloading");
            begin_download(app, update);
            UpdaterCheckResult::UpdateAvailable { version }
        }
        Ok(None) => UpdaterCheckResult::UpToDate,
        Err(err) => {
            log::warn!(target: "updater", "manual check failed: {err}");
            UpdaterCheckResult::Error {
                message: err.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updater_phase_wire_shape_matches_frontend_contract() {
        // The frontend banner matches on these exact strings — pin them.
        assert_eq!(
            serde_json::to_value(UpdaterPhase::Idle).unwrap(),
            serde_json::json!({ "phase": "idle" })
        );
        assert_eq!(
            serde_json::to_value(UpdaterPhase::Downloading {
                version: "1.2.0".into()
            })
            .unwrap(),
            // Additive vs the pre-task-30 shape: the frontend matches on
            // `phase` and ignores the extra `version` field.
            serde_json::json!({ "phase": "downloading", "version": "1.2.0" })
        );
        assert_eq!(
            serde_json::to_value(UpdaterPhase::Ready {
                version: "1.2.0".into()
            })
            .unwrap(),
            serde_json::json!({ "phase": "ready", "version": "1.2.0" })
        );
    }

    #[test]
    fn resolve_interval_parses_floors_and_defaults() {
        use super::{resolve_interval, DEFAULT_CHECK_INTERVAL_SECS, MIN_CHECK_INTERVAL_SECS};
        // unset → default
        assert_eq!(
            resolve_interval(None).as_secs(),
            DEFAULT_CHECK_INTERVAL_SECS
        );
        // valid override is honoured
        assert_eq!(resolve_interval(Some("300")).as_secs(), 300);
        // below the floor is clamped up (no hammering the feed)
        assert_eq!(
            resolve_interval(Some("5")).as_secs(),
            MIN_CHECK_INTERVAL_SECS
        );
        // garbage falls back to default
        assert_eq!(
            resolve_interval(Some("nope")).as_secs(),
            DEFAULT_CHECK_INTERVAL_SECS
        );
    }

    #[test]
    fn updater_check_result_wire_shape_is_tagged() {
        assert_eq!(
            serde_json::to_value(UpdaterCheckResult::UpToDate).unwrap(),
            serde_json::json!({ "kind": "upToDate" })
        );
        assert_eq!(
            serde_json::to_value(UpdaterCheckResult::UpdateAvailable {
                version: "9.9.9".into()
            })
            .unwrap(),
            serde_json::json!({ "kind": "updateAvailable", "version": "9.9.9" })
        );
    }

    #[test]
    fn resolve_download_timeout_parses_floors_and_defaults() {
        // unset → default
        assert_eq!(
            resolve_download_timeout(None).as_secs(),
            DEFAULT_DOWNLOAD_HARD_CAP_SECS
        );
        // valid override is honoured
        assert_eq!(resolve_download_timeout(Some("900")).as_secs(), 900);
        // below the floor is clamped up (a typo can't set an impossible cap)
        assert_eq!(
            resolve_download_timeout(Some("5")).as_secs(),
            MIN_DOWNLOAD_HARD_CAP_SECS
        );
        // garbage falls back to default
        assert_eq!(
            resolve_download_timeout(Some("nope")).as_secs(),
            DEFAULT_DOWNLOAD_HARD_CAP_SECS
        );
    }

    // --- Download guard timing (paused clock → deterministic, instant) -------
    //
    // These exercise the recovery logic that was missing — the reason a degraded
    // CDN edge wedged the updater. `start_paused` auto-advances virtual time to
    // the next timer whenever the runtime is idle, so a "60s stall" or "10min
    // cap" resolves instantly and deterministically with no real waiting.

    const TEST_STALL: Duration = Duration::from_secs(60);
    const TEST_POLL: Duration = Duration::from_secs(10);

    #[tokio::test(start_paused = true)]
    async fn guard_returns_completed_when_future_finishes() {
        let last = Arc::new(Mutex::new(Instant::now()));
        let out: GuardOutcome<(), ()> = run_with_guard(
            async { Ok(()) },
            last,
            TEST_STALL,
            Duration::from_secs(600),
            TEST_POLL,
        )
        .await;
        assert!(matches!(out, GuardOutcome::Completed(Ok(()))));
    }

    #[tokio::test(start_paused = true)]
    async fn guard_surfaces_inner_error_as_completed_err() {
        let last = Arc::new(Mutex::new(Instant::now()));
        let out: GuardOutcome<(), &str> = run_with_guard(
            async { Err("boom") },
            last,
            TEST_STALL,
            Duration::from_secs(600),
            TEST_POLL,
        )
        .await;
        assert!(matches!(out, GuardOutcome::Completed(Err("boom"))));
    }

    #[tokio::test(start_paused = true)]
    async fn guard_returns_stalled_when_no_progress_arrives() {
        // A frozen connection: the future never completes and progress is never
        // refreshed → the stall watchdog fires before the (much later) hard cap.
        let last = Arc::new(Mutex::new(Instant::now()));
        let out: GuardOutcome<(), ()> = run_with_guard(
            std::future::pending(),
            last,
            TEST_STALL,
            Duration::from_secs(600),
            TEST_POLL,
        )
        .await;
        assert!(matches!(out, GuardOutcome::Stalled));
    }

    #[tokio::test(start_paused = true)]
    async fn guard_times_out_on_hard_cap_despite_steady_progress() {
        // The real incident's shape: bytes keep trickling (progress refreshes
        // every 5s, well under the stall threshold) but the download never
        // finishes within the cap → the hard cap, not the stall watchdog, ends it.
        let last = Arc::new(Mutex::new(Instant::now()));
        let progress = Arc::clone(&last);
        let trickle = async move {
            // 100 × 5s = 500s of "downloading", far past the 125s cap, so the
            // loop never completes — the guard returns first. No unreachable code.
            for _ in 0..100 {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if let Ok(mut since) = progress.lock() {
                    *since = Instant::now();
                }
            }
            Ok(())
        };
        let out: GuardOutcome<(), ()> = run_with_guard(
            trickle,
            last,
            TEST_STALL,
            Duration::from_secs(125),
            TEST_POLL,
        )
        .await;
        assert!(matches!(out, GuardOutcome::TimedOut));
    }

    #[test]
    fn claim_transitions_idle_to_downloading_exactly_once() {
        // Task 30a — the atomic check-and-set. From Idle the first claim wins
        // and flips the phase to Downloading (carrying the version); a second
        // claim now sees a non-Idle phase and loses. This is the single-lock
        // guarantee that closes the old check-then-set gap.
        let phase = Mutex::new(UpdaterPhase::Idle);
        assert!(
            claim_idle_to_downloading(&phase, "1.2.0"),
            "first claim wins"
        );
        assert_eq!(
            *phase.lock().unwrap(),
            UpdaterPhase::Downloading {
                version: "1.2.0".into()
            }
        );
        assert!(
            !claim_idle_to_downloading(&phase, "9.9.9"),
            "second claim must lose — a download is already in flight"
        );
        // The in-flight version is untouched by the losing claim.
        assert_eq!(
            *phase.lock().unwrap(),
            UpdaterPhase::Downloading {
                version: "1.2.0".into()
            }
        );
    }

    #[test]
    fn concurrent_claims_yield_exactly_one_winner() {
        // Many threads race to claim a single Idle phase; exactly one may win.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let phase = Arc::new(Mutex::new(UpdaterPhase::Idle));
        let winners = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for i in 0..16 {
            let phase = Arc::clone(&phase);
            let winners = Arc::clone(&winners);
            handles.push(std::thread::spawn(move || {
                if claim_idle_to_downloading(&phase, &format!("v{i}")) {
                    winners.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            winners.load(Ordering::SeqCst),
            1,
            "exactly one concurrent claim may transition Idle→Downloading"
        );
        assert!(matches!(
            *phase.lock().unwrap(),
            UpdaterPhase::Downloading { .. }
        ));
    }

    #[test]
    fn claim_recovers_a_poisoned_lock() {
        // A prior panic while holding the lock must not wedge the updater — the
        // claim recovers the poisoned guard rather than panicking.
        use std::sync::Arc;
        let phase = Arc::new(Mutex::new(UpdaterPhase::Idle));
        let p2 = Arc::clone(&phase);
        let _ = std::thread::spawn(move || {
            let _g = p2.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert!(phase.is_poisoned());
        assert!(
            claim_idle_to_downloading(&phase, "1.0.0"),
            "claim must recover the poisoned lock and still transition"
        );
    }

    #[test]
    fn download_guard_error_reasons_are_stable_low_cardinality() {
        assert_eq!(DownloadGuardError::Stalled.reason(), "no_progress");
        assert_eq!(DownloadGuardError::TimedOut.reason(), "timeout");
        assert_eq!(
            DownloadGuardError::Failed("any message".into()).reason(),
            "failed"
        );
    }
}
