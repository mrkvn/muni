//! Phase 9 — OS permission probes used by the Onboarding wizard.
//!
//! The orchestrator already has its own one-shot probes inside `audio.rs`
//! (microphone) and `injection/macos.rs` (Accessibility) that map to typed
//! `MuniError` variants for the runtime hot path. This module is the
//! wizard-side counterpart: callers want a *plain status* they can map to
//! UI copy ("Granted" / "Denied" / "Not asked yet"), plus the ability to
//! actively trigger the system prompts on macOS.
//!
//! Non-macOS builds expose the same symbols as no-op stubs so the
//! commands compile cross-platform; the wizard isn't shown there yet (the
//! onboarding window is registered for macOS-style permissions only) but
//! keeping the interface uniform avoids `#[cfg]` peppering at the IPC
//! boundary.

use serde::Serialize;

/// Authorisation status for the system microphone, mirroring
/// `AVAuthorizationStatus` (`AVCaptureDevice.h`). Serialised in
/// camelCase to match the rest of the IPC surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MicrophoneStatus {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
}

/// Authorisation status for Input Monitoring
/// (`kIOHIDRequestTypeListenEvent`). Wraps the four possible values
/// `IOHIDCheckAccess` returns (the enum is `IOHIDAccessType`):
/// 0 = granted, 1 = denied, 2 = unknown / not determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InputMonitoringStatus {
    NotDetermined,
    Denied,
    Granted,
    Unknown,
}

#[cfg(target_os = "macos")]
mod imp {
    use super::MicrophoneStatus;

    /// Read the current `AVAuthorizationStatus` for audio without prompting.
    pub fn microphone_status() -> MicrophoneStatus {
        use objc2::{class, msg_send};
        use objc2_foundation::NSString;

        // AVAuthorizationStatus enum values from AVCaptureDevice.h.
        const STATUS_NOT_DETERMINED: isize = 0;
        const STATUS_RESTRICTED: isize = 1;
        const STATUS_DENIED: isize = 2;
        const STATUS_AUTHORIZED: isize = 3;

        #[link(name = "AVFoundation", kind = "framework")]
        unsafe extern "C" {
            static AVMediaTypeAudio: &'static NSString;
        }

        // SAFETY: thread-safe class method on a singleton class shipped with
        // AVFoundation; the call has no side effects beyond returning the
        // current TCC status integer.
        let raw: isize = unsafe {
            let cls = class!(AVCaptureDevice);
            msg_send![cls, authorizationStatusForMediaType: AVMediaTypeAudio]
        };
        match raw {
            STATUS_NOT_DETERMINED => MicrophoneStatus::NotDetermined,
            STATUS_RESTRICTED => MicrophoneStatus::Restricted,
            STATUS_DENIED => MicrophoneStatus::Denied,
            STATUS_AUTHORIZED => MicrophoneStatus::Authorized,
            other => {
                log::warn!(
                    target: "permissions",
                    "unknown AVAuthorizationStatus={other}; treating as Denied"
                );
                MicrophoneStatus::Denied
            }
        }
    }

    /// Fire `+[AVCaptureDevice requestAccessForMediaType:completionHandler:]`
    /// and resolve once the user dismisses the system prompt.
    ///
    /// On `notDetermined`, the OS shows the modal. On any other state the
    /// completion handler fires synchronously with the current decision —
    /// AppKit short-circuits without re-prompting, which matches what we
    /// want (the wizard's "Open System Settings" button handles the
    /// already-denied case explicitly).
    pub async fn request_microphone() -> bool {
        // The Objective-C block is created synchronously, retained by
        // AppKit on the `msg_send!` line, then dropped locally. Pulling
        // that into a separate non-async helper keeps the resulting
        // future `Send` — `RcBlock` is `!Send`, so holding it across an
        // `.await` would otherwise poison the whole IPC handler.
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        fire_microphone_request(tx);
        match rx.await {
            Ok(granted) => granted,
            Err(_) => {
                // Sender dropped without firing. Defensive — treat as
                // denied so the wizard doesn't claim success spuriously.
                log::warn!(
                    target: "permissions",
                    "microphone request completion never fired"
                );
                false
            }
        }
    }

