// @vitest-environment jsdom
import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  useEditableRowList,
  type EditableRowListConfig,
} from "./useEditableRowList";

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

// Two-field entry shape mirroring `MyWordsEntry`/`VocabularyEntry`.
type FakeEntry = { primary: string; secondary: string };

const EMPTY_ENTRY: FakeEntry = { primary: "", secondary: "" };

/** Mirrors My Words' config: BOTH fields required to survive prune, but
 * only `primary` gates the save-time cleanup filter, and only `primary`
 * is trimmed on save (asymmetric — and load-bearing: this pins the
 * task-59 "comment says canonical, code doesn't" fix). */
const BOTH_REQUIRED_CONFIG: EditableRowListConfig<FakeEntry> = {
  getCommand: "fake_get",
  saveCommand: "fake_save",
  fieldKeys: ["primary", "secondary"],
  emptyEntry: EMPTY_ENTRY,
  primaryField: "primary",
  requiredFieldsForPrune: ["primary", "secondary"],
  trimOnSaveFields: ["primary"],
  loadErrorLabel: "fake entries",
};

/** Mirrors Vocabulary's config: only `primary` required anywhere, both
 * fields trimmed on save. */
const ONLY_PRIMARY_REQUIRED_CONFIG: EditableRowListConfig<FakeEntry> = {
  ...BOTH_REQUIRED_CONFIG,
  requiredFieldsForPrune: ["primary"],
  trimOnSaveFields: ["primary", "secondary"],
};

beforeEach(() => {
  invokeHandlers.clear();
  invokeCalls.length = 0;
});

afterEach(() => {
  invokeHandlers.clear();
  invokeCalls.length = 0;
});

