//! macOS CoreAudio device-change watcher.
//!
//! Listens for two property changes on the system audio object:
//! - `kAudioHardwarePropertyDevices` — fires when an input/output
//!   device is added or removed (USB plug/unplug, Bluetooth connect/
//!   disconnect).
//! - `kAudioHardwarePropertyDefaultInputDevice` — fires when the OS
//!   switches the default input (e.g. AirPods auto-claim the default
//!   on connect).
//!
//! On any matched event we emit the [`EVENT_DEVICES_CHANGED`] Tauri
//! event; the tray module subscribes to it and rebuilds the
//! `Microphone ▸` submenu so the user never has to click `Restart
//! Muni` to see a freshly-plugged mic.
//!
//! The callback is invoked on a CoreAudio property-listener thread
//! (not a realtime audio thread), so emitting a Tauri event from it
//! is safe — Tauri's event router fans out to the main thread before
//! the registered listener runs.

#[cfg(target_os = "macos")]
pub use macos::{start, EVENT_DEVICES_CHANGED};

#[cfg(not(target_os = "macos"))]
pub use stub::{start, EVENT_DEVICES_CHANGED};

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;
    use std::sync::OnceLock;

    use tauri::{AppHandle, Emitter};

    /// Tauri event emitted whenever CoreAudio notifies us that the
    /// device set or the default input changed. The tray's listener
    /// is the only consumer today; future callers (a Settings audio
    /// panel, a status pill) can subscribe to the same event.
    pub const EVENT_DEVICES_CHANGED: &str = "audio://devices-changed";

    // -- FourCharCode constants -------------------------------------
    // CoreAudio property selectors are encoded as 'four-char codes'
    // (4 ASCII bytes packed into a u32, big-endian). The literal
    // values shipped in CoreAudio headers — duplicated here so we
    // don't need to pull in `coreaudio-sys` for four constants.

    const K_AUDIO_OBJECT_SYSTEM_OBJECT: u32 = 1;
    const K_AUDIO_HARDWARE_PROPERTY_DEVICES: u32 = fourcc(*b"dev#");
    const K_AUDIO_HARDWARE_PROPERTY_DEFAULT_INPUT_DEVICE: u32 = fourcc(*b"dIn ");
    const K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL: u32 = fourcc(*b"glob");
    const K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN: u32 = 0;

    const fn fourcc(s: [u8; 4]) -> u32 {
        ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct AudioObjectPropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }

    type Listener = unsafe extern "C" fn(
        in_object_id: u32,
        in_number_addresses: u32,
        in_addresses: *const AudioObjectPropertyAddress,
        in_client_data: *mut c_void,
    ) -> i32;

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectAddPropertyListener(
            in_object_id: u32,
            in_address: *const AudioObjectPropertyAddress,
            in_listener: Listener,
            in_client_data: *mut c_void,
        ) -> i32;
    }

    /// Static slot for the AppHandle the callback emits through.
    /// `OnceLock` keeps this safe-by-construction: a single writer at
    /// startup, many readers thereafter. The callback dereferences it
    /// without locking on the hot path.
    static APP: OnceLock<AppHandle> = OnceLock::new();

    /// Install the CoreAudio listeners and remember the AppHandle the
    /// callback should emit through. Idempotent — a second call is a
    /// no-op with a warn log.
    pub fn start(app: AppHandle) {
        if APP.set(app).is_err() {
            log::warn!(target: "audio", "device watcher: start called twice; ignoring");
            return;
        }
        install(K_AUDIO_HARDWARE_PROPERTY_DEVICES, "devices");
        install(
            K_AUDIO_HARDWARE_PROPERTY_DEFAULT_INPUT_DEVICE,
            "default_input",
        );
        log::info!(target: "audio", "device watcher: CoreAudio listeners armed");
    }

    fn install(selector: u32, name: &'static str) {
        let addr = AudioObjectPropertyAddress {
            selector,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };
        // SAFETY: `AudioObjectAddPropertyListener` is a documented C
        // function in CoreAudio.framework (linked above). The address
        // pointer is to a stack value — CoreAudio copies the struct
        // before returning. `callback` is `extern "C"` and remains
        // valid for the process lifetime. `inClientData` is null
        // because the callback retrieves the AppHandle via `APP`
        // instead of relying on a Rust closure.
        let status = unsafe {
            AudioObjectAddPropertyListener(
                K_AUDIO_OBJECT_SYSTEM_OBJECT,
                &addr,
                callback,
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            log::warn!(
                target: "audio",
                "AudioObjectAddPropertyListener({name}) failed: OSStatus={status}"
            );
        }
    }

    /// CoreAudio property-listener callback. Runs on a CoreAudio
    /// notifier thread (NOT the realtime audio thread), so non-RT
    /// work — emitting a Tauri event — is safe. Failures are
    /// swallowed because the callback cannot return a meaningful
    /// error to CoreAudio and logging from this context risks
    /// recursion if log sinks themselves touch audio resources.
    extern "C" fn callback(
        _in_object_id: u32,
        _in_number_addresses: u32,
        _in_addresses: *const AudioObjectPropertyAddress,
        _in_client_data: *mut c_void,
    ) -> i32 {
        if let Some(app) = APP.get() {
            let _ = app.emit(EVENT_DEVICES_CHANGED, ());
        }
        0
    }
}

#[cfg(not(target_os = "macos"))]
mod stub {
    use tauri::AppHandle;

    /// Same event name on all platforms so consumers (the tray) can
    /// subscribe unconditionally. On non-macOS the event simply
    /// never fires until/unless a platform-specific watcher lands.
    pub const EVENT_DEVICES_CHANGED: &str = "audio://devices-changed";

    /// No-op on platforms without a CoreAudio listener.
    pub fn start(_app: AppHandle) {}
}
