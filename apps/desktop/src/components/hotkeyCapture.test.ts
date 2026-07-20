import { describe, expect, it } from "vitest";

import {
  BINDING_ERROR_COPY,
  captureReducer,
  type CaptureAction,
  type CaptureMode,
  type CaptureState,
  dictationChips,
  initialCaptureState,
  isAllowedAnchorKey,
  isComposingKeydown,
  isDisplayableKey,
  isReservedPasteCombo,
  keyDisplaySymbol,
  keySymbol,
  keyedModifierFloorOk,
  modifierFromCode,
  repasteChips,
} from "./hotkeyCapture";

/** Fold a sequence of actions over a freshly-started recorder of `mode`. */
function run(mode: CaptureMode, actions: CaptureAction[]): CaptureState {
  return actions.reduce(
    captureReducer,
    captureReducer(initialCaptureState(mode), { type: "start" }),
  );
}

describe("captureReducer — modifiers mode", () => {
  it("accumulates held modifiers on keydown", () => {
    const state = run("modifiers", [
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "keydown", modifier: "option", code: "AltLeft" },
    ]);
    expect(state.held).toEqual(["control", "option"]);
    expect(state.peak).toEqual(["control", "option"]);
    expect(state.result).toBeNull();
  });

  it("commits the peak chord on release once ≥2 modifiers were held", () => {
    const state = run("modifiers", [
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "keydown", modifier: "option", code: "AltLeft" },
      { type: "keyup", modifier: "option" },
    ]);
    expect(state.recording).toBe(false);
    expect(state.result).toEqual({
      kind: "modifiers",
      mods: ["control", "option"],
    });
    expect(state.error).toBeNull();
  });

  it("commits modifiers in canonical order regardless of press order", () => {
    // Press Command first, then Control — canonical order is ⌃⌘, not ⌘⌃.
    const state = run("modifiers", [
      { type: "keydown", modifier: "command", code: "MetaLeft" },
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "keyup", modifier: "control" },
    ]);
    expect(state.result).toEqual({
      kind: "modifiers",
      mods: ["control", "command"],
    });
  });

  it("commits a Shift-inclusive modifier-only chord (footgun allowed)", () => {
    // Feature 038 removed the Shift rejection: Ctrl+Shift is a valid (if
    // footgun-y) modifier-only chord now — no error, commits the peak.
    const state = run("modifiers", [
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "keydown", modifier: "shift", code: "ShiftLeft" },
      { type: "keyup", modifier: "shift" },
    ]);
    expect(state.recording).toBe(false);
    expect(state.result).toEqual({
      kind: "modifiers",
      mods: ["control", "shift"],
    });
    expect(state.error).toBeNull();
  });

  it("errors when a non-modifier key is pressed with no modifier held", () => {
    const state = run("modifiers", [
      { type: "keydown", modifier: null, code: "KeyR" },
    ]);
    expect(state.error).toBe(BINDING_ERROR_COPY.tooFewModifiers);
    expect(state.result).toBeNull();
    expect(state.recording).toBe(true);
  });

  it("commits a keyed push-to-talk binding on a non-modifier keydown", () => {
    // One modifier held, then a key: Feature 038 commits { mods, key }.
    const state = run("modifiers", [
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "keydown", modifier: null, code: "KeyR" },
    ]);
    expect(state.recording).toBe(false);
    expect(state.result).toEqual({
      kind: "modifiers",
      mods: ["control"],
      key: "KeyR",
    });
    expect(state.error).toBeNull();
  });

  it("commits a keyed binding carrying every held modifier", () => {
    const state = run("modifiers", [
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "keydown", modifier: "shift", code: "ShiftLeft" },
      { type: "keydown", modifier: null, code: "KeyR" },
    ]);
    expect(state.recording).toBe(false);
    expect(state.result).toEqual({
      kind: "modifiers",
      mods: ["control", "shift"],
      key: "KeyR",
    });
  });

  it("clears the error on cancel (click-away)", () => {
    const state = run("modifiers", [
      { type: "keydown", modifier: null, code: "KeyR" }, // sets an error
      { type: "cancel" },
    ]);
    expect(state.error).toBeNull();
    expect(state.recording).toBe(false);
  });

  it("commits Ctrl+Shift once and ignores the trailing key release", () => {
    // Releasing Ctrl+Shift fires two keyups. The first (Shift) commits the peak
    // and stops recording; the trailing one (Ctrl) must be a no-op.
    const state = run("modifiers", [
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "keydown", modifier: "shift", code: "ShiftLeft" },
      { type: "keyup", modifier: "shift" },
      { type: "keyup", modifier: "control" },
    ]);
    expect(state.recording).toBe(false);
    expect(state.result).toEqual({
      kind: "modifiers",
      mods: ["control", "shift"],
    });
    expect(state.error).toBeNull();
  });

  it("commits the fullest chord even if keys are released in stages", () => {
    // Ctrl+Option+Cmd held, then Cmd released first: peak (all three) commits.
    const state = run("modifiers", [
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "keydown", modifier: "option", code: "AltLeft" },
      { type: "keydown", modifier: "command", code: "MetaLeft" },
      { type: "keyup", modifier: "command" },
    ]);
    expect(state.result).toEqual({
      kind: "modifiers",
      mods: ["control", "option", "command"],
    });
  });

  it("rejects a single-modifier chord with the too-few error", () => {
    const state = run("modifiers", [
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "keyup", modifier: "control" },
    ]);
    expect(state.result).toBeNull();
    expect(state.recording).toBe(true);
    expect(state.error).toBe(BINDING_ERROR_COPY.tooFewModifiers);
  });

  it("does not double-count a repeated keydown for the same modifier", () => {
    const state = run("modifiers", [
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "keydown", modifier: "control", code: "ControlLeft" },
    ]);
    expect(state.held).toEqual(["control"]);
  });

  // Plan 039 dogfood (2026-07-09) — a keyed anchor rejected on keydown must NOT
  // have its specific error flipped to the generic `tooFewModifiers` when the
  // modifier is released, which showed as two error messages flickering.
  it("keeps the weak-modifier error on release for a rejected Shift+key", () => {
    const state = run("modifiers", [
      { type: "keydown", modifier: "shift", code: "ShiftLeft" },
      { type: "keydown", modifier: null, code: "KeyA" },
      { type: "keyup", modifier: "shift" },
    ]);
    expect(state.error).toBe(BINDING_ERROR_COPY.weakModifier);
    expect(state.recording).toBe(true);
    expect(state.result).toBeNull();
  });

  it("keeps the reserved-paste error on release for a rejected ⌘V", () => {
    const state = run("modifiers", [
      { type: "keydown", modifier: "command", code: "MetaLeft" },
      { type: "keydown", modifier: null, code: "KeyV" },
      { type: "keyup", modifier: "command" },
    ]);
    expect(state.error).toBe("Can't use ⌘V.");
    expect(state.recording).toBe(true);
    expect(state.result).toBeNull();
  });
});

