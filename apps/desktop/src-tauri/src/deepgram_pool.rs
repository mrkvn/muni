//! Single-slot Deepgram WebSocket pre-warming pool (plan 039 slice 25).
//!
//! Extracted verbatim from `session.rs`. Keeps one warm, keepalive-pinged
//! socket parked so the next press streams without paying the handshake.
//! No behavior change from the in-`session.rs` original — visibility widened
//! to `pub(crate)` only where the `session` test module reaches in.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex as TokioMutex, Notify};

use crate::deepgram::DeepgramClient;
use crate::error::MuniError;
use crate::session::{probe_within, KEEPALIVE_INTERVAL, PARKED_PROBE_TIMEOUT, WARMER_BACKOFF_S};

// ---- DeepgramPool ---------------------------------------------------------

/// Resolver for the live Deepgram API key. Called on every warmer
/// attempt and every inline open so a key saved through the
/// onboarding wizard or the Settings → API Keys tab takes effect on
/// the very next press — without needing to reconstruct the pool or
/// restart the app.
///
/// In production this wraps `secrets::get(DEEPGRAM_ACCOUNT)` (env var
/// first, then macOS Keychain). In tests it wraps a constant string.
pub type DeepgramKeyProvider = Arc<dyn Fn() -> Result<String, MuniError> + Send + Sync>;

/// Convenience: build a [`DeepgramKeyProvider`] that always returns the
/// supplied literal. Used by tests; production code should pass a
/// closure that reads from `secrets::get` so live keychain updates
/// propagate.
pub fn fixed_deepgram_key(value: impl Into<String>) -> DeepgramKeyProvider {
    let value = value.into();
    Arc::new(move || Ok(value.clone()))
}

/// Single-slot pre-warming pool for [`DeepgramClient`].
///
/// Always spawns the next warmer as soon as a slot is taken, so the steady
/// state is "one warm socket parked, ready for the next press." The first
/// press of the app's life MAY race the initial warmer; in that case the
/// take path opens its own socket inline (paying the handshake cost ONCE).
///
/// Deepgram closes idle sockets after ~10–15 s, so each parked socket has
/// a dedicated keepalive task that sends `{"type":"KeepAlive"}` every
/// [`KEEPALIVE_INTERVAL`] until [`take`](Self::take) hands the socket off
/// (cancel signalled) or the keepalive send fails (the parked socket is
/// dead — clear the slot, the next take re-warms inline).
///
/// The API key is resolved through [`DeepgramKeyProvider`] on every
/// warmer attempt + inline open. Capturing the key at construction
/// would mean a key saved mid-session through the wizard / Settings
/// would not take effect until the next launch.
pub struct DeepgramPool {
    key_provider: DeepgramKeyProvider,
    endpoint: String,
    keepalive_interval: Duration,
    pub(crate) parked: Arc<TokioMutex<Option<ParkedEntry>>>,
    /// Cumulative count of successfully parked warm sockets. Test-only
    /// signal used to assert the warmer-spawn-on-take invariant.
    warmer_count: Arc<AtomicUsize>,
    /// True while a warmer task is running. Prevents duplicate warmers from
    /// piling up if `take()` is called more than once in quick succession.
    warmer_inflight: Arc<AtomicBool>,
}

/// Parked socket plus the cancellation handle that lets `take()` (or a
/// failed keepalive) tell the keepalive task to stop pinging.
pub(crate) struct ParkedEntry {
    pub(crate) client: Arc<DeepgramClient>,
    pub(crate) keepalive_cancel: Arc<Notify>,
}

impl DeepgramPool {
    /// Spawn a pool pointing at the Deepgram endpoint (production by default,
    /// or the `MUNI_DEEPGRAM_ENDPOINT` override for local-mock / dogfood
    /// sessions) and schedule the first warmer. The resolved endpoint flows to
    /// both the warmer and the inline `take()` open.
    pub fn spawn(key_provider: DeepgramKeyProvider) -> Arc<Self> {
        Self::spawn_with_endpoint(key_provider, crate::deepgram::resolve_deepgram_endpoint())
    }

    /// Variant of [`spawn`](Self::spawn) that accepts a custom endpoint.
    /// Used by integration tests to point the pool at a local mock WS.
    pub fn spawn_with_endpoint(key_provider: DeepgramKeyProvider, endpoint: String) -> Arc<Self> {
        Self::spawn_with_endpoint_and_keepalive(key_provider, endpoint, KEEPALIVE_INTERVAL)
    }

    /// Variant of [`spawn_with_endpoint`](Self::spawn_with_endpoint) that
    /// accepts a custom keepalive cadence. Used by integration tests to
    /// drive the keepalive lifecycle in seconds rather than minutes.
    pub fn spawn_with_endpoint_and_keepalive(
        key_provider: DeepgramKeyProvider,
        endpoint: String,
        keepalive_interval: Duration,
    ) -> Arc<Self> {
        let pool = Arc::new(Self {
            key_provider,
            endpoint,
            keepalive_interval,
            parked: Arc::new(TokioMutex::new(None)),
            warmer_count: Arc::new(AtomicUsize::new(0)),
            warmer_inflight: Arc::new(AtomicBool::new(false)),
        });
        pool.schedule_warmer();
        pool
    }

