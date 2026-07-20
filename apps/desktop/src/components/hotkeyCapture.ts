/**
 * Feature 037 — the pure capture logic behind `HotkeyRecorder`.
 *
 * The DOM keyboard glue in the component stays thin: it translates raw
 * `KeyboardEvent`s into {@link CaptureAction}s and feeds them to
 * {@link captureReducer}. Everything decision-shaped — accumulating held
 * modifiers, the ≥2-modifier gate, the combo commit — lives here so it is
 * unit-testable without a DOM (see `hotkeyCapture.test.ts`).
 *
 * The reducer is deliberately ignorant of the binding *types* it feeds
 * (`DictationBinding` / `RepasteBinding`); it emits a neutral {@link CaptureResult}
 * the component maps onto the right shape. That keeps this module free of the
 * settings contract and easy to reason about in isolation.
 */
import type { HotkeyModifier } from "@/hooks/useSettings";

/** Recorder flavour: modifier-only (dictation) vs modifier+key (re-paste). */
export type CaptureMode = "modifiers" | "combo";

/** Minimum modifiers a modifier-only dictation chord requires (mirrors Rust). */
export const MIN_DICTATION_MODIFIERS = 2;

/**
 * Inline validation copy, mirroring `BindingError`'s `Display` in
 * `hotkey_binding.rs`. The recorder surfaces the reducer-level errors
 * (too-few / missing) inline; the cross-binding `Conflict` only Rust can judge,
 * so it arrives via the command's rejected promise (a toast), not from here.
 */
export const BINDING_ERROR_COPY = {
  tooFewModifiers: "Use two modifiers, or a modifier and a key.",
  missingModifier: "Use at least one modifier key.",
  missingKey: "Add a key to complete the shortcut.",
  conflict: "The dictation and re-paste shortcuts can't use the same keys.",
  // Plan 039 task 50b — combos Muni injects itself (⌘V / ⌘⇧V). The user-facing
  // copy names the exact rejected chord (see `reservedPasteError`), so it lives
  // as a function rather than a static string. Mirrors
  // `BindingError::ReservedForPaste` in `hotkey_binding.rs`.
  // Plan 039 task 50c — a keyed binding whose only modifier is Shift or Option
  // would swallow typing / accents. Mirrors `BindingError::WeakModifier`.
  weakModifier: "Add Control, Command, or Fn, or a second modifier.",
  // Plan 039 task 50a — a re-paste chord that is a superset of a modifier-only
  // dictation chord always trips dictation. Judged cross-binding in Rust
  // (`BindingError::SupersetTripsDictation`), so it arrives here as a toast, not
  // an inline reducer error; the copy is mirrored for parity.
  supersetTripsDictation:
    "These modifiers would always trigger dictation. Use a different combo.",
  // Plan 039 task 53a — the anchor key isn't in the allowlist (CapsLock,
  // NumLock, media keys, IME codes, …). Mirrors `BindingError::InvalidAnchorKey`.
  invalidAnchorKey: "That key can't be used.",
} as const;

/**
 * Combos Muni injects itself and therefore refuses to bind (task 50b): the
 * synthetic paste is ⌘V and macOS "paste and match style" is ⌘⇧V. Only `KeyV`
 * with exactly `{command}` or `{command, shift}` is reserved — the default
 * re-paste ⌃⌘V (adds Control) stays bindable. Mirrors `is_reserved_paste_combo`
 * in `hotkey_binding.rs`.
 */
export function isReservedPasteCombo(
  mods: HotkeyModifier[],
  key: string,
): boolean {
  if (key !== "KeyV") return false;
  const set = new Set(mods);
  const isExactly = (...want: HotkeyModifier[]) =>
    set.size === want.length && want.every((m) => set.has(m));
  return isExactly("command") || isExactly("command", "shift");
}

/**
 * Modifier floor for a keyed binding (task 50c): Shift or Option *alone* would
 * swallow ordinary typing / accented input, so a keyed binding needs at least
 * one hard modifier (Control, Command, or Fn) OR two *distinct* modifiers.
 * Counts the set, not the list, so a duplicated modifier (which only a
 * hand-edited store could produce) can't inflate the count past the floor.
 * Mirrors `keyed_modifier_floor_ok` in `hotkey_binding.rs`.
 */
