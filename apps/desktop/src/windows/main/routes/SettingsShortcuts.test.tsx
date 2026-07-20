// @vitest-environment jsdom
/**
 * Plan 039 task 56 — route test for `SettingsShortcuts`: both the dictation
 * and re-paste recorders render, seeded from `settings_get`, and persist a
 * newly-captured chord through the typed `set_*_hotkey` commands.
 */
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { SettingsShortcuts } from "./SettingsShortcuts";

type InvokeCall = { name: string; args?: Record<string, unknown> };
const invokeCalls: InvokeCall[] = [];

const SETTINGS: Record<string, unknown> = {
  "hotkey.dictation_binding": { kind: "modifiers", mods: ["control", "option"] },
  "hotkey.repaste_binding": { mods: ["control", "command"], key: "KeyV" },
};

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (name: string, args?: Record<string, unknown>) => {
    invokeCalls.push({ name, args });
    if (name === "settings_get") {
      const key = (args as { key: string }).key;
      return SETTINGS[key] ?? null;
    }
    if (name === "set_dictation_hotkey" || name === "set_repaste_hotkey") {
      return undefined;
    }
    return undefined;
  },
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: async () => () => {},
}));

vi.mock("sonner", () => ({
  toast: { error: vi.fn(), success: vi.fn() },
}));

beforeEach(() => {
  invokeCalls.length = 0;
});

afterEach(() => {
  cleanup();
});

describe("SettingsShortcuts route", () => {
  it("renders both the dictation and re-paste recorders, seeded from settings", async () => {
    render(<SettingsShortcuts />);

    expect(
      await screen.findByRole("button", { name: /dictation shortcut/i }),
    ).toBeTruthy();
    expect(
      screen.getByRole("button", { name: /re-paste shortcut/i }),
    ).toBeTruthy();
    // Seeded from settings_get, not the shipped defaults.
    expect(
      screen.getByRole("button", { name: /currently control option/i }),
    ).toBeTruthy();
  });

  it("persists a newly-recorded dictation chord via set_dictation_hotkey", async () => {
    render(<SettingsShortcuts />);
    const dictationButton = await screen.findByRole("button", {
      name: /dictation shortcut/i,
    });

    await userEvent.click(dictationButton);
    fireEvent.keyDown(dictationButton, { code: "ControlLeft" });
    fireEvent.keyDown(dictationButton, { code: "ShiftLeft" });
    fireEvent.keyUp(dictationButton, { code: "ShiftLeft" });

    const setCall = await vi.waitUntil(() =>
      invokeCalls.find((c) => c.name === "set_dictation_hotkey"),
    );
    expect(setCall?.args).toEqual({
      binding: { kind: "modifiers", mods: ["control", "shift"] },
    });
  });
});
