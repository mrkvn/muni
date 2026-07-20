//! Serde binding model for Muni's configurable hotkeys (Feature 037).
//!
//! This module owns the *data* shape of the two user-configurable hotkeys
//! and the pure logic around them — validation, display labels, the
//! `CGEventFlags` mask the flags-tap matches against, and the
//! `tauri-plugin-global-shortcut` accelerator string the re-paste shortcut
//! registers with. It deliberately holds **no** runtime state; the live tap
//! state and shortcut registration live in `hotkey.rs`/`lib.rs`.
//!
//! ## Portability
//!
//! `core-graphics` only compiles on macOS (see the target-gated dependency in
//! `Cargo.toml`), so the `CGEventFlags` conversions are gated behind
//! `#[cfg(target_os = "macos")]`. Everything the frontend contract depends on
//! — serde shapes, `validate`, `label`, `accelerator` — stays portable so the
//! type round-trips and unit tests build on every host.
//!
//! ## JSON shapes (stored verbatim in `settings.json`)
//!
//! ```jsonc
//! // hotkey.dictation_binding — modifier-only (no "key" member):
//! { "kind": "modifiers", "mods": ["control", "option"] }
//!
//! // hotkey.dictation_binding — keyed (modifier(s)+key, push-to-talk):
//! { "kind": "modifiers", "mods": ["control", "shift"], "key": "KeyR" }
//!
//! // hotkey.repaste_binding — key+modifier, or null when disabled:
//! { "mods": ["control", "command"], "key": "KeyV" }
//! ```

use std::fmt;

use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
use core_graphics::event::CGEventFlags;

/// Minimum number of modifiers a modifier-only dictation binding requires.
/// Fewer than two invites accidental triggers from a single common modifier.
pub const MIN_DICTATION_MODIFIERS: usize = 2;

/// One of the five macOS modifier keys Muni can bind.
///
/// Serialised as its lowercase name (`"control"`, `"option"`, …) to match the
/// tokens the frontend recorder emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HotkeyModifier {
    Control,
    Option,
    Command,
    Shift,
    Fn,
}

impl HotkeyModifier {
    /// Device-independent `CGEventFlags` bit for this modifier — the value the
    /// flags-tap ANDs against the live modifier state.
    #[cfg(target_os = "macos")]
    pub fn to_cg_flag(self) -> CGEventFlags {
        match self {
            HotkeyModifier::Control => CGEventFlags::CGEventFlagControl,
            HotkeyModifier::Option => CGEventFlags::CGEventFlagAlternate,
            HotkeyModifier::Command => CGEventFlags::CGEventFlagCommand,
            HotkeyModifier::Shift => CGEventFlags::CGEventFlagShift,
            HotkeyModifier::Fn => CGEventFlags::CGEventFlagSecondaryFn,
        }
    }

    /// Glyph shown in the UI (`⌃⌥⌘⇧`, or the literal `fn`).
    pub fn to_symbol(self) -> &'static str {
        match self {
            HotkeyModifier::Control => "⌃",
            HotkeyModifier::Option => "⌥",
            HotkeyModifier::Command => "⌘",
            HotkeyModifier::Shift => "⇧",
            HotkeyModifier::Fn => "fn",
        }
    }

    /// Token accepted by the `tauri-plugin-global-shortcut` accelerator parser
    /// (the `global_hotkey` grammar recognises `Control`/`Option`/`Command`/
    /// `Shift`). `fn` has no accelerator token — emitting a token the parser
    /// rejects means a stray Fn re-paste binding fails to register (a WARN,
    /// non-fatal) rather than silently binding the wrong combo.
    fn to_accelerator_token(self) -> &'static str {
        match self {
            HotkeyModifier::Control => "Control",
            HotkeyModifier::Option => "Option",
            HotkeyModifier::Command => "Command",
            HotkeyModifier::Shift => "Shift",
            HotkeyModifier::Fn => "Fn",
        }
    }

    /// Canonical display/accelerator rank. Modifiers are rendered in the Apple
    /// convention — `fn ⌃ ⌥ ⇧ ⌘` (Command always last) — regardless of the order
    /// the user pressed them, so `[Command, Control]` still shows `⌃⌘`.
    fn order_rank(self) -> u8 {
        match self {
            HotkeyModifier::Fn => 0,
            HotkeyModifier::Control => 1,
            HotkeyModifier::Option => 2,
            HotkeyModifier::Shift => 3,
            HotkeyModifier::Command => 4,
        }
    }
}

/// Copy of `mods` sorted into canonical display order ([`HotkeyModifier::order_rank`]).
fn ordered_mods(mods: &[HotkeyModifier]) -> Vec<HotkeyModifier> {
    let mut ordered = mods.to_vec();
    ordered.sort_by_key(|m| m.order_rank());
    ordered
}

/// Accelerator string in the `tauri-plugin-global-shortcut` grammar
/// (`"Control+Shift+KeyR"`): modifier tokens in canonical order, then the
/// `Code` key, joined by `+`. Shared by both bindings so keyed dictation and
/// re-paste emit the identical shape the plugin parser accepts.
fn accelerator_of(mods: &[HotkeyModifier], key: &str) -> String {
    let mut parts: Vec<&str> = ordered_mods(mods)
        .iter()
        .map(|m| m.to_accelerator_token())
        .collect();
    parts.push(key);
    parts.join("+")
}