export function keyedModifierFloorOk(mods: HotkeyModifier[]): boolean {
  const hasHard = mods.some(
    (m) => m === "control" || m === "command" || m === "fn",
  );
  return hasHard || new Set(mods).size >= MIN_DICTATION_MODIFIERS;
}

/**
 * Validate a keyed (modifier+key) capture against the single-binding rules that
 * don't need the other binding — the reserved-paste and modifier-floor rules
 * (tasks 50b/50c). Returns the inline error copy, or null if it passes. (The
 * cross-binding superset/conflict rules are Rust's job and surface as toasts.)
 */
export function keyedCaptureError(
  mods: HotkeyModifier[],
  key: string,
): string | null {
  if (isReservedPasteCombo(mods, key)) return reservedPasteError(mods, key);
  if (!keyedModifierFloorOk(mods)) return BINDING_ERROR_COPY.weakModifier;
  return null;
}

/**
 * Reserved-paste rejection copy that NAMES the exact chord the user pressed —
 * `Can't use ⌘V.` / `Can't use ⌘⇧V.` — instead of a generic "Muni uses this to
 * paste" (dogfood feedback 2026-07-09: be specific). Only ever called for a
 * combo [`isReservedPasteCombo`] already matched, so the chord is one of those
 * two. Uses the same glyph helpers as the recorder chips for a consistent look.
 */
export function reservedPasteError(mods: HotkeyModifier[], key: string): string {
  const chord =
    sortModifiers(mods).map(modifierSymbol).join("") + keySymbol(key);
  return `Can't use ${chord}.`;
}

/**
 * Anchor-key allowlist for a keyed binding (task 53a): letters, digits,
 * F-keys, navigation, punctuation, and Space/Tab/Enter as a deliberate
 * whitespace/control choice. Everything else — CapsLock, NumLock, ScrollLock,
 * ContextMenu, media keys, IME composition codes — is rejected. Mirrors
 * `is_allowed_anchor_key` in `hotkey_binding.rs` byte-for-byte (Rust is the
 * source of truth); keep the two lists in lockstep.
 */
const ANCHOR_KEY_ALLOWLIST: ReadonlySet<string> = new Set([
  // Letters (KeyA–KeyZ).
  ...Array.from({ length: 26 }, (_, i) => `Key${String.fromCharCode(65 + i)}`),
  // Digits (Digit0–Digit9).
  ...Array.from({ length: 10 }, (_, i) => `Digit${i}`),
  // Function keys (F1–F24 — some Apple keyboards expose up to F19).
  ...Array.from({ length: 24 }, (_, i) => `F${i + 1}`),
  // Navigation.
  "ArrowUp",
  "ArrowDown",
  "ArrowLeft",
  "ArrowRight",
  "Home",
  "End",
  "PageUp",
  "PageDown",
  "Insert",
  "Delete",
  // Punctuation.
  "Backquote",
  "Minus",
  "Equal",
  "BracketLeft",
  "BracketRight",
  "Backslash",
  "Semicolon",
  "Quote",
  "Comma",
  "Period",
  "Slash",
  "IntlBackslash",
  // Deliberate whitespace/control choices.
  "Space",
  "Tab",
  "Enter",
]);

export function isAllowedAnchorKey(code: string): boolean {
  return ANCHOR_KEY_ALLOWLIST.has(code);
}

/**
 * True for a keydown that belongs to an IME composition rather than a real
 * physical key the user intends to bind (task 53b). `isComposing` covers
 * browsers that set it on the native event; `keyCode === 229` is the legacy
 * fallback some IMEs still emit instead.
 */
export function isComposingKeydown(event: {
  isComposing?: boolean;
  keyCode?: number;
}): boolean {
  return Boolean(event.isComposing) || event.keyCode === 229;
}

