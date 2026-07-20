//! Platform-agnostic text injection.
//!
//! The injector is the last stage of the dictation pipeline: cleaned-up text
//! lands here and is delivered into whichever app is currently focused. The
//! current macOS strategy (mirrored from Swift v1's `TextInjector`) is to
//! snapshot the user's pasteboard, write our text, post a synthetic Cmd+V,
//! sleep long enough for the receiving app to consume the paste, then restore
//! the original pasteboard contents.
//!
//! Phase 5 ships the macOS implementation. Windows is a stub (per plan §002
//! R7) so the rest of the app compiles cross-platform; the Windows port lands
//! in a future Muni v2 milestone alongside the equivalent `SendInput` +
//! clipboard work in `injection/windows.rs`.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::error::MuniError;

mod factory;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
mod windows;

pub use factory::default_injector;

/// Plan 039 task 48(a) — decorator that serializes **every** `paste()` against a
/// single async mutex so the dictation-delivery path and the re-paste hotkey can
/// never interleave their snapshot→write→⌘V→restore critical sections.
///
/// Both call sites (`session`'s `deliver_final` and `lib::spawn_repaste`) share
/// the one `Arc<dyn PlatformInjector>` constructed at boot, so wrapping that Arc
/// here makes the lock process-wide for pastes. Two concurrent pastes would
/// otherwise race the pasteboard: paste A snapshots the user's clipboard, writes
/// its text and posts ⌘V; before A restores, paste B snapshots (capturing A's
/// text as the "original"), writes, pastes, and restores — leaving A's text on
/// the clipboard and losing the user's original. Serializing makes each paste's
/// restore land before the next paste's snapshot (learned/032's restore-window
/// math is preserved per paste).
///
/// The lock wraps only the inner `paste()` — the exact critical section — and is
/// never held across `press_enter`/`has_editable_focus` (those delegate
/// straight through), honoring "do NOT hold it across unrelated awaits".
pub struct SerializedInjector {
    inner: Arc<dyn PlatformInjector>,
    /// One paste at a time. An async mutex (not `std::sync`) because the guard is
    /// held across the inner `paste()` await.
    gate: Mutex<()>,
}

impl SerializedInjector {
    /// Wrap an injector so all its pastes serialize through one gate.
    pub fn new(inner: Arc<dyn PlatformInjector>) -> Self {
        Self {
            inner,
            gate: Mutex::new(()),
        }
    }
}

#[async_trait]
impl PlatformInjector for SerializedInjector {
    async fn paste(&self, text: &str) -> Result<(), MuniError> {
        // Hold the gate for the whole inner paste (snapshot→write→⌘V→restore).
        let _guard = self.gate.lock().await;
        self.inner.paste(text).await
    }

    async fn press_enter(&self) -> Result<(), MuniError> {
        // Not a pasteboard operation and only ever invoked after a paste has
        // already returned — delegate without taking the paste gate.
        self.inner.press_enter().await
    }

    async fn has_editable_focus(&self) -> FocusProbe {
        self.inner.has_editable_focus().await
    }
}

/// Result of asking the OS whether an editable field currently has focus
/// (Feature 037). Consumed at the delivery decision point so a dictation that
/// would otherwise fire a blind Cmd+V into nothing can instead be held for the
/// re-paste hotkey.
///
/// The three-way shape exists to make the conservative bias structural: the
/// caller may skip a paste **only** on [`FocusProbe::NoEditableField`]. Every
/// ambiguous outcome collapses to [`FocusProbe::Unknown`], which pastes exactly
/// as Muni does today — a wrong "no field" would swallow a real paste, the
/// worst possible failure, so doubt always biases toward pasting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusProbe {
    /// An editable text target is focused — paste as normal.
    Editable,
    /// The OS positively reports that no editable field is focused. The **only**
    /// value that authorises skipping the automatic paste.
    NoEditableField,
    /// The focus state couldn't be determined confidently (permission denied,
    /// an AX error/timeout, an unreadable attribute, or no platform support).
    /// Treated exactly like [`FocusProbe::Editable`] at the call site.
    Unknown,
}

/// Strategy contract for delivering cleaned text into the focused app.
///
/// Implementors are expected to handle their own pre-flight checks (permission
/// probes, secure-input detection) and surface them as typed [`MuniError`]
/// variants — callers just see "did the paste work or not?". Implementors are
/// also responsible for any pasteboard / clipboard restoration.
#[async_trait]
pub trait PlatformInjector: Send + Sync {
    async fn paste(&self, text: &str) -> Result<(), MuniError>;

    /// Inject a synthetic Return/Enter keypress into the focused app.
    ///
    /// Used by the "press Enter to finish" toggle gesture: after the
    /// cleaned text is pasted, this submits it (e.g. sends the message in
    /// a chat app) so the user doesn't have to reach for Enter themselves.
    /// Called only when the press finished via Enter, never on a re-tap.
    ///
    /// The default is [`MuniError::PlatformUnsupported`] so stub/test
    /// backends don't have to implement it; only the active platform
    /// backend overrides it with a real keystroke.
    async fn press_enter(&self) -> Result<(), MuniError> {
        Err(MuniError::PlatformUnsupported)
    }

    /// Ask the OS whether an editable field is currently focused (Feature 037).
    ///
    /// Called once at the delivery decision point, just before the automatic
    /// paste. The default returns [`FocusProbe::Unknown`] so stub/test backends
    /// (and the Windows port) always fall through to the existing paste path —
    /// only the macOS backend overrides this with a real Accessibility probe.
    ///
    /// Implementors MUST return [`FocusProbe::NoEditableField`] only on a
    /// confident negative; any doubt maps to [`FocusProbe::Unknown`].
    async fn has_editable_focus(&self) -> FocusProbe {
        FocusProbe::Unknown
    }
}