/// The main dictation trigger. Two shapes, both under the stable
/// `{"kind":"modifiers", …}` serde tag:
///
/// - **modifier-only** (`key` absent): a ≥2-modifier chord matched by the
///   flags-tap for hold-to-talk / tap-to-toggle.
/// - **keyed** (`key` present): a modifier(s)+key combo registered as a
///   consuming global shortcut, whose Pressed/Released edges drive the same
///   hold/tap/toggle state machine.
///
/// A single-variant enum keeps the serde shape stable for stored settings + the
/// TS contract. The `key` field uses `default` (so legacy modifier-only JSON
/// with no `key` loads as `None`) + `skip_serializing_if` (so a modifier-only
/// binding resaves byte-for-byte, with no gratuitous `"key":null`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DictationBinding {
    Modifiers {
        mods: Vec<HotkeyModifier>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        /// Plan 039 task 54 — the `KeyboardEvent.key` captured at record time,
        /// a layout-aware display hint for a keyed binding (e.g. `"a"` on
        /// AZERTY where the physical `key` code is `"KeyQ"`). Matching is
        /// always by the physical `key` code above; this field only feeds
        /// [`DictationBinding::label`]. `default` so legacy stored bindings
        /// (pre-054) with no display hint deserialize to `None`;
        /// `skip_serializing_if` keeps a binding with no hint resaving
        /// byte-for-byte.
        #[serde(
            rename = "displayKey",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        display_key: Option<String>,
    },
}

impl DictationBinding {
    /// The shipped default — `Ctrl+Option`, matching the pre-037 hardcoded
    /// binding so existing muscle memory is preserved on upgrade.
    pub fn default_dictation() -> Self {
        DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Option],
            key: None,
            display_key: None,
        }
    }

    /// The bound key (`KeyboardEvent.code`, e.g. `"KeyR"`) for a keyed binding,
    /// or `None` for a modifier-only chord.
    pub fn key(&self) -> Option<&str> {
        let DictationBinding::Modifiers { key, .. } = self;
        key.as_deref()
    }

    /// Accelerator string in the `tauri-plugin-global-shortcut` grammar
    /// (`"Control+Shift+KeyR"`) — `Some` only for a keyed binding, `None` for a
    /// modifier-only chord (which is driven by the flags-tap, not a shortcut).
    pub fn accelerator(&self) -> Option<String> {
        let DictationBinding::Modifiers { mods, key, .. } = self;
        key.as_ref().map(|k| accelerator_of(mods, k))
    }

    /// Reject structurally invalid bindings before they reach the tap.
    pub fn validate(&self) -> Result<(), BindingError> {
        let DictationBinding::Modifiers { mods, key, .. } = self;
        // A bare key with no modifier would fire on every keystroke.
        if mods.is_empty() && key.is_some() {
            return Err(BindingError::MissingModifier);
        }
        // Modifier-only chords need a ≥2-modifier floor to avoid accidental
        // triggers from a single common modifier; a keyed binding is anchored by
        // its key, so one modifier suffices. Count the *distinct* set, not the
        // list length: a hand-edited store carrying `["shift","shift"]` is
        // effectively Shift-only and must not clear the floor on duplicate count.
        if distinct_mod_count(mods) < MIN_DICTATION_MODIFIERS && key.is_none() {
            return Err(BindingError::TooFewModifiers);
        }
        // Keyed bindings carry the same self-injected-combo + modifier-floor
        // rules as re-paste (plan 039 tasks 50b/50c) — the key anchors them, but
        // a lone Shift/Option would still swallow typing and ⌘V is ours.
        if let Some(key) = key {
            // Task 53a — reject a key outside the anchor allowlist (CapsLock,
            // NumLock, media keys, IME codes, …) before the softer checks below.
            if !is_allowed_anchor_key(key) {
                return Err(BindingError::InvalidAnchorKey);
            }
            if is_reserved_paste_combo(mods, key) {
                return Err(BindingError::ReservedForPaste);
            }
            if !keyed_modifier_floor_ok(mods) {
                return Err(BindingError::WeakModifier);
            }
        }
        Ok(())
    }

    /// OR of the modifier flag bits — the mask the tap tests with
    /// `(live_flags & mask) == mask` (subset semantics; transient bits such as
    /// caps-lock / numeric-pad may also be set — see learned note 025).
    #[cfg(target_os = "macos")]
    pub fn required_flags_mask(&self) -> u64 {
        let DictationBinding::Modifiers { mods, .. } = self;
        mods.iter()
            .fold(CGEventFlags::empty(), |acc, m| acc | m.to_cg_flag())
            .bits()
    }

    /// Human-readable label for the Settings UI (`⌃⌥`, or `⌃⇧R` when keyed).
    ///
    /// Plan 039 task 54: when `display_key` is present it wins over the
    /// physical `key` code — a layout-aware glyph (e.g. `A` on AZERTY where
    /// `key` is the physical `"KeyQ"`) instead of the QWERTY-shaped
    /// [`key_symbol`] fallback.
    pub fn label(&self) -> String {
        let DictationBinding::Modifiers {
            mods,
            key,
            display_key,
        } = self;
        let mut label = symbols(mods);
        if let Some(key) = key {
            label.push_str(&display_key_symbol(key, display_key.as_deref()));
        }
        label
    }
}

/// The re-paste hotkey: a modifier(s)+key combo registered as a global
/// shortcut. `Option::None` at the call sites means "disabled" (cleared).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepasteBinding {
    pub mods: Vec<HotkeyModifier>,
    /// A `KeyboardEvent.code` string (`"KeyV"`, `"F13"`) — matches the token
    /// the frontend recorder captures and the accelerator parser accepts.
    pub key: String,
    /// Plan 039 task 54 — see [`DictationBinding::Modifiers::display_key`]
    /// (the re-paste binding is always keyed, so this is populated whenever
    /// the recorder captured the combo post-054).
    #[serde(
        rename = "displayKey",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub display_key: Option<String>,
}