describe("captureReducer — combo mode", () => {
  it("commits modifiers + the first non-modifier keydown", () => {
    const state = run("combo", [
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "keydown", modifier: "command", code: "MetaLeft" },
      { type: "keydown", modifier: null, code: "KeyV" },
    ]);
    expect(state.recording).toBe(false);
    expect(state.result).toEqual({
      kind: "combo",
      mods: ["control", "command"],
      key: "KeyV",
    });
  });

  it("rejects a bare key with the missing-modifier error", () => {
    const state = run("combo", [
      { type: "keydown", modifier: null, code: "KeyV" },
    ]);
    expect(state.result).toBeNull();
    expect(state.error).toBe(BINDING_ERROR_COPY.missingModifier);
  });

  it("narrows the held set on modifier keyup without committing", () => {
    const state = run("combo", [
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "keydown", modifier: "command", code: "MetaLeft" },
      { type: "keyup", modifier: "command" },
    ]);
    expect(state.held).toEqual(["control"]);
    expect(state.result).toBeNull();
  });
});

describe("captureReducer — lifecycle", () => {
  it("ignores events before start", () => {
    const idle = initialCaptureState("modifiers");
    const next = captureReducer(idle, {
      type: "keydown",
      modifier: "control",
      code: "ControlLeft",
    });
    expect(next).toEqual(idle);
  });

  it("cancel clears the in-progress chord", () => {
    const state = run("modifiers", [
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "cancel" },
    ]);
    expect(state.recording).toBe(false);
    expect(state.held).toEqual([]);
  });
});

