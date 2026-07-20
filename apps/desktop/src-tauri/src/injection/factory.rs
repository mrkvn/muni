//! Platform selector for the default [`PlatformInjector`].
//!
//! Centralizes the `cfg(target_os = ...)` switch so call sites in `lib.rs` /
//! `session.rs` stay platform-agnostic and the matrix of supported platforms
//! is grep-able from one place.

use std::sync::Arc;

use super::{PlatformInjector, SerializedInjector};

/// Construct the platform-default injector with a caller-supplied paste delay.
///
/// `paste_delay_ms` is the user-configurable `paste.delay_ms` setting (see
/// [`crate::settings::KEY_PASTE_DELAY_MS`]), read from the store at boot. The
/// macOS backend clamps it to [`super::macos::MAX_PASTE_DELAY_MS`] so a corrupt
/// or pathological stored value can't stall every paste (Priority #1: speed).
/// The restore delay is deliberately *not* exposed as a setting — it's a
/// correctness floor, not a user preference (see backlog 0067 / learned 031).
///
/// macOS returns a configured [`super::macos::MacInjector`]. Windows and any
/// other unsupported target ignore the delay and return a stub injector that
/// fails every paste with [`crate::error::MuniError::PlatformUnsupported`].
///
/// Plan 039 task 48(a): the platform injector is wrapped in a
/// [`SerializedInjector`] so dictation delivery and the re-paste hotkey — which
/// share this one Arc — can never run their pasteboard critical sections
/// concurrently.
#[cfg(target_os = "macos")]
pub fn default_injector(paste_delay_ms: u64) -> Arc<dyn PlatformInjector> {
    let base: Arc<dyn PlatformInjector> = Arc::new(super::macos::MacInjector {
        paste_delay_ms: paste_delay_ms.min(super::macos::MAX_PASTE_DELAY_MS),
        ..Default::default()
    });
    Arc::new(SerializedInjector::new(base))
}

#[cfg(target_os = "windows")]
pub fn default_injector(_paste_delay_ms: u64) -> Arc<dyn PlatformInjector> {
    Arc::new(SerializedInjector::new(Arc::new(
        super::windows::WindowsInjector,
    )))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn default_injector(_paste_delay_ms: u64) -> Arc<dyn PlatformInjector> {
    Arc::new(SerializedInjector::new(Arc::new(UnsupportedInjector)))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
struct UnsupportedInjector;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[async_trait::async_trait]
impl PlatformInjector for UnsupportedInjector {
    async fn paste(&self, _text: &str) -> Result<(), crate::error::MuniError> {
        Err(crate::error::MuniError::PlatformUnsupported)
    }
}