impl RepasteBinding {
    /// The shipped default — `Ctrl+Cmd+V` (avoids the common `Cmd+Shift+V`
    /// "paste and match style" collision; brainstorm 014 Q3).
    pub fn default_repaste() -> Self {
        RepasteBinding {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Command],
            key: "KeyV".to_string(),
            display_key: None,
        }
    }

    /// A re-paste combo needs at least one modifier (a bare key would fire on
    /// every keystroke) and a non-empty key.
    pub fn validate(&self) -> Result<(), BindingError> {
        if self.mods.is_empty() {
            return Err(BindingError::MissingModifier);
        }
        if self.key.trim().is_empty() {
            return Err(BindingError::MissingKey);
        }
        // Task 53a — reject a key outside the anchor allowlist (CapsLock,
        // NumLock, media keys, IME codes, …) before the softer checks below.
        if !is_allowed_anchor_key(&self.key) {
            return Err(BindingError::InvalidAnchorKey);
        }
        // Never bind a combo Muni injects itself (⌘V / ⌘⇧V) — it would fire the
        // re-paste's own synthetic paste (plan 039 task 50b).
        if is_reserved_paste_combo(&self.mods, &self.key) {
            return Err(BindingError::ReservedForPaste);
        }
        // Shift/Option alone swallow typing / accented input (task 50c).
        if !keyed_modifier_floor_ok(&self.mods) {
            return Err(BindingError::WeakModifier);
        }
        Ok(())
    }

    /// Accelerator string in the `tauri-plugin-global-shortcut` grammar
    /// (`"Control+Command+KeyV"`) — modifier tokens then the `Code` key,
    /// joined by `+`.
    pub fn accelerator(&self) -> String {
        accelerator_of(&self.mods, &self.key)
    }

    /// Human-readable label for the Settings UI and the HUD notice (`⌃⌘V`).
    /// Prefers `display_key` when present (plan 039 task 54) — see
    /// [`DictationBinding::label`].
    pub fn label(&self) -> String {
        let mut label = symbols(&self.mods);
        label.push_str(&display_key_symbol(&self.key, self.display_key.as_deref()));
        label
    }
}

/// A binding-validation failure, surfaced to the frontend as a toast via
/// `Display`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingError {
    /// Modifier-only dictation binding with fewer than two modifiers (and no
    /// key to anchor it).
    TooFewModifiers,
    /// Re-paste binding with no key.
    MissingKey,
    /// Re-paste binding with no modifier, or a keyed dictation binding with a
    /// key but no modifier.
    MissingModifier,
    /// The dictation and re-paste bindings share the same chord.
    Conflict,
    /// A keyed binding that Muni injects itself (⌘V / ⌘⇧V) — binding it would
    /// fire Muni's own synthetic paste (plan 039 task 50b).
    ReservedForPaste,
    /// A keyed binding whose only modifier is Shift or Option — those alone
    /// swallow ordinary typing / accented input, so a keyed binding needs a
    /// hard modifier (Control/Command/Fn) or two modifiers (task 50c).
    WeakModifier,
    /// A re-paste chord whose modifier set is a strict superset of a
    /// modifier-only dictation chord: the subset flags-tap would trip dictation
    /// on every re-paste press (task 50a).
    SupersetTripsDictation,
    /// A keyed binding's anchor key falls outside the allowlist (plan 039 task
    /// 53a) — CapsLock/NumLock/ScrollLock/ContextMenu, a media key, or an IME
    /// composition code. None of those are safe anchors for a global shortcut.
    InvalidAnchorKey,
}

impl fmt::Display for BindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            BindingError::TooFewModifiers => "Use two modifiers, or a modifier and a key.",
            BindingError::MissingKey => "Add a key to complete the shortcut.",
            BindingError::MissingModifier => "Use at least one modifier key.",
            BindingError::Conflict => {
                "The dictation and re-paste shortcuts can't use the same keys."
            }
            BindingError::ReservedForPaste => "Muni uses this to paste.",
            BindingError::WeakModifier => "Add Control, Command, or Fn, or a second modifier.",
            BindingError::SupersetTripsDictation => {
                "These modifiers would always trigger dictation. Use a different combo."
            }
            BindingError::InvalidAnchorKey => "That key can't be used.",
        };
        f.write_str(message)
    }
}

impl std::error::Error for BindingError {}

/// Validate both bindings together, rejecting a chord collision.
///
/// Beyond each binding's own `validate`, this guards the one cross-binding
/// rule (brainstorm 014 Q11): the two hotkeys must not use the same modifier
/// chord. Because the dictation flags-tap fires on a *subset* match
/// (`(live_flags & mask) == mask`), a re-paste whose modifier set is exactly
/// the dictation modifier set would always trip dictation first — so we reject
/// that overlap. `repaste == None` (disabled) can't collide.
pub fn validate_pair(
    dictation: &DictationBinding,
    repaste: Option<&RepasteBinding>,
) -> Result<(), BindingError> {
    dictation.validate()?;
    let Some(repaste) = repaste else {
        return Ok(());
    };
    repaste.validate()?;

    // Keyed dictation and re-paste are both consuming global shortcuts, so they
    // only collide when their *full* accelerators are identical — different keys
    // coexist fine.
    match dictation.accelerator() {
        Some(accel) => {
            if accel == repaste.accelerator() {
                return Err(BindingError::Conflict);
            }
        }
        // Modifier-only dictation fires on a *subset* flag match, so it collides
        // with a re-paste that shares its exact modifier set (Conflict), and it
        // is also tripped by any re-paste whose modifier set is a strict
        // *superset* — pressing the re-paste satisfies the dictation mask on its
        // way (plan 039 task 50a).
        None => {
            let DictationBinding::Modifiers { mods, .. } = dictation;
            if modifiers_set_eq(mods, &repaste.mods) {
                return Err(BindingError::Conflict);
            }
            if modifiers_is_subset(mods, &repaste.mods) {
                return Err(BindingError::SupersetTripsDictation);
            }
        }
    }
    Ok(())
}