// Plan 039 task 50 — the validation rules mirrored byte-for-byte from
// `hotkey_binding.rs`. The Rust side has the identical case tables in its
// `#[cfg(test)]` module (`reserved_paste_combos_are_rejected`,
// `keyed_binding_rejects_shift_or_option_alone`,
// `keyed_binding_accepts_hard_modifier_or_two_modifiers`).
describe("task 50 — keyed binding validation (mirrors Rust)", () => {
  describe("isReservedPasteCombo (task 50b)", () => {
    it("reserves ⌘V and ⌘⇧V", () => {
      expect(isReservedPasteCombo(["command"], "KeyV")).toBe(true);
      expect(isReservedPasteCombo(["command", "shift"], "KeyV")).toBe(true);
    });
    it("does not reserve the default ⌃⌘V or non-V keys", () => {
      expect(isReservedPasteCombo(["control", "command"], "KeyV")).toBe(false);
      expect(isReservedPasteCombo(["command"], "KeyC")).toBe(false);
    });
  });

  describe("keyedModifierFloorOk (task 50c)", () => {
    it("accepts a single hard modifier (Control/Command/Fn)", () => {
      expect(keyedModifierFloorOk(["control"])).toBe(true);
      expect(keyedModifierFloorOk(["command"])).toBe(true);
      expect(keyedModifierFloorOk(["fn"])).toBe(true);
    });
    it("accepts two soft modifiers", () => {
      expect(keyedModifierFloorOk(["shift", "option"])).toBe(true);
    });
    it("rejects Shift or Option alone", () => {
      expect(keyedModifierFloorOk(["shift"])).toBe(false);
      expect(keyedModifierFloorOk(["option"])).toBe(false);
    });
    it("counts the distinct set, not the list length", () => {
      // A duplicated modifier (only a hand-edited store could produce it) is
      // still effectively single-modifier and must not clear the floor.
      expect(keyedModifierFloorOk(["shift", "shift"])).toBe(false);
      expect(keyedModifierFloorOk(["option", "option"])).toBe(false);
    });
  });

  it("modifiers mode: rejects a reserved ⌘V keyed capture inline", () => {
    const state = run("modifiers", [
      { type: "keydown", modifier: "command", code: "MetaLeft" },
      { type: "keydown", modifier: null, code: "KeyV" },
    ]);
    expect(state.result).toBeNull();
    expect(state.recording).toBe(true);
    expect(state.error).toBe("Can't use ⌘V.");
  });

  it("modifiers mode: rejects a Shift-only keyed capture inline", () => {
    const state = run("modifiers", [
      { type: "keydown", modifier: "shift", code: "ShiftLeft" },
      { type: "keydown", modifier: null, code: "KeyR" },
    ]);
    expect(state.result).toBeNull();
    expect(state.recording).toBe(true);
    expect(state.error).toBe(BINDING_ERROR_COPY.weakModifier);
  });

  it("combo mode: rejects ⌘V (reserved) inline", () => {
    const state = run("combo", [
      { type: "keydown", modifier: "command", code: "MetaLeft" },
      { type: "keydown", modifier: null, code: "KeyV" },
    ]);
    expect(state.result).toBeNull();
    expect(state.recording).toBe(true);
    expect(state.error).toBe("Can't use ⌘V.");
  });

  it("combo mode: rejects Option-only (weak modifier) inline", () => {
    const state = run("combo", [
      { type: "keydown", modifier: "option", code: "AltLeft" },
      { type: "keydown", modifier: null, code: "KeyE" },
    ]);
    expect(state.result).toBeNull();
    expect(state.recording).toBe(true);
    expect(state.error).toBe(BINDING_ERROR_COPY.weakModifier);
  });

  it("still commits a valid keyed binding (⌃⇧R)", () => {
    const state = run("modifiers", [
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "keydown", modifier: "shift", code: "ShiftLeft" },
      { type: "keydown", modifier: null, code: "KeyR" },
    ]);
    expect(state.result).toEqual({
      kind: "modifiers",
      mods: ["control", "shift"],
      key: "KeyR",
    });
    expect(state.error).toBeNull();
  });
});