    /// Atomically take the parked socket — or open one inline if the pool is
    /// empty — and schedule the next warmer.
    ///
    /// Returns [`MuniError::DeepgramConnectionFailed`] (or
    /// [`MuniError::DeepgramMissingApiKey`]) only on the inline-open fall
    /// through path. The parked client is liveness-probed before being
    /// returned: a sudden network drop can leave the WS in a half-closed
    /// state that the keepalive task hasn't yet noticed (its sleep is up
    /// to [`KEEPALIVE_INTERVAL`] long), and serving that closed socket
    /// would dump hundreds of "Sending after closing is not allowed"
    /// errors into the forwarder before finalize gives up. The probe
    /// reuses the existing `send_keepalive` (one tiny text frame); on
    /// failure we discard the parked entry and fall through to inline
    /// open.
    pub async fn take(&self) -> Result<Arc<DeepgramClient>, MuniError> {
        let parked = {
            let mut g = self.parked.lock().await;
            g.take()
        };
        // Fire-and-forget: schedule the next warmer immediately so it
        // overlaps with the current press's audio path.
        self.schedule_warmer();
        if let Some(entry) = parked {
            // Stop the keepalive task so it can't race with the press's
            // audio sends on the same WebSocket sink.
            entry.keepalive_cancel.notify_waiters();
            // Liveness probe: a parked WS that's been silently closed
            // by a network drop would otherwise feed the forwarder a
            // dead sink. send_keepalive returns Err immediately when
            // the underlying tungstenite stream is in Closed state.
            //
            // Plan 039 slice 4 (task 12): the probe gets its OWN short budget
            // ([`PARKED_PROBE_TIMEOUT`], ~200 ms) instead of riding
            // send_keepalive's 1 s `SEND_TIMEOUT`. A truly wedged half-open
            // socket (full kernel buffer) would otherwise make send_keepalive
            // block for the full second BEFORE we give up and open inline —
            // a whole second of press-start latency on the very presses the
            // probe exists to protect. On probe timeout OR error we discard
            // the parked slot and fall through to an inline open.
            if probe_within(PARKED_PROBE_TIMEOUT, entry.client.send_keepalive()).await {
                log::info!(target: "session", "Deepgram pool: served from warm slot");
                return Ok(entry.client);
            }
            log::warn!(
                target: "session",
                "parked WS failed liveness probe (or timed out after {:?}) — opening inline",
                PARKED_PROBE_TIMEOUT
            );
            // Fire-and-forget the discard. `close()` on a wedged half-open
            // socket blocks up to `deepgram::CLOSE_TIMEOUT` (500 ms) draining
            // the close frame into a full kernel buffer — and we're on the
            // press-start critical path (speed is priority #1). We've already
            // decided to abandon this slot, so detach the teardown and fall
            // straight through to the inline open. Plan 039 slice 4 (task 12).
            let discarded = entry.client;
            tauri::async_runtime::spawn(async move {
                discarded.close().await;
            });
        } else {
            log::warn!(
                target: "session",
                "Deepgram pool empty — opening WS inline (talk-too-soon gap on this press)"
            );
        }
        let api_key = (self.key_provider)()?;
        DeepgramClient::open_at(&api_key, &self.endpoint)
            .await
            .map(Arc::new)
    }

    /// Cumulative number of warm sockets the pool has successfully parked
    /// since creation. Test-only — no production callers should depend on
    /// this counter.
    pub fn warmer_count(&self) -> usize {
        self.warmer_count.load(Ordering::SeqCst)
    }

    /// Drop the parked WebSocket (if any) and immediately schedule a
    /// fresh warmer.
    ///
    /// Called whenever the Deepgram API key changes — `secrets_set_*`,
    /// `secrets_delete_*`. The parked socket was opened with the
    /// previous key and is already authenticated against Deepgram's
    /// servers; without this, a "Remove saved key" + immediate press
    /// would happily stream through the stale-but-authenticated WS
    /// (QA repro: the first press after key removal pasted
    /// successfully). Killing the slot makes the next press resolve
    /// the current key — empty → `DeepgramMissingApiKey` loud — so
    /// the user-visible behavior matches the configured state.
    pub async fn clear_parked(&self) {
        let parked = {
            let mut g = self.parked.lock().await;
            g.take()
        };
        if let Some(entry) = parked {
            log::info!(target: "session", "Deepgram pool: parked WS invalidated (key changed)");
            entry.keepalive_cancel.notify_waiters();
            entry.client.close().await;
        }
        self.schedule_warmer();
    }

