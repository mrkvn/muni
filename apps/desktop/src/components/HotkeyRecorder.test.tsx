// @vitest-environment jsdom
/**
 * Plan 039 task 56 — component tests for {@link HotkeyRecorder}: the
 * `hotkey_set_recording` invoke bracketing around a recording session (the
 * "A2 class" of suppress/resume pairing bugs), Escape/blur cancel, keydown
 * repeat filtering, single-fire commit, focus retention after commit
 * (task 55), and the rejected-anchor-key inline error path (task 53a).
 */
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { DictationBinding, RepasteBinding } from "@/hooks/useSettings";

import { HotkeyRecorder } from "./HotkeyRecorder";

type InvokeCall = { name: string; args?: Record<string, unknown> };
const invokeCalls: InvokeCall[] = [];

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (name: string, args?: Record<string, unknown>) => {
    invokeCalls.push({ name, args });
    return undefined;
  },
}));

beforeEach(() => {
  invokeCalls.length = 0;
});

afterEach(() => {
  cleanup();
});

/** The `hotkey_set_recording` calls only, in call order. */
function recordingInvokes() {
  return invokeCalls
    .filter((c) => c.name === "hotkey_set_recording")
    .map((c) => c.args);
}

/** The recorder button — matched by name so it's stable whether or not the
 * mode also renders a "Clear" button (combo mode) alongside it. */
function getRecorderButton() {
  return screen.getByRole("button", { name: /shortcut/i });
}

