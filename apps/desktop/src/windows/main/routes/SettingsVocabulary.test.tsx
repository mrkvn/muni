// @vitest-environment jsdom
/**
 * Plan 039 task 59 — smoke test: `SettingsVocabulary` renders through the
 * shared `useEditableRowList` + `RowListEditor` pair with its real
 * Vocabulary IPC command names and field config (note optional, both
 * fields trimmed on save). Detailed behavior is covered generically in
 * `useEditableRowList.test.ts` / `RowListEditor.test.tsx`.
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, useNavigate } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { SettingsVocabulary } from "./SettingsVocabulary";

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
    <MemoryRouter initialEntries={["/settings/vocabulary"]}>
      <SettingsVocabulary />
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

describe("SettingsVocabulary", () => {
  it("renders the heading and loads via the correct IPC commands", async () => {
    invokeHandlers.set("vocabulary_get", () => ({
      enabled: true,
      entries: [{ term: "Acme", note: "my company" }],
    }));

    renderPane();

    expect(screen.getByRole("heading", { name: "Vocabulary" })).toBeTruthy();
    await waitFor(() =>
      expect(invokeCalls.some((c) => c.name === "vocabulary_get")).toBe(true),
    );
    await waitFor(() => expect(screen.getByDisplayValue("Acme")).toBeTruthy());
  });

  it("prunes an empty-term draft on tab leave but keeps a term-only row", async () => {
    // Vocabulary's `requiredFieldsForPrune` is `["term"]` only — `note` is
    // intentionally optional (unlike My Words, which requires both sides).
    invokeHandlers.set("vocabulary_get", () => ({
      enabled: true,
      entries: [{ term: "Jiro", note: "" }],
    }));

    function Harness() {
      const navigate = useNavigate();
      return (
        <>
          <button onClick={() => navigate("/settings/general")}>leave</button>
          <SettingsVocabulary />
        </>
      );
    }

    render(
      <MemoryRouter initialEntries={["/settings/vocabulary"]}>
        <Harness />
      </MemoryRouter>,
    );

    await waitFor(() => expect(screen.getByDisplayValue("Jiro")).toBeTruthy());

    // Add a second, still-blank draft row.
    fireEvent.click(screen.getByRole("button", { name: /^add$/i }));
    await waitFor(() => expect(screen.getAllByLabelText("Term")).toHaveLength(2));

    // Leaving the tab prunes the blank draft...
    fireEvent.click(screen.getByRole("button", { name: "leave" }));
    await waitFor(() => expect(screen.getAllByLabelText("Term")).toHaveLength(1));

    // ...but the pre-existing term-only (empty note) row survives.
    expect(screen.getByDisplayValue("Jiro")).toBeTruthy();
  });
});