    fn schedule_warmer(&self) {
        if self.warmer_inflight.swap(true, Ordering::SeqCst) {
            // A warmer is already running; it will park its result and the
            // next take() will see it.
            return;
        }
        let key_provider = self.key_provider.clone();
        let endpoint = self.endpoint.clone();
        let keepalive_interval = self.keepalive_interval;
        let parked = self.parked.clone();
        let warmer_count = self.warmer_count.clone();
        let warmer_inflight = self.warmer_inflight.clone();
        tauri::async_runtime::spawn(async move {
            let mut attempt = 0usize;
            loop {
                // Resolve the key fresh on every attempt — a key
                // saved through Settings → API Keys (or the wizard)
                // mid-warmer-backoff takes effect on the next try
                // without waiting for an app restart.
                let key_result = (key_provider)();
                let api_key = match key_result {
                    Ok(k) => k,
                    Err(err) => {
                        if attempt >= WARMER_BACKOFF_S.len() {
                            log::error!(
                                target: "session",
                                "Deepgram warmer giving up after {} attempts: {}",
                                attempt,
                                err.user_message()
                            );
                            break;
                        }
                        let backoff = WARMER_BACKOFF_S[attempt];
                        log::warn!(
                            target: "session",
                            "Deepgram warmer failed (attempt {}): {} — retrying in {}s",
                            attempt + 1,
                            err.user_message(),
                            backoff
                        );
                        attempt = attempt.saturating_add(1);
                        tokio::time::sleep(Duration::from_secs(backoff)).await;
                        continue;
                    }
                };
                match DeepgramClient::open_at(&api_key, &endpoint).await {
                    Ok(client) => {
                        let client = Arc::new(client);
                        let cancel = Arc::new(Notify::new());
                        let mut g = parked.lock().await;
                        if g.is_some() {
                            // Race: someone else parked between our open
                            // start and now. Drop our duplicate cleanly.
                            log::debug!(
                                target: "session",
                                "warmer found existing parked WS — closing duplicate"
                            );
                            drop(g);
                            client.close().await;
                        } else {
                            // Spawn keepalive BEFORE releasing the lock so a
                            // racing take() can't grab the entry without a
                            // ticking keepalive cancel handle.
                            spawn_keepalive(
                                parked.clone(),
                                client.clone(),
                                cancel.clone(),
                                keepalive_interval,
                            );
                            *g = Some(ParkedEntry {
                                client,
                                keepalive_cancel: cancel,
                            });
                            log::info!(target: "session", "Deepgram WS warmed and parked");
                        }
                        warmer_count.fetch_add(1, Ordering::SeqCst);
                        break;
                    }
                    Err(err) => {
                        if attempt >= WARMER_BACKOFF_S.len() {
                            log::error!(
                                target: "session",
                                "Deepgram warmer giving up after {} attempts: {}",
                                attempt,
                                err.user_message()
                            );
                            break;
                        }
                        let backoff = WARMER_BACKOFF_S[attempt];
                        log::warn!(
                            target: "session",
                            "Deepgram warmer failed (attempt {}): {} — retrying in {}s",
                            attempt + 1,
                            err.user_message(),
                            backoff
                        );
                        attempt = attempt.saturating_add(1);
                        tokio::time::sleep(Duration::from_secs(backoff)).await;
                    }
                }
            }
            // Release the inflight flag so the next take() — or a future
            // take() that follows a give-up — can schedule a fresh warmer.
            warmer_inflight.store(false, Ordering::SeqCst);
        });
    }
}

// ---- helpers --------------------------------------------------------------

/// Spawn a task that sends `KeepAlive` frames on the parked socket every
/// `interval` until either:
/// - `cancel` is signalled (a take handed the socket off), or
/// - a keepalive send fails (the socket is dead — clear the parked slot
///   so the next take re-warms inline rather than serving a corpse).
fn spawn_keepalive(
    parked: Arc<TokioMutex<Option<ParkedEntry>>>,
    client: Arc<DeepgramClient>,
    cancel: Arc<Notify>,
    interval: Duration,
) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                () = cancel.notified() => {
                    log::debug!(target: "session", "keepalive cancelled (slot taken)");
                    return;
                }
                () = tokio::time::sleep(interval) => {}
            }
            if let Err(err) = client.send_keepalive().await {
                log::warn!(
                    target: "session",
                    "Deepgram keepalive failed — discarding parked WS: {}",
                    err.user_message()
                );
                // Clear the slot if it still references THIS client. ptr_eq
                // protects against the race where take() already swapped in
                // a fresh entry for someone else.
                let mut g = parked.lock().await;
                let same = g
                    .as_ref()
                    .map(|e| Arc::ptr_eq(&e.client, &client))
                    .unwrap_or(false);
                if same {
                    g.take();
                }
                return;
            }
            log::debug!(target: "session", "Deepgram keepalive sent");
        }
    });
}