describe("HotkeyRecorder — modifiers mode", () => {
  const emptyDictation: DictationBinding = { kind: "modifiers", mods: [] };

  it("brackets hotkey_set_recording true→false around a committed chord", async () => {
    const onChange = vi.fn();
    render(
      <HotkeyRecorder
        mode="modifiers"
        value={emptyDictation}
        onChange={onChange}
      />,
    );

    await userEvent.click(getRecorderButton());
    const button = getRecorderButton();
    fireEvent.keyDown(button, { code: "ControlLeft" });
    fireEvent.keyDown(button, { code: "AltLeft" });
    fireEvent.keyUp(button, { code: "AltLeft" });

    expect(onChange).toHaveBeenCalledWith({
      kind: "modifiers",
      mods: ["control", "option"],
    });
    expect(recordingInvokes()).toEqual([
      { recording: true },
      { recording: false },
    ]);
  });

  it("cancels on Escape without committing, still resuming the trigger", async () => {
    const onChange = vi.fn();
    render(
      <HotkeyRecorder
        mode="modifiers"
        value={emptyDictation}
        onChange={onChange}
      />,
    );

    await userEvent.click(getRecorderButton());
    const button = getRecorderButton();
    fireEvent.keyDown(button, { code: "ControlLeft" });
    fireEvent.keyDown(button, { code: "Escape" });

    expect(onChange).not.toHaveBeenCalled();
    expect(recordingInvokes()).toEqual([
      { recording: true },
      { recording: false },
    ]);
  });

  it("cancels on blur without committing", async () => {
    const onChange = vi.fn();
    render(
      <HotkeyRecorder
        mode="modifiers"
        value={emptyDictation}
        onChange={onChange}
      />,
    );

    await userEvent.click(getRecorderButton());
    const button = getRecorderButton();
    fireEvent.keyDown(button, { code: "ControlLeft" });
    fireEvent.blur(button);

    expect(onChange).not.toHaveBeenCalled();
    const invokes = recordingInvokes();
    expect(invokes[invokes.length - 1]).toEqual({ recording: false });
  });

  it("filters a repeated (auto-repeat) keydown without corrupting the chord", async () => {
    const onChange = vi.fn();
    render(
      <HotkeyRecorder
        mode="modifiers"
        value={emptyDictation}
        onChange={onChange}
      />,
    );

    await userEvent.click(getRecorderButton());
    const button = getRecorderButton();
    fireEvent.keyDown(button, { code: "ControlLeft" });
    // Auto-repeat of the still-held Control key must be a no-op, not a
    // duplicate accumulation into `held`.
    fireEvent.keyDown(button, { code: "ControlLeft", repeat: true });
    fireEvent.keyDown(button, { code: "ControlLeft", repeat: true });
    fireEvent.keyDown(button, { code: "AltLeft" });
    fireEvent.keyUp(button, { code: "AltLeft" });

    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange).toHaveBeenCalledWith({
      kind: "modifiers",
      mods: ["control", "option"],
    });
  });

  it("commits exactly once per recording session", async () => {
    const onChange = vi.fn();
    render(
      <HotkeyRecorder
        mode="modifiers"
        value={emptyDictation}
        onChange={onChange}
      />,
    );

    await userEvent.click(getRecorderButton());
    const button = getRecorderButton();
    fireEvent.keyDown(button, { code: "ControlLeft" });
    fireEvent.keyDown(button, { code: "AltLeft" });
    fireEvent.keyUp(button, { code: "AltLeft" });

    expect(onChange).toHaveBeenCalledTimes(1);
  });

  it("keeps focus on the button after a commit (task 55)", async () => {
    const onChange = vi.fn();
    render(
      <HotkeyRecorder
        mode="modifiers"
        value={emptyDictation}
        onChange={onChange}
      />,
    );

    await userEvent.click(getRecorderButton());
    const button = getRecorderButton();
    fireEvent.keyDown(button, { code: "ControlLeft" });
    fireEvent.keyDown(button, { code: "AltLeft" });
    fireEvent.keyUp(button, { code: "AltLeft" });

    expect(document.activeElement).toBe(button);
  });

  it("announces the saved chord in the status live region (task 55)", async () => {
    const onChange = vi.fn();
    render(
      <HotkeyRecorder
        mode="modifiers"
        value={emptyDictation}
        onChange={onChange}
      />,
    );

    await userEvent.click(getRecorderButton());
    const button = getRecorderButton();
    fireEvent.keyDown(button, { code: "ControlLeft" });
    fireEvent.keyDown(button, { code: "AltLeft" });
    fireEvent.keyUp(button, { code: "AltLeft" });

    const status = await screen.findByRole("status");
    expect(status.textContent).toContain("Saved: ⌃⌥");
  });

  it("clears the saved confirmation when the pane is hidden (tab switch)", async () => {
    const onChange = vi.fn();
    const { rerender } = render(
      <HotkeyRecorder
        mode="modifiers"
        visible
        value={emptyDictation}
        onChange={onChange}
      />,
    );

    await userEvent.click(getRecorderButton());
    const button = getRecorderButton();
    fireEvent.keyDown(button, { code: "ControlLeft" });
    fireEvent.keyDown(button, { code: "AltLeft" });
    fireEvent.keyUp(button, { code: "AltLeft" });
    expect(screen.getByRole("status").textContent).toContain("Saved: ⌃⌥");

    // The pane stays mounted across tab switches; `visible` flipping false is
    // the only signal that clears the transient receipt.
    rerender(
      <HotkeyRecorder
        mode="modifiers"
        visible={false}
        value={emptyDictation}
        onChange={onChange}
      />,
    );
    expect(screen.getByRole("status").textContent).not.toContain("Saved");
  });

  it("clears the saved confirmation when the app loses focus (window blur)", async () => {
    const onChange = vi.fn();
    render(
      <HotkeyRecorder
        mode="modifiers"
        value={emptyDictation}
        onChange={onChange}
      />,
    );

    await userEvent.click(getRecorderButton());
    const button = getRecorderButton();
    fireEvent.keyDown(button, { code: "ControlLeft" });
    fireEvent.keyDown(button, { code: "AltLeft" });
    fireEvent.keyUp(button, { code: "AltLeft" });
    expect(screen.getByRole("status").textContent).toContain("Saved: ⌃⌥");

    fireEvent.blur(window);
    expect(screen.getByRole("status").textContent).not.toContain("Saved");
  });

  it("shows the inline error for a rejected anchor key and does not commit (task 53a)", async () => {
    const onChange = vi.fn();
    render(
      <HotkeyRecorder
        mode="modifiers"
        value={emptyDictation}
        onChange={onChange}
      />,
    );

    await userEvent.click(getRecorderButton());
    const button = getRecorderButton();
    fireEvent.keyDown(button, { code: "ControlLeft" });
    fireEvent.keyDown(button, { code: "CapsLock" });

    expect(onChange).not.toHaveBeenCalled();
    expect(screen.getByRole("status").textContent).toContain(
      "That key can't be used.",
    );
    expect(button.getAttribute("aria-invalid")).toBe("true");
    // Still recording — the user gets to retry without re-opening the recorder.
    expect(button.getAttribute("aria-pressed")).toBe("true");
  });
});

describe("HotkeyRecorder — combo mode", () => {
  it("commits a modifier+key combo, carrying the display-key hint (task 54)", async () => {
    const onChange = vi.fn<(binding: RepasteBinding | null) => void>();
    render(<HotkeyRecorder mode="combo" value={null} onChange={onChange} />);

    await userEvent.click(getRecorderButton());
    const button = getRecorderButton();
    fireEvent.keyDown(button, { code: "ControlLeft" });
    fireEvent.keyDown(button, { code: "MetaLeft" });
    fireEvent.keyDown(button, { code: "KeyV", key: "v" });

    expect(onChange).toHaveBeenCalledWith({
      mods: ["control", "command"],
      key: "KeyV",
      displayKey: "v",
    });
  });

  it("rejects the reserved ⌘V paste combo inline", async () => {
    const onChange = vi.fn();
    render(<HotkeyRecorder mode="combo" value={null} onChange={onChange} />);

    await userEvent.click(getRecorderButton());
    const button = getRecorderButton();
    fireEvent.keyDown(button, { code: "MetaLeft" });
    fireEvent.keyDown(button, { code: "KeyV", key: "v" });

    expect(onChange).not.toHaveBeenCalled();
    expect(screen.getByRole("status").textContent).toContain("Can't use ⌘V.");
  });
});