/**
 * Neutral commit payload the component maps onto a concrete binding. A
 * `modifiers` result carries an optional `key`: absent for a modifier-only
 * chord, present for a keyed push-to-talk binding (Feature 038).
 */
export type CaptureResult =
  | {
      kind: "modifiers";
      mods: HotkeyModifier[];
      key?: string;
      /** Plan 039 task 54 — see {@link CaptureAction}'s `displayKey`. */
      displayKey?: string;
    }
  | {
      kind: "combo";
      mods: HotkeyModifier[];
      key: string;
      displayKey?: string;
    };

export type CaptureState = {
  mode: CaptureMode;
  /** Whether the recorder is actively listening for keys. */
  recording: boolean;
  /** Modifiers currently pressed, in press order. */
  held: HotkeyModifier[];
  /** Largest set of modifiers held together this session (the chord peak). */
  peak: HotkeyModifier[];
  /** Non-null once a valid chord/combo is captured; parent commits it. */
  result: CaptureResult | null;
  /** Inline validation message, or null. */
  error: string | null;
};

export type CaptureAction =
  | { type: "start" }
  | { type: "cancel" }
  | {
      type: "keydown";
      modifier: HotkeyModifier | null;
      code: string;
      /**
       * Plan 039 task 54 — the raw `KeyboardEvent.key` at record time, a
       * layout-aware display hint carried through to a committed
       * {@link CaptureResult} (only meaningful for a non-modifier/anchor
       * keydown; modifier keydowns ignore it).
       */
      displayKey?: string;
    }
  | { type: "keyup"; modifier: HotkeyModifier | null };

/** Fresh idle state for a recorder of the given mode. */
export function initialCaptureState(mode: CaptureMode): CaptureState {
  return {
    mode,
    recording: false,
    held: [],
    peak: [],
    result: null,
    error: null,
  };
}

/**
 * Map a `KeyboardEvent.code` to Muni's modifier token, or null for a
 * non-modifier key. The `fn` key is intentionally absent: browsers don't emit
 * a reliable `code` for it, so it can't be captured from the DOM (the Rust
 * model still accepts it for completeness).
 */
export function modifierFromCode(code: string): HotkeyModifier | null {
  switch (code) {
    case "ControlLeft":
    case "ControlRight":
      return "control";
    case "AltLeft":
    case "AltRight":
      return "option";
    case "MetaLeft":
    case "MetaRight":
      return "command";
    case "ShiftLeft":
    case "ShiftRight":
      return "shift";
    default:
      return null;
  }
}

/**
 * Canonical modifier order for display and storage — the Apple convention
 * `fn ⌃ ⌥ ⇧ ⌘` (Command always last). Capture records modifiers in *press*
 * order, so we normalise to this before committing/rendering; otherwise
 * pressing ⌘ then ⌃ would show as `⌘⌃`. Mirrors `order_rank` in
 * `hotkey_binding.rs` so the Rust label/accelerator and the FE agree.
 */
const MODIFIER_ORDER: readonly HotkeyModifier[] = [
  "fn",
  "control",
  "option",
  "shift",
  "command",
];

export function sortModifiers(mods: HotkeyModifier[]): HotkeyModifier[] {
  return [...mods].sort(
    (a, b) => MODIFIER_ORDER.indexOf(a) - MODIFIER_ORDER.indexOf(b),
  );
}

export function captureReducer(
  state: CaptureState,
  action: CaptureAction,
): CaptureState {
  switch (action.type) {
    case "start":
      return { ...initialCaptureState(state.mode), recording: true };
    case "cancel":
      // Also clear the validation error so clicking away leaves no stray red
      // message behind.
      return { ...state, recording: false, held: [], peak: [], error: null };
    case "keydown":
      return reduceKeydown(state, action);
    case "keyup":
      return reduceKeyup(state, action);
    default:
      return state;
  }
}