describe("display helpers", () => {
  it("maps modifier codes to tokens (and non-modifiers to null)", () => {
    expect(modifierFromCode("ControlLeft")).toBe("control");
    expect(modifierFromCode("AltRight")).toBe("option");
    expect(modifierFromCode("MetaLeft")).toBe("command");
    expect(modifierFromCode("ShiftRight")).toBe("shift");
    expect(modifierFromCode("KeyV")).toBeNull();
  });

  it("renders key symbols", () => {
    expect(keySymbol("KeyV")).toBe("V");
    expect(keySymbol("Digit1")).toBe("1");
    expect(keySymbol("F13")).toBe("F13");
  });

  it("renders binding chips", () => {
    expect(
      dictationChips({ kind: "modifiers", mods: ["control", "option"] }),
    ).toEqual(["⌃", "⌥"]);
    expect(
      dictationChips({
        kind: "modifiers",
        mods: ["control", "shift"],
        key: "KeyR",
      }),
    ).toEqual(["⌃", "⇧", "R"]);
    expect(repasteChips({ mods: ["control", "command"], key: "KeyV" })).toEqual(
      ["⌃", "⌘", "V"],
    );
  });

  // Plan 039 task 54 — layout-aware display: an AZERTY-simulated capture
  // (physical code KeyQ, typed key "a") displays the typed letter "A", not
  // the QWERTY-shaped code glyph "Q".
  it("prefers displayKey over the code glyph when present", () => {
    expect(keyDisplaySymbol("KeyQ", "a")).toBe("A");
    expect(
      dictationChips({
        kind: "modifiers",
        mods: ["control"],
        key: "KeyQ",
        displayKey: "a",
      }),
    ).toEqual(["⌃", "A"]);
    expect(
      repasteChips({
        mods: ["control", "command"],
        key: "KeyQ",
        displayKey: "a",
      }),
    ).toEqual(["⌃", "⌘", "A"]);
  });

  it("falls back to the code glyph when displayKey is absent", () => {
    expect(keyDisplaySymbol("KeyQ")).toBe("Q");
  });

  it("passes a multi-character displayKey through unchanged", () => {
    expect(keyDisplaySymbol("Enter", "Enter")).toBe("Enter");
  });

  // `KeyboardEvent.key` for the spacebar is a literal " " — a naive
  // single-char uppercase would render an invisible glyph in the chip.
  it("falls back to the code glyph when displayKey is blank (Space anchor)", () => {
    expect(keyDisplaySymbol("Space", " ")).toBe("Space");
  });

  describe("isDisplayableKey (task 54 — untrustworthy key filter)", () => {
    it("rejects a dead-key composition value", () => {
      expect(isDisplayableKey("Dead", false)).toBe(false);
    });

    it("rejects a single-character glyph captured while Option is held", () => {
      // macOS remaps ⌥R → "®" on US layout — not the pressed key's label.
      expect(isDisplayableKey("®", true)).toBe(false);
    });

    it("accepts a single-character glyph when Option is not held", () => {
      expect(isDisplayableKey("a", false)).toBe(true);
    });

    it("accepts a multi-character key name even while Option is held", () => {
      expect(isDisplayableKey("ArrowUp", true)).toBe(true);
      expect(isDisplayableKey("Enter", true)).toBe(true);
    });
  });
});

// Plan 039 task 53 — anchor allowlist + composition guard.
describe("task 53 — anchor allowlist (mirrors Rust is_allowed_anchor_key)", () => {
  it("allows letters, digits, F-keys, navigation, punctuation, and Space/Tab/Enter", () => {
    for (const code of [
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
    ]) {
      expect(isAllowedAnchorKey(code)).toBe(true);
    }
  });

  it("rejects CapsLock/NumLock/ScrollLock/ContextMenu/media/IME codes", () => {
    for (const code of [
      "CapsLock",
      "NumLock",
      "ScrollLock",
      "ContextMenu",
      "MediaPlayPause",
      "AudioVolumeUp",
      "Lang1",
      "Convert",
    ]) {
      expect(isAllowedAnchorKey(code)).toBe(false);
    }
  });

  it("rejects an invalid anchor inline in modifiers mode, staying recording", () => {
    const state = run("modifiers", [
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "keydown", modifier: null, code: "CapsLock" },
    ]);
    expect(state.result).toBeNull();
    expect(state.recording).toBe(true);
    expect(state.error).toBe(BINDING_ERROR_COPY.invalidAnchorKey);
  });

  it("rejects an invalid anchor inline in combo mode", () => {
    const state = run("combo", [
      { type: "keydown", modifier: "control", code: "ControlLeft" },
      { type: "keydown", modifier: null, code: "MediaPlayPause" },
    ]);
    expect(state.result).toBeNull();
    expect(state.recording).toBe(true);
    expect(state.error).toBe(BINDING_ERROR_COPY.invalidAnchorKey);
  });

  it("rejects an invalid anchor even with no modifier held", () => {
    const state = run("modifiers", [
      { type: "keydown", modifier: null, code: "NumLock" },
    ]);
    expect(state.error).toBe(BINDING_ERROR_COPY.invalidAnchorKey);
  });
});

describe("task 53b — composition guard", () => {
  it("flags a keydown mid-IME-composition", () => {
    expect(isComposingKeydown({ isComposing: true })).toBe(true);
  });

  it("flags the legacy keyCode 229 fallback", () => {
    expect(isComposingKeydown({ isComposing: false, keyCode: 229 })).toBe(
      true,
    );
  });

  it("does not flag an ordinary keydown", () => {
    expect(isComposingKeydown({ isComposing: false, keyCode: 82 })).toBe(
      false,
    );
    expect(isComposingKeydown({})).toBe(false);
  });
});