describe("useEditableRowList", () => {
  it("loads the snapshot and re-keys entries with client-only ids", async () => {
    invokeHandlers.set("fake_get", () => ({
      enabled: false,
      entries: [{ primary: "a", secondary: "b" }],
    }));

    const { result } = renderHook(() =>
      useEditableRowList(BOTH_REQUIRED_CONFIG),
    );

    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.enabled).toBe(false);
    expect(result.current.rows).toHaveLength(1);
    expect(result.current.rows[0]).toMatchObject({
      primary: "a",
      secondary: "b",
    });
    expect(typeof result.current.rows[0]?.id).toBe("string");
  });

  it("surfaces a load error", async () => {
    invokeHandlers.set("fake_get", () => {
      throw new Error("db down");
    });

    const { result } = renderHook(() =>
      useEditableRowList(BOTH_REQUIRED_CONFIG),
    );

    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.loadError).toMatch(/Failed to load fake entries/);
  });

  it("tracks dirty across add/update/remove and clears after a matching revert", async () => {
    invokeHandlers.set("fake_get", () => ({ enabled: true, entries: [] }));

    const { result } = renderHook(() =>
      useEditableRowList(BOTH_REQUIRED_CONFIG),
    );
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.dirty).toBe(false);

    let id = "";
    act(() => {
      id = result.current.addRow();
    });
    expect(result.current.dirty).toBe(true);

    act(() => result.current.removeRow(id));
    expect(result.current.dirty).toBe(false);
  });

  it("revert restores the last-saved rows and drops in-progress edits", async () => {
    invokeHandlers.set("fake_get", () => ({
      enabled: true,
      entries: [{ primary: "orig", secondary: "orig-2" }],
    }));

    const { result } = renderHook(() =>
      useEditableRowList(BOTH_REQUIRED_CONFIG),
    );
    await waitFor(() => expect(result.current.loading).toBe(false));

    const rowId = result.current.rows[0]?.id as string;
    act(() => result.current.updateRow(rowId, { primary: "edited" }));
    expect(result.current.dirty).toBe(true);

    act(() => result.current.revert());
    expect(result.current.dirty).toBe(false);
    expect(result.current.rows[0]?.primary).toBe("orig");
  });

  it("pruneIncompleteRows requires ALL configured fields when both are required", async () => {
    invokeHandlers.set("fake_get", () => ({ enabled: true, entries: [] }));

    const { result } = renderHook(() =>
      useEditableRowList(BOTH_REQUIRED_CONFIG),
    );
    await waitFor(() => expect(result.current.loading).toBe(false));

    let id = "";
    act(() => {
      id = result.current.addRow();
    });
    act(() => result.current.updateRow(id, { primary: "filled" })); // secondary still blank

    act(() => result.current.pruneIncompleteRows());
    // Missing `secondary` — pruned under the both-required config.
    expect(result.current.rows).toHaveLength(0);
  });

  it("pruneIncompleteRows only requires the primary field under the vocabulary-style config", async () => {
    invokeHandlers.set("fake_get", () => ({ enabled: true, entries: [] }));

    const { result } = renderHook(() =>
      useEditableRowList(ONLY_PRIMARY_REQUIRED_CONFIG),
    );
    await waitFor(() => expect(result.current.loading).toBe(false));

    let id = "";
    act(() => {
      id = result.current.addRow();
    });
    act(() => result.current.updateRow(id, { primary: "filled" })); // secondary blank, OK here

    act(() => result.current.pruneIncompleteRows());
    expect(result.current.rows).toHaveLength(1);
  });

  it("save drops rows missing the primary field and persists the rest", async () => {
    invokeHandlers.set("fake_get", () => ({ enabled: true, entries: [] }));
    invokeHandlers.set("fake_save", () => undefined);

    const { result } = renderHook(() =>
      useEditableRowList(BOTH_REQUIRED_CONFIG),
    );
    await waitFor(() => expect(result.current.loading).toBe(false));

    let keptId = "";
    let droppedId = "";
    act(() => {
      keptId = result.current.addRow();
    });
    act(() => result.current.updateRow(keptId, { primary: "keep", secondary: "x" }));
    act(() => {
      droppedId = result.current.addRow();
    });
    act(() => result.current.updateRow(droppedId, { secondary: "orphaned" })); // primary blank

    await act(async () => {
      await result.current.save();
    });

    expect(result.current.rows).toHaveLength(1);
    expect(result.current.rows[0]?.primary).toBe("keep");
    const saveCall = invokeCalls.find((c) => c.name === "fake_save");
    expect(saveCall?.args).toEqual({
      snapshot: { enabled: true, entries: [{ primary: "keep", secondary: "x" }] },
    });
  });

  it("trims only the configured fields on save — the task-59 fix", async () => {
    invokeHandlers.set("fake_get", () => ({ enabled: true, entries: [] }));
    invokeHandlers.set("fake_save", () => undefined);

    const { result } = renderHook(() =>
      useEditableRowList(BOTH_REQUIRED_CONFIG), // trims only `primary`
    );
    await waitFor(() => expect(result.current.loading).toBe(false));

    let id = "";
    act(() => {
      id = result.current.addRow();
    });
    act(() =>
      result.current.updateRow(id, {
        primary: "  trigger  ",
        secondary: "  kept verbatim  ",
      }),
    );

    await act(async () => {
      await result.current.save();
    });

    // `primary` trimmed...
    expect(result.current.rows[0]?.primary).toBe("trigger");
    // ...but `secondary` (this config's untrimmed field, mirroring My
    // Words' `replacement`) survives verbatim, whitespace and all.
    expect(result.current.rows[0]?.secondary).toBe("  kept verbatim  ");
  });

  it("trims BOTH fields on save under the vocabulary-style config", async () => {
    invokeHandlers.set("fake_get", () => ({ enabled: true, entries: [] }));
    invokeHandlers.set("fake_save", () => undefined);

    const { result } = renderHook(() =>
      useEditableRowList(ONLY_PRIMARY_REQUIRED_CONFIG),
    );
    await waitFor(() => expect(result.current.loading).toBe(false));

    let id = "";
    act(() => {
      id = result.current.addRow();
    });
    act(() =>
      result.current.updateRow(id, {
        primary: "  term  ",
        secondary: "  note  ",
      }),
    );

    await act(async () => {
      await result.current.save();
    });

    expect(result.current.rows[0]?.primary).toBe("term");
    expect(result.current.rows[0]?.secondary).toBe("note");
  });

  it("surfaces a save error without discarding the in-progress rows", async () => {
    invokeHandlers.set("fake_get", () => ({ enabled: true, entries: [] }));
    invokeHandlers.set("fake_save", () => {
      throw new Error("network down");
    });

    const { result } = renderHook(() =>
      useEditableRowList(BOTH_REQUIRED_CONFIG),
    );
    await waitFor(() => expect(result.current.loading).toBe(false));

    let id = "";
    act(() => {
      id = result.current.addRow();
    });
    act(() => result.current.updateRow(id, { primary: "x", secondary: "y" }));

    await act(async () => {
      await expect(result.current.save()).rejects.toThrow();
    });

    expect(result.current.saveError).toMatch(/Save failed/);
    expect(result.current.rows[0]?.primary).toBe("x");
  });

  it("setEnabled persists immediately and rolls back on failure", async () => {
    invokeHandlers.set("fake_get", () => ({ enabled: true, entries: [] }));
    invokeHandlers.set("fake_save", () => {
      throw new Error("locked");
    });

    const { result } = renderHook(() =>
      useEditableRowList(BOTH_REQUIRED_CONFIG),
    );
    await waitFor(() => expect(result.current.loading).toBe(false));

    await act(async () => {
      await expect(result.current.setEnabled(false)).rejects.toThrow();
    });

    // Optimistic flip is rolled back on failure.
    expect(result.current.enabled).toBe(true);
    expect(result.current.saveError).toMatch(/Failed to update toggle/);
  });
});