function reduceKeydown(
  state: CaptureState,
  action: Extract<CaptureAction, { type: "keydown" }>,
): CaptureState {
  if (!state.recording) return state;

  if (action.modifier !== null) {
    const held = state.held.includes(action.modifier)
      ? state.held
      : [...state.held, action.modifier];
    // Track the peak so a chord captured across staggered key presses commits
    // the fullest set, not whatever happened to still be down at release.
    const peak = held.length > state.peak.length ? held : state.peak;
    return { ...state, held, peak, error: null };
  }

  // A non-modifier key. Task 53a rejects any code outside the anchor
  // allowlist before it even reaches the modifier-count/reserved-combo
  // checks below — a CapsLock/media/IME key is invalid regardless of what's
  // held, in either mode.
  if (!isAllowedAnchorKey(action.code)) {
    return { ...state, error: BINDING_ERROR_COPY.invalidAnchorKey };
  }

  if (state.mode === "modifiers") {
    // With ≥1 modifier held, a non-modifier key commits a keyed push-to-talk
    // binding (Feature 038) — same commit-on-keydown move as combo mode. With
    // no modifier held it's a bare key, which needs an anchor: nudge the user.
    if (state.held.length === 0) {
      return { ...state, error: BINDING_ERROR_COPY.tooFewModifiers };
    }
    const mods = sortModifiers(state.held);
    // Tasks 50b/50c: a keyed dictation binding carries the reserved-paste and
    // modifier-floor rules — reject inline, stay recording for a retry.
    const keyedError = keyedCaptureError(mods, action.code);
    if (keyedError) {
      return { ...state, error: keyedError };
    }
    return {
      ...state,
      recording: false,
      result: {
        kind: "modifiers",
        mods,
        key: action.code,
        displayKey: action.displayKey,
      },
      error: null,
    };
  }

  // Combo recorder commits on the first non-modifier keydown, gated on ≥1
  // currently-held modifier.
  if (state.held.length === 0) {
    return { ...state, error: BINDING_ERROR_COPY.missingModifier };
  }
  const mods = sortModifiers(state.held);
  // Tasks 50b/50c apply to the re-paste combo too (it is also keyed).
  const keyedError = keyedCaptureError(mods, action.code);
  if (keyedError) {
    return { ...state, error: keyedError };
  }
  return {
    ...state,
    recording: false,
    result: {
      kind: "combo",
      mods,
      key: action.code,
      displayKey: action.displayKey,
    },
    error: null,
  };
}

function reduceKeyup(
  state: CaptureState,
  action: Extract<CaptureAction, { type: "keyup" }>,
): CaptureState {
  if (!state.recording) return state;
  if (action.modifier === null) return state;

  // Ignore a release for a modifier we're not currently holding. Releasing a
  // multi-key chord fires one keyup per key; once the first release resolves
  // the chord (commit or reject) and clears `held`, the trailing releases must
  // be no-ops — otherwise they re-evaluate an empty peak and flip the error
  // (e.g. "Shift needs an anchor" briefly downgrading to "too few modifiers").
  if (!state.held.includes(action.modifier)) return state;

  const held = state.held.filter((m) => m !== action.modifier);

  // The combo recorder only commits on a non-modifier keydown; releasing a
  // modifier just narrows the held set.
  if (state.mode === "combo") {
    return { ...state, held };
  }

  // Modifier-only recorder commits when the user starts releasing the chord.
  // Shift-inclusive chords are allowed now (Feature 038) — the safe alternative
  // to the `Ctrl+Shift` footgun is a keyed `Ctrl+Shift+key` binding, and the
  // recorder no longer rejects Shift-only chords.
  if (state.peak.length >= MIN_DICTATION_MODIFIERS) {
    return {
      ...state,
      recording: false,
      held: [],
      result: { kind: "modifiers", mods: sortModifiers(state.peak) },
      error: null,
    };
  }

  // Released with too few modifiers held — flag it and reset for a retry
  // (stay recording so the user can immediately press a fuller chord).
  //
  // Preserve a MORE SPECIFIC error the keydown already set this attempt: a
  // keyed anchor that was pressed and rejected (`weakModifier` for Shift+A,
  // `reservedForPaste` for ⌘V) must NOT be flipped to the generic
  // `tooFewModifiers` on the modifier release — that produced a two-message
  // flicker (specific error → generic) users saw as "two errors". A fresh
  // chord clears `error` on the first modifier keydown, so a lingering error
  // here always belongs to the CURRENT attempt. `tooFewModifiers` still shows
  // for a bare modifier-only chord released too short (no prior error).
  return {
    ...state,
    held,
    peak: held,
    error: state.error ?? BINDING_ERROR_COPY.tooFewModifiers,
  };
}

