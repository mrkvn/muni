// @vitest-environment jsdom
/**
 * Plan 039 task 59 — smoke test: `SettingsMyWords` renders through the
 * shared `useEditableRowList` + `RowListEditor` pair with its real My
 * Words IPC command names and field config. Detailed behavior (dirty
 * tracking, Escape guard, trim-on-save) is covered generically in
 * `useEditableRowList.test.ts` / `RowListEditor.test.tsx`.
 */
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { SettingsMyWords } from "./SettingsMyWords";

type InvokeHandler = (args?: Record<string, unknown>) => unknown | Promise<unknown>;
const invokeHandlers = new Map<string, InvokeHandler>();
const invokeCalls: { name: string; args?: Record<string, unknown> }[] = [];

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (name: string, args?: Record<string, unknown>) => {
    invokeCalls.push({ name, args });
    const handler = invokeHandlers.get(name);
    if (!handler) throw new Error(`unmocked invoke("${name}")`);
    return handler(args);
  },
}));

vi.mock("sonner", () => ({
  toast: { error: vi.fn(), success: vi.fn() },
}));

function renderPane() {
  return render(
    <MemoryRouter initialEntries={["/settings/my-words"]}>
      <SettingsMyWords />
    </MemoryRouter>,
  );
}

beforeEach(() => {
  invokeHandlers.clear();
  invokeCalls.length = 0;
});

afterEach(() => {
  cleanup();
  invokeHandlers.clear();
  invokeCalls.length = 0;
});

describe("SettingsMyWords", () => {
  it("renders the heading and loads via the correct IPC commands", async () => {
    invokeHandlers.set("my_words_get", () => ({
      enabled: true,
      entries: [{ trigger: "cloud code", replacement: "Claude Code" }],
    }));

    renderPane();

    expect(screen.getByRole("heading", { name: "Substitutions" })).toBeTruthy();
    await waitFor(() =>
      expect(invokeCalls.some((c) => c.name === "my_words_get")).toBe(true),
    );
    await waitFor(() =>
      expect(screen.getByDisplayValue("cloud code")).toBeTruthy(),
    );
  });

  it("shows the placeholder example row when the list is empty", async () => {
    invokeHandlers.set("my_words_get", () => ({ enabled: true, entries: [] }));

    renderPane();

    await waitFor(() =>
      expect(screen.getByLabelText("Example rules (not saved)")).toBeTruthy(),
    );
  });
});
