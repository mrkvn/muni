//! User-authored "preferences" prompt for the cleanup stage.
//!
//! Distinct from [`crate::about_me`] (vocabulary context) — this is the
//! user's free-form instruction block describing how they want their
//! dictation cleaned up. When non-empty it is appended **after** the
//! bundled cleanup body inside a labeled `## USER PREFERENCES` block
//! that explicitly tells the model to follow these instructions when
//! they conflict with rules above.
//!
//! Empty string is the canonical "off" state — produces a byte-identical
//! prompt to the pre-feature behaviour so cached presses stay fast.
//!
//! Storage mirrors [`crate::about_me`]: an `Arc<UserPrompt>` lives in
//! Tauri-managed state with the inner string behind a
//! `parking_lot::RwLock`; the press path takes one read-lock to clone
//! the current snapshot.

use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::json;
use tauri::{AppHandle, Runtime};
use tauri_plugin_store::StoreExt;

use crate::settings::{KEY_CLEANUP_USER_PROMPT, SETTINGS_FILE};

/// Hard cap on the persisted user-prompt body. Matches
/// [`crate::about_me::MAX_LEN`] so token-budget intuition transfers.
pub const MAX_LEN: usize = 1000;

#[derive(Debug)]
pub struct UserPrompt {
    state: RwLock<String>,
}

impl Default for UserPrompt {
    fn default() -> Self {
        Self {
            state: RwLock::new(String::new()),
        }
    }
}

impl UserPrompt {
    pub fn empty() -> Arc<Self> {
        Arc::new(Self::default())
    }

    #[cfg(test)]
    pub fn with_text(text: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            state: RwLock::new(text.into()),
        })
    }

    pub fn from_app<R: Runtime>(app: &AppHandle<R>) -> Arc<Self> {
        let text = read_from_store(app);
        if !text.is_empty() {
            log::info!(target: "user_prompt", "loaded ({} chars)", text.len());
        }
        Arc::new(Self {
            state: RwLock::new(text),
        })
    }

    pub fn text(&self) -> String {
        self.state.read().clone()
    }

    pub fn save<R: Runtime>(&self, app: &AppHandle<R>, next: String) -> Result<(), String> {
        let sanitized = next.trim_end().to_string();
        if sanitized.chars().count() > MAX_LEN {
            return Err(format!(
                "user_prompt exceeds {MAX_LEN}-char cap ({} chars)",
                sanitized.chars().count()
            ));
        }

        let store = app
            .store(SETTINGS_FILE)
            .map_err(|e| format!("open store: {e}"))?;
        store.set(KEY_CLEANUP_USER_PROMPT, json!(sanitized));
        store.save().map_err(|e| format!("save store: {e}"))?;

        let n = sanitized.len();
        *self.state.write() = sanitized;
        if n > 0 {
            log::info!(target: "user_prompt", "saved ({n} chars)");
        } else {
            log::info!(target: "user_prompt", "saved (empty)");
        }
        Ok(())
    }
}

fn read_from_store<R: Runtime>(app: &AppHandle<R>) -> String {
    let store = match app.store(SETTINGS_FILE) {
        Ok(s) => s,
        Err(e) => {
            log::warn!(target: "user_prompt", "store open failed at boot: {e} — defaulting to empty");
            return String::new();
        }
    };
    store
        .get(KEY_CLEANUP_USER_PROMPT)
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_by_default() {
        let up = UserPrompt::default();
        assert_eq!(up.text(), "");
    }

    #[test]
    fn with_text_constructor_round_trips() {
        let up = UserPrompt::with_text("Always be terse.");
        assert_eq!(up.text(), "Always be terse.");
    }

    #[test]
    fn empty_helper_is_empty() {
        let up = UserPrompt::empty();
        assert_eq!(up.text(), "");
    }

    #[test]
    fn max_len_is_one_thousand() {
        assert_eq!(MAX_LEN, 1000);
    }
}
