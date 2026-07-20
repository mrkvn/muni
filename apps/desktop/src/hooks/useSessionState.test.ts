// @vitest-environment jsdom
import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useSessionState } from "./useSessionState";

type InvokeHandler = (args?: Record<string, unknown>) => unknown | Promise<unknown>;
type EventHandler = (event: { event: string; payload: unknown }) => void;

const invokeHandlers = new Map<string, InvokeHandler>();
const eventHandlers = new Map<string, Set<EventHandler>>();
const invokeGate = { resolveNext: null as (() => void) | null };

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (name: string, args?: Record<string, unknown>) => {
    const handler = invokeHandlers.get(name);
    if (!handler) throw new Error(`unmocked invoke("${name}")`);
    if (invokeGate.resolveNext) {
      await new Promise<void>((resolve) => {
        invokeGate.resolveNext = resolve;
      });
    }
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

beforeEach(() => {
  invokeHandlers.clear();
  eventHandlers.clear();
  invokeGate.resolveNext = null;
});

afterEach(() => {
  invokeHandlers.clear();
  eventHandlers.clear();
  invokeGate.resolveNext = null;
});

describe("useSessionState", () => {
  it("seeds state from get_session_state on mount", async () => {
    // Regression guard for the HUD-first-press-miss bug: on a fresh
    // launch the HUD webview can mount AFTER the orchestrator's first
    // `Listening` transition, missing the pub/sub event entirely. The
    // hook must pull the live state from Rust on mount so the pill
    // still shows for the in-flight press.
    invokeHandlers.set("get_session_state", () => "listening");

    const { result } = renderHook(() => useSessionState());

    await waitFor(() => expect(result.current).toBe("listening"));
  });

  it("does not seed when an event arrives before the seed resolves", async () => {
    // Race guard: if an event lands between `listen()` registration
    // and the `invoke()` round trip, the event wins. The seed is a
    // snapshot from the moment the IPC entered Rust — clobbering a
    // newer event with it would regress the user-visible state.
    invokeHandlers.set("get_session_state", () => "idle");
    invokeGate.resolveNext = () => {}; // park invoke until released

    const { result } = renderHook(() => useSessionState());

    // Listener should be registered before we emit.
    await waitFor(() =>
      expect(eventHandlers.get("session://state-changed")?.size ?? 0).toBe(1),
    );

    act(() => emit("session://state-changed", "cleaning"));
    await waitFor(() => expect(result.current).toBe("cleaning"));

    // Release the parked seed (which resolves to "idle"); the hook
    // must NOT overwrite the more-recent "cleaning" value.
    act(() => {
      invokeGate.resolveNext?.();
      invokeGate.resolveNext = null;
    });

    // Give the microtask queue a tick to drain.
    await new Promise((r) => setTimeout(r, 0));
    expect(result.current).toBe("cleaning");
  });

  it("updates from subsequent session://state-changed events", async () => {
    invokeHandlers.set("get_session_state", () => "idle");

    const { result } = renderHook(() => useSessionState());

    await waitFor(() => expect(result.current).toBe("idle"));

    act(() => emit("session://state-changed", "listening"));
    await waitFor(() => expect(result.current).toBe("listening"));

    act(() => emit("session://state-changed", "cleaning"));
    await waitFor(() => expect(result.current).toBe("cleaning"));

    act(() => emit("session://state-changed", "idle"));
    await waitFor(() => expect(result.current).toBe("idle"));
  });

  it("ignores unknown payload values", async () => {
    invokeHandlers.set("get_session_state", () => "idle");

    const { result } = renderHook(() => useSessionState());

    await waitFor(() => expect(result.current).toBe("idle"));

    act(() => emit("session://state-changed", "bogus"));
    act(() => emit("session://state-changed", 42));
    expect(result.current).toBe("idle");
  });

  it("falls back to idle when get_session_state is unavailable", async () => {
    // Older Rust builds without the IPC command must not crash the
    // hook — the listener stays armed and the next real event lands
    // normally.
    // No handler registered → mocked invoke throws.

    const { result } = renderHook(() => useSessionState());

    await waitFor(() =>
      expect(eventHandlers.get("session://state-changed")?.size ?? 0).toBe(1),
    );

    expect(result.current).toBe("idle");

    act(() => emit("session://state-changed", "listening"));
    await waitFor(() => expect(result.current).toBe("listening"));
  });

  it("unsubscribes on unmount", async () => {
    invokeHandlers.set("get_session_state", () => "idle");

    const { unmount } = renderHook(() => useSessionState());

    await waitFor(() =>
      expect(eventHandlers.get("session://state-changed")?.size ?? 0).toBe(1),
    );

    unmount();

    await waitFor(() =>
      expect(eventHandlers.get("session://state-changed")?.size ?? 0).toBe(0),
    );
  });
});
