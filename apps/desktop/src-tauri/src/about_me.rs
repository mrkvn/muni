//! Feature 014 — "About Me" free-form vocabulary context.
//!
//! A single optional string the user authors in Settings → Cleanup that
//! describes their domain, projects, jargon, and proper-noun set. When
//! non-empty it is prepended to the Groq cleanup system prompt as a
//! clearly-labeled `## ABOUT THE USER` block so the model biases toward
//! the user's likely terms when disambiguating phonetic mishears.
//!
//! Empty string is the canonical "off" state — when empty the cleanup
//! prompt sent to Groq is byte-identical to the pre-feature behaviour.
//!
//! Storage mirrors the [`crate::my_words`] managed-state pattern: an
//! `Arc<AboutMe>` lives in Tauri-managed state, the inner string sits
//! behind a `parking_lot::RwLock`, and the hot path on every press
//! takes one read lock to clone the current snapshot.

use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::json;
use tauri::{AppHandle, Runtime};
use tauri_plugin_store::StoreExt;

use crate::settings::{KEY_CLEANUP_ABOUT_ME, SETTINGS_FILE};

/// Hard cap on the persisted About Me body. Sized to stay within roughly
/// 250 prompt tokens worst-case, keeping the per-press latency overhead
/// within the speed budget documented in feature 014's plan.
pub const MAX_LEN: usize = 1000;

/// Managed state — one per process. Cheap to `Arc`-clone; the body
/// lives behind an `RwLock<String>` for the read-heavy press path.
#[derive(Debug)]
pub struct AboutMe {
    state: RwLock<String>,
}

impl Default for AboutMe {
    fn default() -> Self {
        Self {
            state: RwLock::new(String::new()),
        }
    }
}

impl AboutMe {
    /// Test-only constructor producing an empty managed state.
    pub fn empty() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Direct constructor used by tests.
    #[cfg(test)]
    pub fn with_text(text: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            state: RwLock::new(text.into()),
        })
    }

    /// Load initial state from the Tauri store. Read failures are
    /// treated as `""` with a logged warning — About Me must never
    /// block the app from booting.
    pub fn from_app<R: Runtime>(app: &AppHandle<R>) -> Arc<Self> {
        let text = read_from_store(app);
        if !text.is_empty() {
            log::info!(target: "about_me", "loaded ({} chars)", text.len());
        }
        Arc::new(Self {
            state: RwLock::new(text),
        })
    }

    /// Snapshot the current body under a read lock. Cloning a `String`
    /// is cheap and frees the lock immediately so the press path never
    /// holds it across the Groq HTTP call.
    pub fn text(&self) -> String {
        self.state.read().clone()
    }

    /// Persist a new body. Trims trailing whitespace and enforces the
    /// 1000-char cap; the store write happens BEFORE the in-memory
    /// swap so a failed disk write doesn't leave the press path using
    /// a value the user can't see in the UI on next launch.
    pub fn save<R: Runtime>(&self, app: &AppHandle<R>, next: String) -> Result<(), String> {
        let sanitized = next.trim_end().to_string();
        if sanitized.chars().count() > MAX_LEN {
            return Err(format!(
                "about_me exceeds {MAX_LEN}-char cap ({} chars)",
                sanitized.chars().count()
            ));
        }

        let store = app
            .store(SETTINGS_FILE)
            .map_err(|e| format!("open store: {e}"))?;
        store.set(KEY_CLEANUP_ABOUT_ME, json!(sanitized));
        store.save().map_err(|e| format!("save store: {e}"))?;

        let n = sanitized.len();
        *self.state.write() = sanitized;
        if n > 0 {
            log::info!(target: "about_me", "saved ({n} chars)");
        } else {
            log::info!(target: "about_me", "saved (empty)");
        }
        Ok(())
    }
}

fn read_from_store<R: Runtime>(app: &AppHandle<R>) -> String {
    let store = match app.store(SETTINGS_FILE) {
        Ok(s) => s,
        Err(e) => {
            log::warn!(target: "about_me", "store open failed at boot: {e} — defaulting to empty");
            return String::new();
        }
    };
    store
        .get(KEY_CLEANUP_ABOUT_ME)
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_by_default() {
        let am = AboutMe::default();
        assert_eq!(am.text(), "");
    }

    #[test]
    fn with_text_constructor_round_trips() {
        let am = AboutMe::with_text("I work on Muni.");
        assert_eq!(am.text(), "I work on Muni.");
    }

    #[test]
    fn empty_helper_is_empty() {
        let am = AboutMe::empty();
        assert_eq!(am.text(), "");
    }

    #[test]
    fn max_len_is_one_thousand() {
        assert_eq!(MAX_LEN, 1000);
    }
}
