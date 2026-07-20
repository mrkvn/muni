// @vitest-environment jsdom
/**
 * Plan 039 task 59 — the Escape-key guard regression: Escape on a row with
 * ANY content must never delete it (only blur/cancel); Escape on a
 * still-blank draft row may remove it. Exercised against a small fake
 * two-field config wired through the real {@link useEditableRowList} +
 * {@link RowListEditor} pair (both panes are thin configuration over this
 * same combination, so this is the shared regression test for both).
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  useEditableRowList,
  type EditableRowListConfig,
} from "@/hooks/useEditableRowList";

import { RowListEditor, type RowFieldSpec } from "./RowListEditor";

type FakeEntry = { primary: string; secondary: string };

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

const CONFIG: EditableRowListConfig<FakeEntry> = {
  getCommand: "fake_get",
  saveCommand: "fake_save",
  fieldKeys: ["primary", "secondary"],
  emptyEntry: { primary: "", secondary: "" },
  primaryField: "primary",
  requiredFieldsForPrune: ["primary", "secondary"],
  trimOnSaveFields: ["primary"],
  loadErrorLabel: "fake entries",
};

const FIELDS: readonly [RowFieldSpec<FakeEntry>, RowFieldSpec<FakeEntry>] = [
  { key: "primary", ariaLabel: "Primary field", required: true },
  { key: "secondary", ariaLabel: "Secondary field", required: true },
];

function Harness() {
  const state = useEditableRowList(CONFIG);
  return (
    <RowListEditor
      state={state}
      fields={FIELDS}
      headingId="fake-heading"
      heading="Fake"
      subheading="A fake row list for testing"
      toggleId="fake-enabled"
      toggleLabel="Enable"
      columnLabels={["Primary", "Secondary"]}
      placeholders={[]}
      placeholderAriaLabel="Examples"
      deleteButtonAriaLabel="Delete row"
      incompleteMessage="Fill in both fields to save."
      savedToastMessage="Saved"
      routePath="/settings/fake"
    />
  );
}

function renderHarness() {
  return render(
    <MemoryRouter initialEntries={["/settings/fake"]}>
      <Harness />
    </MemoryRouter>,
  );
}

beforeEach(() => {
  invokeHandlers.clear();
  invokeCalls.length = 0;
  invokeHandlers.set("fake_get", () => ({ enabled: true, entries: [] }));
  invokeHandlers.set("fake_save", () => undefined);
});

afterEach(() => {
  cleanup();
  invokeHandlers.clear();
  invokeCalls.length = 0;
});

describe("RowListEditor — Escape guard", () => {
  it("removes a still-blank draft row on Escape", async () => {
    renderHarness();

    fireEvent.click(await screen.findByRole("button", { name: /^add$/i }));

    const primaryInput = await screen.findByLabelText("Primary field");
    fireEvent.keyDown(primaryInput, { key: "Escape" });

    await waitFor(() =>
      expect(screen.queryByLabelText("Primary field")).toBeNull(),
    );
  });

  it("never deletes a row with content on Escape (regression)", async () => {
    renderHarness();

    fireEvent.click(await screen.findByRole("button", { name: /^add$/i }));
    const primaryInput = (await screen.findByLabelText(
      "Primary field",
    )) as HTMLInputElement;

    fireEvent.change(primaryInput, { target: { value: "some content" } });
    fireEvent.keyDown(primaryInput, { key: "Escape" });

    // The row must still be present — Escape only backs out of editing.
    expect(screen.queryByLabelText("Primary field")).not.toBeNull();
    expect((screen.getByLabelText("Primary field") as HTMLInputElement).value).toBe(
      "some content",
    );
  });

  it("also preserves a row with only the SECOND field filled", async () => {
    renderHarness();

    fireEvent.click(await screen.findByRole("button", { name: /^add$/i }));
    const secondaryInput = (await screen.findByLabelText(
      "Secondary field",
    )) as HTMLInputElement;

    fireEvent.change(secondaryInput, { target: { value: "note only" } });
    fireEvent.keyDown(secondaryInput, { key: "Escape" });

    expect(screen.queryByLabelText("Secondary field")).not.toBeNull();
  });

  it("Escape still clears an active row-level error without deleting", async () => {
    renderHarness();

    fireEvent.click(await screen.findByRole("button", { name: /^add$/i }));
    const primaryInput = (await screen.findByLabelText(
      "Primary field",
    )) as HTMLInputElement;
    fireEvent.change(primaryInput, { target: { value: "filled" } });

    // Enter with `secondary` still blank raises the inline error.
    fireEvent.keyDown(primaryInput, { key: "Enter" });
    expect(await screen.findByRole("alert")).toBeTruthy();

    fireEvent.keyDown(primaryInput, { key: "Escape" });

    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.queryByLabelText("Primary field")).not.toBeNull();
  });
});

describe("RowListEditor — filter / search", () => {
  // A two-entry saved list so the filter box renders (it only appears once
  // the list has rows) and we can assert what shows vs. hides.
  function seedTwoRows() {
    invokeHandlers.set("fake_get", () => ({
      enabled: true,
      entries: [
        { primary: "cloud code", secondary: "Claude Code" },
        { primary: "mic test", secondary: "my test" },
      ],
    }));
  }

  it("hides non-matching rows and keeps matching ones (case-insensitive)", async () => {
    seedTwoRows();
    renderHarness();

    await screen.findByDisplayValue("cloud code");
    fireEvent.change(await screen.findByLabelText("Search list"), {
      target: { value: "CLAUDE" },
    });

    // Matches "cloud code / Claude Code" (second field), hides "mic test".
    expect(screen.queryByDisplayValue("cloud code")).not.toBeNull();
    expect(screen.queryByDisplayValue("mic test")).toBeNull();
  });

  it("shows the no-matches message when nothing matches", async () => {
    seedTwoRows();
    renderHarness();

    await screen.findByDisplayValue("cloud code");
    fireEvent.change(await screen.findByLabelText("Search list"), {
      target: { value: "zzznope" },
    });

    expect(screen.getByText("No matches.")).toBeTruthy();
    expect(screen.queryByDisplayValue("cloud code")).toBeNull();
  });

  it("does not drop hidden rows from the saved list — filtering is render-only", async () => {
    seedTwoRows();
    renderHarness();

    await screen.findByDisplayValue("cloud code");
    // Filter down to a single visible row, then edit it to make the list dirty.
    fireEvent.change(await screen.findByLabelText("Search list"), {
      target: { value: "mic" },
    });
    const visible = screen.getByDisplayValue("mic test") as HTMLInputElement;
    fireEvent.change(visible, { target: { value: "mic check" } });
    fireEvent.keyDown(visible, { key: "Enter" });

    await waitFor(() =>
      expect(invokeCalls.some((c) => c.name === "fake_save")).toBe(true),
    );
    // The saved payload must still contain the row that was filtered out of view.
    const saveCall = invokeCalls.find((c) => c.name === "fake_save");
    const snapshot = saveCall?.args?.snapshot as
      | { entries: { primary: string }[] }
      | undefined;
    const entries = snapshot?.entries ?? [];
    expect(entries.map((e) => e.primary)).toContain("cloud code");
    expect(entries.map((e) => e.primary)).toContain("mic check");
  });

  it("Cmd+F focuses the filter box", async () => {
    seedTwoRows();
    renderHarness();

    await screen.findByDisplayValue("cloud code");
    fireEvent.keyDown(window, { key: "f", metaKey: true });

    expect(document.activeElement).toBe(screen.getByLabelText("Search list"));
  });

  it("Escape in the filter box clears the filter", async () => {
    seedTwoRows();
    renderHarness();

    await screen.findByDisplayValue("cloud code");
    const search = (await screen.findByLabelText(
      "Search list",
    )) as HTMLInputElement;
    fireEvent.change(search, { target: { value: "mic" } });
    expect(screen.queryByDisplayValue("cloud code")).toBeNull();

    fireEvent.keyDown(search, { key: "Escape" });

    expect(search.value).toBe("");
    expect(screen.queryByDisplayValue("cloud code")).not.toBeNull();
  });

  it("adding a row clears an active filter so the new row is visible", async () => {
    seedTwoRows();
    renderHarness();

    await screen.findByDisplayValue("cloud code");
    const search = (await screen.findByLabelText(
      "Search list",
    )) as HTMLInputElement;
    fireEvent.change(search, { target: { value: "mic" } });
    fireEvent.click(screen.getByRole("button", { name: /^add$/i }));

    // Filter cleared → previously-hidden row is back, and the new blank row
    // (an empty primary field) is present and focusable.
    expect(search.value).toBe("");
    expect(screen.queryByDisplayValue("cloud code")).not.toBeNull();
  });
});

describe("RowListEditor — Enter-to-save", () => {
  it("saves when Enter is pressed and every required field is filled", async () => {
    renderHarness();

    fireEvent.click(await screen.findByRole("button", { name: /^add$/i }));
    const primaryInput = (await screen.findByLabelText(
      "Primary field",
    )) as HTMLInputElement;
    const secondaryInput = (await screen.findByLabelText(
      "Secondary field",
    )) as HTMLInputElement;

    fireEvent.change(primaryInput, { target: { value: "trigger" } });
    fireEvent.change(secondaryInput, { target: { value: "replacement" } });
    fireEvent.keyDown(secondaryInput, { key: "Enter" });

    await waitFor(() =>
      expect(invokeCalls.some((c) => c.name === "fake_save")).toBe(true),
    );
  });
});