    fn fire_microphone_request(tx: tokio::sync::oneshot::Sender<bool>) {
        use std::sync::{Arc, Mutex};

        use block2::RcBlock;
        use objc2::runtime::Bool;
        use objc2::{class, msg_send};
        use objc2_foundation::NSString;

        #[link(name = "AVFoundation", kind = "framework")]
        unsafe extern "C" {
            static AVMediaTypeAudio: &'static NSString;
        }

        // `RcBlock::new` requires `Fn`, not `FnOnce`, so the sender is
        // wrapped in `Arc<Mutex<Option<_>>>` to allow the single fire to
        // consume it without breaking the trait bound.
        let sender = Arc::new(Mutex::new(Some(tx)));
        let block = RcBlock::new(move |granted: Bool| {
            if let Ok(mut guard) = sender.lock() {
                if let Some(tx) = guard.take() {
                    let _ = tx.send(granted.as_bool());
                }
            }
        });

        // SAFETY: AVCaptureDevice + the selector are linked via the
        // `AVFoundation` framework. The block is heap-allocated by
        // `RcBlock` (Apple requires the block to outlive the call), and
        // `&*block` produces the `&Block<dyn Fn(Bool)>` reference that
        // `msg_send!` knows how to lower. AppKit retains its own +1
        // reference on the block before returning, so dropping the
        // local `RcBlock` at end-of-scope is safe.
        unsafe {
            let cls = class!(AVCaptureDevice);
            let _: () = msg_send![
                cls,
                requestAccessForMediaType: AVMediaTypeAudio,
                completionHandler: &*block
            ];
        }
    }

    /// `AXIsProcessTrusted()` — pure status read, no UI side effect.
    pub fn is_accessibility_trusted() -> bool {
        #[link(name = "ApplicationServices", kind = "framework")]
        unsafe extern "C" {
            fn AXIsProcessTrusted() -> bool;
        }
        // SAFETY: documented thread-safe.
        unsafe { AXIsProcessTrusted() }
    }

    /// Read the current Input Monitoring (kIOHIDRequestTypeListenEvent)
    /// authorisation. Pure status read, no UI side effect — equivalent
    /// to `IOHIDCheckAccess` on macOS Catalina+.
    pub fn input_monitoring_status() -> super::InputMonitoringStatus {
        // IOHIDAccessType (from <IOKit/hid/IOHIDLib.h>):
        const ACCESS_GRANTED: u32 = 0;
        const ACCESS_DENIED: u32 = 1;
        const ACCESS_UNKNOWN: u32 = 2;
        const REQUEST_TYPE_LISTEN_EVENT: u32 = 1;

        #[link(name = "IOKit", kind = "framework")]
        unsafe extern "C" {
            fn IOHIDCheckAccess(request: u32) -> u32;
        }

        // SAFETY: IOHIDCheckAccess is a thread-safe pure-status read
        // shipped with IOKit on macOS 10.15+.
        let raw = unsafe { IOHIDCheckAccess(REQUEST_TYPE_LISTEN_EVENT) };
        match raw {
            ACCESS_GRANTED => super::InputMonitoringStatus::Granted,
            ACCESS_DENIED => super::InputMonitoringStatus::Denied,
            ACCESS_UNKNOWN => super::InputMonitoringStatus::NotDetermined,
            other => {
                log::warn!(
                    target: "permissions",
                    "unknown IOHIDAccessType={other}; treating as Unknown"
                );
                super::InputMonitoringStatus::Unknown
            }
        }
    }

    /// Trigger the macOS "Keystroke Receiving" prompt. Returns the
    /// resulting status — `IOHIDRequestAccess` is documented as
    /// synchronous-but-may-prompt: on first call, macOS shows the
    /// modal and BLOCKS until the user dismisses it; on subsequent
    /// calls, returns the cached TCC decision instantly.
    ///
    /// Recovery if the user dismissed the prompt without granting:
    /// the wizard's "Open System Settings" deep link opens the
    /// Input Monitoring pane so the toggle can be flipped manually.
    pub fn request_input_monitoring() -> super::InputMonitoringStatus {
        const REQUEST_TYPE_LISTEN_EVENT: u32 = 1;

        #[link(name = "IOKit", kind = "framework")]
        unsafe extern "C" {
            fn IOHIDRequestAccess(request: u32) -> bool;
        }

        // SAFETY: IOHIDRequestAccess is shipped with IOKit on macOS
        // 10.15+. The bool return is the granted-or-not decision; we
        // discard it and re-read the authoritative status below to
        // unify the success/failure surface with the polling path.
        let _ = unsafe { IOHIDRequestAccess(REQUEST_TYPE_LISTEN_EVENT) };
        input_monitoring_status()
    }

