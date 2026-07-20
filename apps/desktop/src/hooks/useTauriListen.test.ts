// @vitest-environment jsdom
import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useTauriListen } from "./useTauriListen";

type EventHandler = (event: { event: string; payload: unknown }) => void;

const eventHandlers = new Map<string, Set<EventHandler>>();
let listenShouldReject = false;

vi.mock("@tauri-apps/api/event", () => ({
  listen: async (event: string, handler: EventHandler) => {
    if (listenShouldReject) throw new Error("no bridge");
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

beforeEach(() => {
  eventHandlers.clear();
  listenShouldReject = false;
});

afterEach(() => {
  eventHandlers.clear();
});

describe("useTauriListen", () => {
  it("subscribes on mount and delivers payloads to the handler", async () => {
    const handler = vi.fn();
    renderHook(() => useTauriListen<string>("demo://event", handler));

    await waitFor(() => expect(eventHandlers.has("demo://event")).toBe(true));

    act(() => emit("demo://event", "hello"));

    expect(handler).toHaveBeenCalledWith("hello");
  });

  it("unsubscribes on unmount", async () => {
    const handler = vi.fn();
    const { unmount } = renderHook(() =>
      useTauriListen<string>("demo://event", handler),
    );

    await waitFor(() =>
      expect(eventHandlers.get("demo://event")?.size ?? 0).toBe(1),
    );

    unmount();

    expect(eventHandlers.get("demo://event")?.size ?? 0).toBe(0);
  });

  it("always calls the LATEST handler even if it wasn't memoized", async () => {
    const first = vi.fn();
    const second = vi.fn();
    const { rerender } = renderHook(
      ({ handler }: { handler: (p: string) => void }) =>
        useTauriListen<string>("demo://event", handler),
      { initialProps: { handler: first } },
    );

    await waitFor(() => expect(eventHandlers.has("demo://event")).toBe(true));

    // A fresh inline handler on every render (no useCallback) must still be
    // the one invoked — this is the whole point of the ref indirection.
    rerender({ handler: second });

    act(() => emit("demo://event", "payload"));

    expect(first).not.toHaveBeenCalled();
    expect(second).toHaveBeenCalledWith("payload");
  });

  it("re-subscribes when the event name changes", async () => {
    const handler = vi.fn();
    const { rerender } = renderHook(
      ({ event }: { event: string }) => useTauriListen<string>(event, handler),
      { initialProps: { event: "demo://one" } },
    );

    await waitFor(() => expect(eventHandlers.has("demo://one")).toBe(true));

    rerender({ event: "demo://two" });

    await waitFor(() =>
      expect(eventHandlers.get("demo://one")?.size ?? 0).toBe(0),
    );
    await waitFor(() => expect(eventHandlers.has("demo://two")).toBe(true));
  });

  it("degrades gracefully (no unhandled rejection) when listen() throws", async () => {
    listenShouldReject = true;
    const handler = vi.fn();

    expect(() => {
      renderHook(() => useTauriListen<string>("demo://event", handler));
    }).not.toThrow();

    // Give the rejected promise a turn to be caught internally.
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(eventHandlers.has("demo://event")).toBe(false);
  });
});