/// True when every modifier in `sub` is also present in `sup` (set subset,
/// order- and duplicate-independent).
fn modifiers_is_subset(sub: &[HotkeyModifier], sup: &[HotkeyModifier]) -> bool {
    sub.iter().all(|m| sup.contains(m))
}

/// True when two modifier lists denote the same set.
fn modifiers_set_eq(a: &[HotkeyModifier], b: &[HotkeyModifier]) -> bool {
    modifiers_is_subset(a, b) && modifiers_is_subset(b, a)
}

/// Combos Muni injects itself and therefore refuses to bind (plan 039 task 50b):
/// the synthetic paste is ⌘V and macOS "paste and match style" is ⌘⇧V. Only
/// `KeyV` with exactly `{Command}` or `{Command, Shift}` is reserved — the
/// default re-paste ⌃⌘V (adds Control) stays bindable.
fn is_reserved_paste_combo(mods: &[HotkeyModifier], key: &str) -> bool {
    if key != "KeyV" {
        return false;
    }
    modifiers_set_eq(mods, &[HotkeyModifier::Command])
        || modifiers_set_eq(mods, &[HotkeyModifier::Command, HotkeyModifier::Shift])
}

/// Number of *distinct* modifiers in a list. A hand-edited settings store can
/// carry duplicates (`["shift","shift"]`) the recorder can never produce; a
/// length-based floor would let such an effectively single-modifier chord
/// through, so every modifier-count floor counts the set, not the list.
fn distinct_mod_count(mods: &[HotkeyModifier]) -> usize {
    let mut seen: Vec<HotkeyModifier> = Vec::with_capacity(mods.len());
    for m in mods {
        if !seen.contains(m) {
            seen.push(*m);
        }
    }
    seen.len()
}

/// Modifier floor for a keyed binding (plan 039 task 50c): Shift or Option
/// *alone* would swallow ordinary typing / accented input, so a keyed binding
/// needs at least one hard modifier (Control, Command, or Fn) OR two *distinct*
/// modifiers. (Callers guarantee `mods` is non-empty.)
fn keyed_modifier_floor_ok(mods: &[HotkeyModifier]) -> bool {
    let has_hard = mods.iter().any(|m| {
        matches!(
            m,
            HotkeyModifier::Control | HotkeyModifier::Command | HotkeyModifier::Fn
        )
    });
    has_hard || distinct_mod_count(mods) >= MIN_DICTATION_MODIFIERS
}

/// Concatenated glyphs for a modifier list in canonical order
/// (`[Command, Control]` → `"⌃⌘"`).
fn symbols(mods: &[HotkeyModifier]) -> String {
    ordered_mods(mods).iter().map(|m| m.to_symbol()).collect()
}

/// Display glyph for a `KeyboardEvent.code` key: `"KeyV"` → `"V"`,
/// `"Digit1"` → `"1"`, otherwise the code verbatim (`"F13"`, `"Space"`).
fn key_symbol(code: &str) -> String {
    if let Some(rest) = code.strip_prefix("Key") {
        return rest.to_string();
    }
    if let Some(rest) = code.strip_prefix("Digit") {
        return rest.to_string();
    }
    code.to_string()
}

/// Anchor-key allowlist for a keyed binding (plan 039 task 53a): letters,
/// digits, F-keys, navigation, punctuation, and Space/Tab/Enter as a
/// deliberate whitespace/control choice. Everything else — CapsLock, NumLock,
/// ScrollLock, ContextMenu, media keys, IME composition codes — is rejected
/// with [`BindingError::InvalidAnchorKey`]. Mirrors `isAllowedAnchorKey` in
/// `hotkeyCapture.ts` byte-for-byte; keep the two lists in lockstep.
fn is_allowed_anchor_key(code: &str) -> bool {
    if let Some(letter) = code.strip_prefix("Key") {
        return letter.len() == 1
            && letter
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase());
    }
    if let Some(digit) = code.strip_prefix("Digit") {
        return digit.len() == 1 && digit.chars().next().is_some_and(|c| c.is_ascii_digit());
    }
    if let Some(n) = code.strip_prefix('F') {
        return n.parse::<u8>().is_ok_and(|n| (1..=24).contains(&n));
    }
    matches!(
        code,
        "ArrowUp"
            | "ArrowDown"
            | "ArrowLeft"
            | "ArrowRight"
            | "Home"
            | "End"
            | "PageUp"
            | "PageDown"
            | "Insert"
            | "Delete"
            | "Backquote"
            | "Minus"
            | "Equal"
            | "BracketLeft"
            | "BracketRight"
            | "Backslash"
            | "Semicolon"
            | "Quote"
            | "Comma"
            | "Period"
            | "Slash"
            | "IntlBackslash"
            | "Space"
            | "Tab"
            | "Enter"
    )
}

