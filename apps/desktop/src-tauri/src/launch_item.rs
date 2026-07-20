//! Launch-at-Login registration.
//!
//! On macOS this wraps `SMAppService` (ServiceManagement, macOS 13+; our
//! deployment target is 14.0). It replaces the previous
//! `tauri-plugin-autostart` AppleScript path, which scripted
//! `System Events` to add a login item and therefore tripped the macOS
//! Automation prompt — *"Muni wants to control System Events"* — on every
//! enable / disable / probe.
//!
//! `SMAppService` registers the **main app itself** as a login item, so
//! there is no second app to authorize: no prompt is ever shown, the entry
//! still appears under System Settings → General → Login Items → "Open at
//! Login", and `status()` reports the real state (including the user having
//! switched it off in System Settings → `RequiresApproval`).
//!
//! Non-macOS targets are not shipped yet (Windows/Linux are post-v1); the
//! stubs there are no-ops, mirroring the unimplemented injection backend.

/// The OS-level launch-at-login state, normalized across platforms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchItemStatus {
    /// Registered and will launch at login.
    Enabled,
    /// Never registered (or unregistered).
    NotRegistered,
    /// Registered, but the user has switched it off in System Settings.
    /// Re-registering will not flip this — only the user can, in Settings.
    RequiresApproval,
    /// The OS has no record of this service.
    NotFound,
}

impl LaunchItemStatus {
    /// True only when the app will actually launch at login. `RequiresApproval`
    /// is deliberately *not* enabled: it means the user turned Muni off in
    /// System Settings, and the in-app toggle should reflect that as off.
    pub fn is_enabled(self) -> bool {
        matches!(self, LaunchItemStatus::Enabled)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::LaunchItemStatus;
    use objc2_service_management::{SMAppService, SMAppServiceStatus};
    use std::time::Duration;

    /// `kSMErrorAlreadyRegistered` — returned by `register` when the service
    /// is already registered. Idempotent for us: treat as success.
    const SM_ERROR_ALREADY_REGISTERED: isize = 12;
    /// `kSMErrorJobNotFound` — returned by `unregister` when nothing is
    /// registered. Idempotent for us: treat as success.
    const SM_ERROR_JOB_NOT_FOUND: isize = 6;

    fn read_status() -> LaunchItemStatus {
        // SAFETY: `mainAppService` returns the process-wide singleton for the
        // running app bundle; `status` is a pure read with no side effects and
        // never prompts. Both are FFI calls into a system framework.
        let status = unsafe { SMAppService::mainAppService().status() };
        match status {
            SMAppServiceStatus::Enabled => LaunchItemStatus::Enabled,
            SMAppServiceStatus::RequiresApproval => LaunchItemStatus::RequiresApproval,
            SMAppServiceStatus::NotFound => LaunchItemStatus::NotFound,
            // NotRegistered (0) and any future/unknown value fall back to the
            // safe "not launching" reading.
            _ => LaunchItemStatus::NotRegistered,
        }
    }

    pub fn status() -> LaunchItemStatus {
        read_status()
    }

    pub fn set_enabled(enabled: bool) -> Result<(), String> {
        // SAFETY: `mainAppService` is the running app's singleton; register /
        // unregister mutate the OS login-item record for *this* app only and
        // never prompt (no second app is being controlled).
        let service = unsafe { SMAppService::mainAppService() };
        let current = read_status();

        if enabled {
            if current == LaunchItemStatus::Enabled {
                return Ok(());
            }
            let result = unsafe { service.registerAndReturnError() };
            match result {
                Ok(()) => Ok(()),
                Err(err) => {
                    let code = err.code();
                    if code == SM_ERROR_ALREADY_REGISTERED {
                        return Ok(());
                    }
                    Err(format!(
                        "SMAppService register failed (code {code}): {}",
                        err.localizedDescription()
                    ))
                }
            }
        } else {
            if matches!(
                current,
                LaunchItemStatus::NotRegistered | LaunchItemStatus::NotFound
            ) {
                return Ok(());
            }
            let result = unsafe { service.unregisterAndReturnError() };
            match result {
                Ok(()) => Ok(()),
                Err(err) => {
                    let code = err.code();
                    if code == SM_ERROR_JOB_NOT_FOUND {
                        return Ok(());
                    }
                    Err(format!(
                        "SMAppService unregister failed (code {code}): {}",
                        err.localizedDescription()
                    ))
                }
            }
        }
    }

    /// One-time migration helper: delete the legacy `System Events` login
    /// item created by the old `tauri-plugin-autostart` AppleScript path.
    ///
    /// `SMAppService` registers a *separate* record, so a leftover AppleScript
    /// entry for the same bundle would survive the migration and cause a
    /// double-launch at login. We remove it once, best-effort.
    ///
    /// Anyone who had Launch-at-Login enabled under the old mechanism already
    /// granted "control System Events", so this runs **silently** for them.
    /// New installs never reach here (the caller gates on the prior pref), so
    /// they never see an Automation prompt.
    pub fn remove_legacy_login_item() {
        // Plan 039 task 34 — detach the osascript run + bounded wait onto a
        // background thread so `setup` never stalls (up to 3 s worst case) on a
        // slow or hung `System Events`. The migration is best-effort cleanup and
        // the caller doesn't consume its result, so fire-and-forget is correct;
        // the OS reaps the child, and the bounded wait below still caps the
        // thread's own lifetime.
        std::thread::spawn(run_legacy_cleanup);
    }

    fn run_legacy_cleanup() {
        // `delete login item "Muni"` throws a benign AppleScript error if the
        // item is absent; we ignore all failures — this is pure cleanup.
        let script = r#"tell application "System Events" to delete login item "Muni""#;
        let child = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();

        let Ok(mut child) = child else {
            log::warn!(target: "autostart", "legacy login-item cleanup: osascript spawn failed");
            return;
        };

        // Bound the wait so a hung System Events can't stall boot. The work is
        // trivial; a couple seconds is generous.
        let deadline = Duration::from_secs(3);
        let start = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    log::info!(
                        target: "autostart",
                        "legacy login-item cleanup finished (status={status})"
                    );
                    return;
                }
                Ok(None) => {
                    if start.elapsed() >= deadline {
                        let _ = child.kill();
                        log::warn!(target: "autostart", "legacy login-item cleanup timed out");
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(err) => {
                    log::warn!(target: "autostart", "legacy login-item cleanup wait failed: {err}");
                    return;
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::LaunchItemStatus;

    pub fn status() -> LaunchItemStatus {
        LaunchItemStatus::NotRegistered
    }

    pub fn set_enabled(_enabled: bool) -> Result<(), String> {
        // Launch-at-login is macOS-only until Windows/Linux ship (post-v1).
        Ok(())
    }

    pub fn remove_legacy_login_item() {}
}

/// Current OS launch-at-login status. Never prompts.
pub fn status() -> LaunchItemStatus {
    imp::status()
}

/// True when the app will launch at login. Never prompts.
pub fn is_enabled() -> bool {
    imp::status().is_enabled()
}

/// Register (`enabled = true`) or unregister (`false`) the app as a login
/// item. Idempotent. Never prompts.
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    imp::set_enabled(enabled)
}

/// Best-effort one-time removal of the pre-SMAppService AppleScript login
/// item. See the macOS implementation for the silent-vs-prompt rationale.
pub fn remove_legacy_login_item() {
    imp::remove_legacy_login_item();
}