// --- display helpers (mirror the Rust `label` methods) -------------------

const MODIFIER_SYMBOL: Record<HotkeyModifier, string> = {
  control: "⌃",
  option: "⌥",
  command: "⌘",
  shift: "⇧",
  fn: "fn",
};

export function modifierSymbol(modifier: HotkeyModifier): string {
  return MODIFIER_SYMBOL[modifier];
}

/** `"KeyV"` → `"V"`, `"Digit1"` → `"1"`, else the code verbatim (`"F13"`). */
export function keySymbol(code: string): string {
  if (code.startsWith("Key")) return code.slice(3);
  if (code.startsWith("Digit")) return code.slice(5);
  return code;
}

/**
 * Display glyph for a keyed binding's key (plan 039 task 54): prefers the
 * layout-aware `displayKey` (the `KeyboardEvent.key` captured at record time)
 * over the physical-code-derived {@link keySymbol} fallback used for bindings
 * stored before this field existed. A single-character display key is
 * uppercased for a consistent chip glyph (`"a"` → `"A"`); a multi-character
 * one (`"Enter"`, `"ArrowUp"`) is already presentable and passed through. A
 * blank (whitespace-only) display key — e.g. `" "` for the Space anchor,
 * whose `KeyboardEvent.key` is a literal space — renders nothing useful, so
 * it falls back to the code glyph the same as an absent one. Mirrors
 * `display_key_symbol` in `hotkey_binding.rs`.
 */
export function keyDisplaySymbol(code: string, displayKey?: string): string {
  if (displayKey && displayKey.trim().length > 0) {
    return [...displayKey].length === 1 ? displayKey.toUpperCase() : displayKey;
  }
  return keySymbol(code);
}

/**
 * True when a raw `KeyboardEvent.key` is trustworthy as a layout-aware
 * display hint (task 54) rather than noise the recorder should discard in
 * favor of the code-derived {@link keySymbol} fallback.
 *
 * Two cases are untrustworthy, both because the reported `key` no longer
 * reflects the physical key the user thinks they pressed:
 *  - `"Dead"`, reported mid dead-key composition (an accent key before its
 *    base character lands) rather than a real, bindable keypress.
 *  - Any single-character glyph captured while Option is held: macOS remaps
 *    Option-chords to unrelated symbols on many layouts (⌥R → "®", ⌥D → "∂"),
 *    so the reported character doesn't match the key label. Multi-character
 *    key names (`"Enter"`, `"ArrowUp"`, `"F1"`) are unaffected by Option and
 *    stay trustworthy.
 */
export function isDisplayableKey(key: string, optionHeld: boolean): boolean {
  if (key === "Dead") return false;
  if (optionHeld && [...key].length === 1) return false;
  return true;
}

/** Ordered glyph chips for a binding, for the recorder's current-value display. */
export function dictationChips(binding: DictationBindingLike): string[] {
  const chips = sortModifiers(binding.mods).map(modifierSymbol);
  if (binding.key) chips.push(keyDisplaySymbol(binding.key, binding.displayKey));
  return chips;
}

export function repasteChips(binding: RepasteBindingLike): string[] {
  return [
    ...sortModifiers(binding.mods).map(modifierSymbol),
    keyDisplaySymbol(binding.key, binding.displayKey),
  ];
}

// Structural aliases so the helpers don't import the settings contract
// (avoids a cycle and keeps this module self-contained).
type DictationBindingLike = {
  kind: "modifiers";
  mods: HotkeyModifier[];
  key?: string;
  displayKey?: string;
};
type RepasteBindingLike = {
  mods: HotkeyModifier[];
  key: string;
  displayKey?: string;
};