/// Display glyph for a keyed binding's key (plan 039 task 54): prefers the
/// layout-aware `display_key` (the `KeyboardEvent.key` captured at record
/// time) over the physical-code-derived [`key_symbol`] fallback used for
/// bindings stored before this field existed. A single-character display key
/// is uppercased for a consistent chip glyph (`"a"` → `"A"`); a multi-character
/// one (`"Enter"`, `"ArrowUp"`) is already presentable and passed through. A
/// blank (whitespace-only) display key — e.g. `" "` for the Space anchor,
/// whose `KeyboardEvent.key` is a literal space — renders nothing useful, so
/// it falls back to the code glyph the same as a missing one.
fn display_key_symbol(code: &str, display_key: Option<&str>) -> String {
    match display_key {
        Some(key) if key.trim().is_empty() => key_symbol(code),
        Some(key) if key.chars().count() == 1 => key.to_uppercase(),
        Some(key) => key.to_string(),
        None => key_symbol(code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- validation -------------------------------------------------------

    #[test]
    fn dictation_modifiers_requires_at_least_two() {
        let one = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control],
            key: None,
            display_key: None,
        };
        assert_eq!(one.validate(), Err(BindingError::TooFewModifiers));

        let two = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Option],
            key: None,
            display_key: None,
        };
        assert_eq!(two.validate(), Ok(()));
    }

    #[test]
    fn dictation_shift_inclusive_modifier_chord_is_now_allowed() {
        // A Shift-inclusive modifier-only chord used to be rejected. It is now
        // allowed — an intentional footgun (Shift held while typing could trip
        // it); the safe alternative is a Ctrl+Shift+key keyed binding.
        let shift_chord = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Shift],
            key: None,
            display_key: None,
        };
        assert_eq!(shift_chord.validate(), Ok(()));

        // Shift on a re-paste binding is fine — the key anchors it.
        let shift_repaste = RepasteBinding {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Shift],
            key: "KeyR".to_string(),
            display_key: None,
        };
        assert_eq!(shift_repaste.validate(), Ok(()));
    }

    #[test]
    fn dictation_keyed_validates_with_one_modifier_and_key() {
        // A key anchors the binding, so one modifier is enough.
        let keyed = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control],
            key: Some("KeyR".to_string()),
            display_key: None,
        };
        assert_eq!(keyed.validate(), Ok(()));
    }

    #[test]
    fn dictation_bare_key_is_rejected() {
        // A key with no modifier would fire on every keystroke.
        let bare = DictationBinding::Modifiers {
            mods: vec![],
            key: Some("KeyR".to_string()),
            display_key: None,
        };
        assert_eq!(bare.validate(), Err(BindingError::MissingModifier));
    }

    #[test]
    fn repaste_rejects_empty_key_and_zero_modifiers() {
        let empty_key = RepasteBinding {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Command],
            key: "   ".to_string(),
            display_key: None,
        };
        assert_eq!(empty_key.validate(), Err(BindingError::MissingKey));

        let no_mods = RepasteBinding {
            mods: vec![],
            key: "KeyV".to_string(),
            display_key: None,
        };
        assert_eq!(no_mods.validate(), Err(BindingError::MissingModifier));

        assert_eq!(RepasteBinding::default_repaste().validate(), Ok(()));
    }

    // --- flag mask (macOS-only: core-graphics is target-gated) ------------

    #[cfg(target_os = "macos")]
    #[test]
    fn required_flags_mask_ors_the_modifier_bits() {
        let binding = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Option],
            key: None,
            display_key: None,
        };
        let expected =
            (CGEventFlags::CGEventFlagControl | CGEventFlags::CGEventFlagAlternate).bits();
        assert_eq!(binding.required_flags_mask(), expected);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn required_flags_mask_uses_subset_semantics() {
        // The tap matches with `(live & mask) == mask`, tolerating transient
        // bits (caps-lock, numeric-pad) that may also be latched.
        let binding = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Option],
            key: None,
            display_key: None,
        };
        let mask = binding.required_flags_mask();

        let live_exact = mask;
        let live_with_transient = mask | CGEventFlags::CGEventFlagNumericPad.bits();
        let live_missing = CGEventFlags::CGEventFlagControl.bits();

        assert_eq!(live_exact & mask, mask);
        assert_eq!(live_with_transient & mask, mask);
        assert_ne!(live_missing & mask, mask);
    }

    // --- accelerator ------------------------------------------------------

    #[test]
    fn accelerator_matches_plugin_grammar() {
        assert_eq!(
            RepasteBinding::default_repaste().accelerator(),
            "Control+Command+KeyV"
        );
    }

    #[test]
    fn accelerator_round_trips_through_the_plugin_parser() {
        use std::str::FromStr;
        use tauri_plugin_global_shortcut::Shortcut;

        let accelerator = RepasteBinding::default_repaste().accelerator();
        // The plugin's `Shortcut` is `global_hotkey::hotkey::HotKey`; if this
        // parses, the runtime `register(accel)` will accept the same string.
        assert!(
            Shortcut::from_str(&accelerator).is_ok(),
            "accelerator '{accelerator}' must parse via the plugin"
        );
    }

    // --- labels -----------------------------------------------------------

    #[test]
    fn labels_render_expected_glyphs() {
        let dictation = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Option],
            key: None,
            display_key: None,
        };
        assert_eq!(dictation.label(), "⌃⌥");

        assert_eq!(RepasteBinding::default_repaste().label(), "⌃⌘V");
    }

    #[test]
    fn keyed_dictation_label_and_accelerator() {
        let keyed = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Shift],
            key: Some("KeyR".to_string()),
            display_key: None,
        };
        assert_eq!(keyed.label(), "⌃⇧R");
        assert_eq!(keyed.accelerator(), Some("Control+Shift+KeyR".to_string()));
    }

    #[test]
    fn modifier_only_dictation_has_no_accelerator() {
        let modifier_only = DictationBinding::default_dictation();
        assert_eq!(modifier_only.key(), None);
        assert_eq!(modifier_only.accelerator(), None);
    }

    #[test]
    fn labels_and_accelerators_render_in_canonical_order() {
        // Whatever order the user pressed them, modifiers render fn ⌃ ⌥ ⇧ ⌘
        // (Command last) — not press order.
        let dictation = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Command, HotkeyModifier::Control],
            key: None,
            display_key: None,
        };
        assert_eq!(dictation.label(), "⌃⌘");

        let repaste = RepasteBinding {
            mods: vec![
                HotkeyModifier::Command,
                HotkeyModifier::Shift,
                HotkeyModifier::Control,
            ],
            key: "KeyR".to_string(),
            display_key: None,
        };
        assert_eq!(repaste.label(), "⌃⇧⌘R");
        assert_eq!(repaste.accelerator(), "Control+Shift+Command+KeyR");
    }

    // --- pair collision ---------------------------------------------------

    #[test]
    fn validate_pair_rejects_identical_chord() {
        let dictation = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Command],
            key: None,
            display_key: None,
        };
        let repaste = RepasteBinding {
            mods: vec![HotkeyModifier::Command, HotkeyModifier::Control],
            key: "KeyV".to_string(),
            display_key: None,
        };
        assert_eq!(
            validate_pair(&dictation, Some(&repaste)),
            Err(BindingError::Conflict)
        );
    }

    #[test]
    fn validate_pair_keyed_dictation_allows_different_key() {
        // Two consuming shortcuts with different keys coexist even when the
        // modifier set overlaps — only identical full accelerators collide.
        let dictation = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Command],
            key: Some("KeyR".to_string()),
            display_key: None,
        };
        let repaste = RepasteBinding {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Command],
            key: "KeyV".to_string(),
            display_key: None,
        };
        assert_eq!(validate_pair(&dictation, Some(&repaste)), Ok(()));
    }

    #[test]
    fn validate_pair_keyed_dictation_rejects_identical_accelerator() {
        let dictation = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Command],
            key: Some("KeyV".to_string()),
            display_key: None,
        };
        let repaste = RepasteBinding {
            mods: vec![HotkeyModifier::Command, HotkeyModifier::Control],
            key: "KeyV".to_string(),
            display_key: None,
        };
        assert_eq!(
            validate_pair(&dictation, Some(&repaste)),
            Err(BindingError::Conflict)
        );
    }

    #[test]
    fn validate_pair_accepts_the_shipped_defaults() {
        assert_eq!(
            validate_pair(
                &DictationBinding::default_dictation(),
                Some(&RepasteBinding::default_repaste())
            ),
            Ok(())
        );
    }

    #[test]
    fn validate_pair_allows_disabled_repaste() {
        assert_eq!(
            validate_pair(&DictationBinding::default_dictation(), None),
            Ok(())
        );
    }

    // --- task 50a: superset re-paste trips modifier-only dictation --------

    #[test]
    fn validate_pair_rejects_repaste_superset_of_modifier_only_dictation() {
        // Modifier-only dictation ⌃⌥ fires on a subset match, so a re-paste
        // whose modifiers are a strict superset (⌃⌥⌘+V) trips it on every press.
        let dictation = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Option],
            key: None,
            display_key: None,
        };
        let repaste = RepasteBinding {
            mods: vec![
                HotkeyModifier::Control,
                HotkeyModifier::Option,
                HotkeyModifier::Command,
            ],
            key: "KeyR".to_string(),
            display_key: None,
        };
        assert_eq!(
            validate_pair(&dictation, Some(&repaste)),
            Err(BindingError::SupersetTripsDictation)
        );
    }

    #[test]
    fn validate_pair_allows_repaste_disjoint_modifiers() {
        // A re-paste that is NOT a superset (⌘⇧ vs dictation ⌃⌥) coexists.
        let dictation = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Option],
            key: None,
            display_key: None,
        };
        let repaste = RepasteBinding {
            mods: vec![HotkeyModifier::Command, HotkeyModifier::Shift],
            key: "KeyR".to_string(),
            display_key: None,
        };
        assert_eq!(validate_pair(&dictation, Some(&repaste)), Ok(()));
    }

    #[test]
    fn validate_pair_keyed_dictation_ignores_superset_rule() {
        // Keyed dictation is a consuming shortcut, not a subset flags-tap, so a
        // superset re-paste does NOT trip it — only an identical accelerator.
        let dictation = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Option],
            key: Some("KeyD".to_string()),
            display_key: None,
        };
        let repaste = RepasteBinding {
            mods: vec![
                HotkeyModifier::Control,
                HotkeyModifier::Option,
                HotkeyModifier::Command,
            ],
            key: "KeyR".to_string(),
            display_key: None,
        };
        assert_eq!(validate_pair(&dictation, Some(&repaste)), Ok(()));
    }

    // --- task 50b: reserved self-injected paste combos --------------------

    #[test]
    fn reserved_paste_combos_are_rejected() {
        // ⌘V (the synthetic paste) — rejected on both binding types.
        let repaste_cmd_v = RepasteBinding {
            mods: vec![HotkeyModifier::Command],
            key: "KeyV".to_string(),
            display_key: None,
        };
        assert_eq!(
            repaste_cmd_v.validate(),
            Err(BindingError::ReservedForPaste)
        );
        let dictation_cmd_v = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Command],
            key: Some("KeyV".to_string()),
            display_key: None,
        };
        assert_eq!(
            dictation_cmd_v.validate(),
            Err(BindingError::ReservedForPaste)
        );

        // ⌘⇧V ("paste and match style") — also reserved.
        let repaste_cmd_shift_v = RepasteBinding {
            mods: vec![HotkeyModifier::Command, HotkeyModifier::Shift],
            key: "KeyV".to_string(),
            display_key: None,
        };
        assert_eq!(
            repaste_cmd_shift_v.validate(),
            Err(BindingError::ReservedForPaste)
        );
    }

    #[test]
    fn default_repaste_ctrl_cmd_v_is_not_reserved() {
        // ⌃⌘V (adds Control) is the shipped default and must stay bindable.
        assert_eq!(RepasteBinding::default_repaste().validate(), Ok(()));
        // A different key with ⌘ is also fine — only KeyV is reserved.
        let cmd_c = RepasteBinding {
            mods: vec![HotkeyModifier::Command],
            key: "KeyC".to_string(),
            display_key: None,
        };
        // ⌘ alone is Command-only → still needs the hard-modifier floor, which
        // Command satisfies; not reserved (KeyC), so it validates.
        assert_eq!(cmd_c.validate(), Ok(()));
    }

    // --- task 50c: keyed modifier floor (Shift/Option alone) --------------

    #[test]
    fn keyed_binding_rejects_shift_or_option_alone() {
        let shift_only = RepasteBinding {
            mods: vec![HotkeyModifier::Shift],
            key: "KeyR".to_string(),
            display_key: None,
        };
        assert_eq!(shift_only.validate(), Err(BindingError::WeakModifier));

        let option_only = RepasteBinding {
            mods: vec![HotkeyModifier::Option],
            key: "KeyE".to_string(),
            display_key: None,
        };
        assert_eq!(option_only.validate(), Err(BindingError::WeakModifier));

        let dictation_shift_only = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Shift],
            key: Some("KeyR".to_string()),
            display_key: None,
        };
        assert_eq!(
            dictation_shift_only.validate(),
            Err(BindingError::WeakModifier)
        );
    }

    #[test]
    fn keyed_binding_accepts_hard_modifier_or_two_modifiers() {
        // A single hard modifier (Control/Command/Fn) is enough.
        let ctrl = RepasteBinding {
            mods: vec![HotkeyModifier::Control],
            key: "KeyR".to_string(),
            display_key: None,
        };
        assert_eq!(ctrl.validate(), Ok(()));
        let fn_keyed = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Fn],
            key: Some("KeyR".to_string()),
            display_key: None,
        };
        assert_eq!(fn_keyed.validate(), Ok(()));

        // Two soft modifiers (Shift+Option) also clear the floor.
        let shift_option = RepasteBinding {
            mods: vec![HotkeyModifier::Shift, HotkeyModifier::Option],
            key: "KeyR".to_string(),
            display_key: None,
        };
        assert_eq!(shift_option.validate(), Ok(()));
    }

    #[test]
    fn duplicate_modifiers_do_not_satisfy_the_floor() {
        // A hand-edited store can carry a duplicated modifier the recorder can
        // never emit. `["shift","shift"]` is effectively Shift-only, so the floor
        // must count the distinct set (1), not the list length (2) — otherwise a
        // typing-swallowing chord would register at boot.
        let keyed_dup = RepasteBinding {
            mods: vec![HotkeyModifier::Shift, HotkeyModifier::Shift],
            key: "KeyR".to_string(),
            display_key: None,
        };
        assert_eq!(keyed_dup.validate(), Err(BindingError::WeakModifier));

        let dictation_keyed_dup = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Option, HotkeyModifier::Option],
            key: Some("KeyE".to_string()),
            display_key: None,
        };
        assert_eq!(
            dictation_keyed_dup.validate(),
            Err(BindingError::WeakModifier)
        );

        // The modifier-only floor is the same class: `["shift","shift"]` with no
        // key is a single-modifier chord and must be rejected as too few.
        let modifier_only_dup = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Shift, HotkeyModifier::Shift],
            key: None,
            display_key: None,
        };
        assert_eq!(
            modifier_only_dup.validate(),
            Err(BindingError::TooFewModifiers)
        );
    }

    #[test]
    fn validate_pair_surfaces_invalid_dictation_first() {
        let bad = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control],
            key: None,
            display_key: None,
        };
        assert_eq!(
            validate_pair(&bad, Some(&RepasteBinding::default_repaste())),
            Err(BindingError::TooFewModifiers)
        );
    }

    // --- serde round-trips ------------------------------------------------

    #[test]
    fn dictation_modifiers_json_round_trip() {
        // Legacy modifier-only JSON has no `key` member; `default` loads it as
        // `None` and `skip_serializing_if` resaves it byte-for-byte (no
        // gratuitous `"key":null`).
        let json = serde_json::json!({ "kind": "modifiers", "mods": ["control", "option"] });
        let binding: DictationBinding = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(binding, DictationBinding::default_dictation());
        assert_eq!(binding.key(), None);
        assert_eq!(serde_json::to_value(&binding).unwrap(), json);
    }

    #[test]
    fn dictation_keyed_json_round_trip() {
        let json = serde_json::json!({
            "kind": "modifiers",
            "mods": ["control", "shift"],
            "key": "KeyR",
        });
        let binding: DictationBinding = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            binding,
            DictationBinding::Modifiers {
                mods: vec![HotkeyModifier::Control, HotkeyModifier::Shift],
                key: Some("KeyR".to_string()),
                display_key: None,
            }
        );
        assert_eq!(binding.key(), Some("KeyR"));
        assert_eq!(serde_json::to_value(&binding).unwrap(), json);
    }

    #[test]
    fn repaste_json_round_trip() {
        let json = serde_json::json!({ "mods": ["control", "command"], "key": "KeyV" });
        let binding: RepasteBinding = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(binding, RepasteBinding::default_repaste());
        assert_eq!(serde_json::to_value(&binding).unwrap(), json);
    }

    #[test]
    fn repaste_null_deserialises_to_disabled() {
        let disabled: Option<RepasteBinding> =
            serde_json::from_value(serde_json::Value::Null).unwrap();
        assert!(disabled.is_none());
        assert_eq!(
            serde_json::to_value(Option::<RepasteBinding>::None).unwrap(),
            serde_json::Value::Null
        );
    }

    #[test]
    fn all_modifier_symbols_are_stable() {
        assert_eq!(HotkeyModifier::Control.to_symbol(), "⌃");
        assert_eq!(HotkeyModifier::Option.to_symbol(), "⌥");
        assert_eq!(HotkeyModifier::Command.to_symbol(), "⌘");
        assert_eq!(HotkeyModifier::Shift.to_symbol(), "⇧");
        assert_eq!(HotkeyModifier::Fn.to_symbol(), "fn");
    }

    // --- task 53a: anchor-key allowlist ------------------------------------

    #[test]
    fn allowlisted_keys_validate() {
        for code in [
            "KeyR",
            "Digit1",
            "F1",
            "F13",
            "F24",
            "ArrowUp",
            "Home",
            "PageDown",
            "Comma",
            "Semicolon",
            "Space",
            "Tab",
            "Enter",
        ] {
            let keyed = DictationBinding::Modifiers {
                mods: vec![HotkeyModifier::Control],
                key: Some(code.to_string()),
                display_key: None,
            };
            assert_eq!(keyed.validate(), Ok(()), "{code} should be a valid anchor");

            let repaste = RepasteBinding {
                mods: vec![HotkeyModifier::Control],
                key: code.to_string(),
                display_key: None,
            };
            assert_eq!(
                repaste.validate(),
                Ok(()),
                "{code} should be a valid anchor"
            );
        }
    }

    #[test]
    fn caps_lock_class_keys_are_rejected_as_anchors() {
        for code in [
            "CapsLock",
            "NumLock",
            "ScrollLock",
            "ContextMenu",
            "MediaPlayPause",
            "AudioVolumeUp",
            "Lang1",
            "Convert",
        ] {
            let keyed = DictationBinding::Modifiers {
                mods: vec![HotkeyModifier::Control],
                key: Some(code.to_string()),
                display_key: None,
            };
            assert_eq!(
                keyed.validate(),
                Err(BindingError::InvalidAnchorKey),
                "{code} must be rejected as an anchor"
            );

            let repaste = RepasteBinding {
                mods: vec![HotkeyModifier::Control],
                key: code.to_string(),
                display_key: None,
            };
            assert_eq!(
                repaste.validate(),
                Err(BindingError::InvalidAnchorKey),
                "{code} must be rejected as an anchor"
            );
        }
    }

    #[test]
    fn invalid_anchor_key_message_matches_frontend_copy() {
        assert_eq!(
            BindingError::InvalidAnchorKey.to_string(),
            "That key can't be used."
        );
    }

    // --- task 54: layout-aware display key ---------------------------------

    #[test]
    fn label_prefers_display_key_over_physical_code() {
        // AZERTY: physical code is `KeyQ`, but the user typed "a" — the label
        // should show the typed letter, not the QWERTY-shaped code glyph.
        let keyed = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control],
            key: Some("KeyQ".to_string()),
            display_key: Some("a".to_string()),
        };
        assert_eq!(keyed.label(), "⌃A");

        let repaste = RepasteBinding {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Command],
            key: "KeyQ".to_string(),
            display_key: Some("a".to_string()),
        };
        assert_eq!(repaste.label(), "⌃⌘A");
    }

    #[test]
    fn label_falls_back_to_code_symbol_when_display_key_absent() {
        let keyed = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control],
            key: Some("KeyR".to_string()),
            display_key: None,
        };
        assert_eq!(keyed.label(), "⌃R");
    }

    #[test]
    fn label_falls_back_to_code_symbol_for_blank_space_display_key() {
        // `KeyboardEvent.key` for the spacebar is a literal `" "` — a naive
        // single-char uppercase would render an invisible glyph. The label
        // must fall back to the code-derived "Space" instead.
        let keyed = DictationBinding::Modifiers {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Command],
            key: Some("Space".to_string()),
            display_key: Some(" ".to_string()),
        };
        assert_eq!(keyed.label(), "⌃⌘Space");

        let repaste = RepasteBinding {
            mods: vec![HotkeyModifier::Control, HotkeyModifier::Command],
            key: "Space".to_string(),
            display_key: Some(" ".to_string()),
        };
        assert_eq!(repaste.label(), "⌃⌘Space");
    }

    #[test]
    fn dictation_display_key_json_round_trip() {
        let json = serde_json::json!({
            "kind": "modifiers",
            "mods": ["control"],
            "key": "KeyQ",
            "displayKey": "a",
        });
        let binding: DictationBinding = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            binding,
            DictationBinding::Modifiers {
                mods: vec![HotkeyModifier::Control],
                key: Some("KeyQ".to_string()),
                display_key: Some("a".to_string()),
            }
        );
        assert_eq!(serde_json::to_value(&binding).unwrap(), json);
    }

    #[test]
    fn dictation_without_display_key_deserialises_and_resaves_without_it() {
        // Pre-054 stored JSON has no `displayKey` member at all — it must
        // default to `None` and resave byte-for-byte (no gratuitous null).
        let json = serde_json::json!({
            "kind": "modifiers",
            "mods": ["control", "shift"],
            "key": "KeyR",
        });
        let binding: DictationBinding = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(binding.label(), "⌃⇧R");
        assert_eq!(serde_json::to_value(&binding).unwrap(), json);
    }

    #[test]
    fn repaste_display_key_json_round_trip() {
        let json = serde_json::json!({
            "mods": ["control", "command"],
            "key": "KeyV",
            "displayKey": "v",
        });
        let binding: RepasteBinding = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(
            binding,
            RepasteBinding {
                mods: vec![HotkeyModifier::Control, HotkeyModifier::Command],
                key: "KeyV".to_string(),
                display_key: Some("v".to_string()),
            }
        );
        assert_eq!(serde_json::to_value(&binding).unwrap(), json);
    }
}
