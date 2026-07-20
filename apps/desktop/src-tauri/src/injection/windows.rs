//! Windows text-injection stub.
//!
//! The real Windows port (using `SendInput` for the synthetic Ctrl+V plus the
//! Win32 clipboard APIs for snapshot/restore) is scheduled for Muni v2. The
//! stub keeps the rest of the codebase compiling on Windows while making the
//! "you can't paste yet" failure loud and typed instead of silently dropping
//! cleaned text.

use async_trait::async_trait;

use super::PlatformInjector;
use crate::error::MuniError;

/// Stub injector for Windows targets. Always returns
/// [`MuniError::PlatformUnsupported`].
pub struct WindowsInjector;

#[async_trait]
impl PlatformInjector for WindowsInjector {
    async fn paste(&self, _text: &str) -> Result<(), MuniError> {
        Err(MuniError::PlatformUnsupported)
    }
}