    /// `AXIsProcessTrustedWithOptions({kAXTrustedCheckOptionPrompt: true})` —
    /// triggers the macOS Accessibility prompt the first time a process
    /// asks. Returns the trusted status at the moment of the call (the
    /// prompt itself is asynchronous; the wizard polls
    /// [`is_accessibility_trusted`] afterwards).
    pub fn prompt_accessibility() -> bool {
        use core_foundation::base::TCFType;
        use core_foundation::boolean::CFBoolean;
        use core_foundation::dictionary::CFDictionary;
        use core_foundation::string::CFString;
        use core_foundation_sys::dictionary::CFDictionaryRef;
        use core_foundation_sys::string::CFStringRef;

        #[link(name = "ApplicationServices", kind = "framework")]
        unsafe extern "C" {
            static kAXTrustedCheckOptionPrompt: CFStringRef;
            fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
        }

        // SAFETY: `kAXTrustedCheckOptionPrompt` is a process-lifetime
        // CFStringRef constant exported by ApplicationServices.
        // `wrap_under_get_rule` retains it; the local CFDictionary owns
        // the +1 reference until it's dropped at end-of-scope.
        let prompt_key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
        let prompt_value = CFBoolean::true_value();
        let options = CFDictionary::from_CFType_pairs(&[(prompt_key, prompt_value)]);
        // SAFETY: `options.as_concrete_TypeRef()` borrows the underlying
        // CFDictionaryRef; AXIsProcessTrustedWithOptions does not retain
        // it past the call.
        unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{InputMonitoringStatus, MicrophoneStatus};

    pub fn microphone_status() -> MicrophoneStatus {
        // No TCC equivalent exists on non-macOS yet; treat the platform as
        // permanently authorised so onboarding can advance through this
        // step on dev machines. The runtime audio stack will surface a
        // `MuniError::AudioStreamFailed` if the underlying API rejects.
        MicrophoneStatus::Authorized
    }

    pub async fn request_microphone() -> bool {
        true
    }

    pub fn is_accessibility_trusted() -> bool {
        true
    }

    pub fn prompt_accessibility() -> bool {
        true
    }

    pub fn input_monitoring_status() -> InputMonitoringStatus {
        InputMonitoringStatus::Granted
    }

    pub fn request_input_monitoring() -> InputMonitoringStatus {
        InputMonitoringStatus::Granted
    }
}

pub use imp::{
    input_monitoring_status, is_accessibility_trusted, microphone_status, prompt_accessibility,
    request_input_monitoring, request_microphone,
};

/// Tagged target for `open_system_settings` deep links. Restricting the
/// IPC to a closed enum means a compromised webview can't ask the host
/// to launch arbitrary `x-apple.systempreferences:` payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemSettingsTarget {
    Microphone,
    Accessibility,
    InputMonitoring,
}

impl SystemSettingsTarget {
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "microphone" => Some(Self::Microphone),
            "accessibility" => Some(Self::Accessibility),
            "input-monitoring" | "inputMonitoring" => Some(Self::InputMonitoring),
            _ => None,
        }
    }

    /// macOS deep-link URL for the matching System Settings pane.
    /// Verified against the Ventura+ pane identifiers; on macOS 14 the
    /// scheme still resolves through the legacy preference IDs.
    pub fn deep_link(self) -> &'static str {
        match self {
            Self::Microphone => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
            }
            Self::Accessibility => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            Self::InputMonitoring => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_round_trips_through_wire_format() {
        for (wire, target, link_fragment) in [
            (
                "microphone",
                SystemSettingsTarget::Microphone,
                "Privacy_Microphone",
            ),
            (
                "accessibility",
                SystemSettingsTarget::Accessibility,
                "Privacy_Accessibility",
            ),
            (
                "input-monitoring",
                SystemSettingsTarget::InputMonitoring,
                "Privacy_ListenEvent",
            ),
        ] {
            assert_eq!(SystemSettingsTarget::from_wire(wire), Some(target));
            assert!(target.deep_link().contains(link_fragment));
        }
    }

    #[test]
    fn unknown_target_is_rejected() {
        assert!(SystemSettingsTarget::from_wire("camera").is_none());
        assert!(SystemSettingsTarget::from_wire("").is_none());
    }

    #[test]
    fn input_monitoring_accepts_camel_case_alias() {
        // The frontend has historically sent both kebab- and camelCase;
        // accepting both keeps the IPC forgiving without widening the
        // closed set of allowed targets.
        assert_eq!(
            SystemSettingsTarget::from_wire("inputMonitoring"),
            Some(SystemSettingsTarget::InputMonitoring)
        );
    }

    #[test]
    fn microphone_status_serialises_to_camel_case() {
        let json = serde_json::to_string(&MicrophoneStatus::NotDetermined).unwrap();
        assert_eq!(json, "\"notDetermined\"");
        let json = serde_json::to_string(&MicrophoneStatus::Authorized).unwrap();
        assert_eq!(json, "\"authorized\"");
    }

    #[test]
    fn input_monitoring_status_serialises_to_camel_case() {
        // The frontend switches on the wire string; locking the
        // serialisation here means a future variant rename can't
        // silently break the wizard's gating logic.
        for (variant, wire) in [
            (InputMonitoringStatus::NotDetermined, "\"notDetermined\""),
            (InputMonitoringStatus::Denied, "\"denied\""),
            (InputMonitoringStatus::Granted, "\"granted\""),
            (InputMonitoringStatus::Unknown, "\"unknown\""),
        ] {
            assert_eq!(serde_json::to_string(&variant).unwrap(), wire);
        }
    }
}
