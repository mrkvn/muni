// @vitest-environment jsdom
/**
 * Plan 039 task 61(c) — `remove`/`wipe`'s optimistic-update rollback must be
 * skipped if a fresher server snapshot (from a `history://changed`-triggered
 * `refresh()`) has already landed while the mutating `invoke` was still in
 * flight — otherwise that race would silently undo real, newer data with a
 * stale pre-optimistic snapshot.
 */
import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useHistory, type DictationRecord } from "./useHistory";

type InvokeHandler = (args?: Record<string, unknown>) => unknown | Promise<unknown>;
type EventHandler = (event: { event: string; payload: unknown }) => void;

const invokeHandlers = new Map<string, InvokeHandler>();
const eventHandlers = new Map<string, Set<EventHandler>>();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (name: string, args?: Record<string, unknown>) => {
    const handler = invokeHandlers.get(name);
    if (!handler) throw new Error(`unmocked invoke("${name}")`);
    return handler(args);
  },
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: async (event: string, handler: EventHandler) => {
    const set = eventHandlers.get(event) ?? new Set();
    set.add(handler);
    eventHandlers.set(event, set);
    return () => {
      set.delete(handler);
    };
  },
}));

function emit(event: string, payload?: unknown) {
  const set = eventHandlers.get(event);
  if (!set) return;
  for (const handler of set) handler({ event, payload });
}

function record(id: number): DictationRecord {
  return {
    id,
    createdAt: 1_700_000_000,
    rawText: `raw ${id}`,
    cleanedText: `clean ${id}`,
    targetAppBundleId: null,
    charCount: 7,
  };
}

beforeEach(() => {
  invokeHandlers.clear();
  eventHandlers.clear();
});

afterEach(() => {
  invokeHandlers.clear();
  eventHandlers.clear();
});

describe("useHistory — optimistic rollback race (task 61c)", () => {
  it("rolls back a failed remove when NO fresher snapshot has landed", async () => {
    invokeHandlers.set("history_list", () => [record(1), record(2)]);
    invokeHandlers.set("history_delete", () => {
      throw new Error("db locked");
    });

    const { result } = renderHook(() => useHistory());
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.records).toHaveLength(2);

    await act(async () => {
      await expect(result.current.remove(1)).rejects.toThrow();
    });

    // No refresh raced this — the pre-optimistic snapshot is restored.
    expect(result.current.records.map((r) => r.id)).toEqual([1, 2]);
  });

  it("does NOT roll back when a newer refresh landed while the delete was in flight", async () => {
    invokeHandlers.set("history_list", () => [record(1), record(2)]);

    const { result } = renderHook(() => useHistory());
    await waitFor(() => expect(result.current.loading).toBe(false));

    // The delete's invoke() call hangs until we release it below.
    let releaseDelete: ((err: unknown) => void) | null = null;
    invokeHandlers.set(
      "history_delete",
      () =>
        new Promise((_resolve, reject) => {
          releaseDelete = reject;
        }),
    );

    let removePromise: Promise<void> | undefined;
    act(() => {
      removePromise = result.current.remove(1);
    });
    // Optimistic removal already applied.
    expect(result.current.records.map((r) => r.id)).toEqual([2]);

    // A NEW dictation lands server-side and broadcasts `history://changed`
    // while the delete is still pending — its refresh reflects reality as
    // of THAT moment (record 1 already gone server-side, record 3 newly
    // arrived): a genuinely newer, authoritative snapshot.
    invokeHandlers.set("history_list", () => [record(2), record(3)]);
    await act(async () => {
      emit("history://changed");
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(result.current.records.map((r) => r.id)).toEqual([2, 3]);

    // Now the stale delete call finally rejects.
    await act(async () => {
      releaseDelete?.(new Error("timed out"));
      await expect(removePromise).rejects.toThrow();
    });

    // Must NOT have rolled back to the pre-optimistic `[1, 2]` snapshot —
    // that would resurrect record 1 and drop the newly-arrived record 3.
    expect(result.current.records.map((r) => r.id)).toEqual([2, 3]);
    expect(result.current.error).toMatch(/timed out|db|delete/i);
  });

  it("wipe has the same race-safe rollback behavior", async () => {
    invokeHandlers.set("history_list", () => [record(1)]);
    const { result } = renderHook(() => useHistory());
    await waitFor(() => expect(result.current.loading).toBe(false));

    let releaseWipe: ((err: unknown) => void) | null = null;
    invokeHandlers.set(
      "history_wipe",
      () =>
        new Promise((_resolve, reject) => {
          releaseWipe = reject;
        }),
    );

    let wipePromise: Promise<void> | undefined;
    act(() => {
      wipePromise = result.current.wipe();
    });
    expect(result.current.records).toHaveLength(0);

    invokeHandlers.set("history_list", () => [record(2)]);
    await act(async () => {
      emit("history://changed");
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(result.current.records.map((r) => r.id)).toEqual([2]);

    await act(async () => {
      releaseWipe?.(new Error("failed"));
      await expect(wipePromise).rejects.toThrow();
    });

    expect(result.current.records.map((r) => r.id)).toEqual([2]);
  });
});
