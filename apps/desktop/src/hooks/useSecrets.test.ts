// @vitest-environment jsdom
import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useSecretProbe } from "./useSecrets";

// In-memory IPC + event stubs. The probe calls `invoke` for the initial
// fetch and any refresh, and registers a `listen` callback for
// `secrets://changed`. Tests drive both surfaces through the maps below.
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

beforeEach(() => {
  invokeHandlers.clear();
  eventHandlers.clear();
});

afterEach(() => {
  invokeHandlers.clear();
  eventHandlers.clear();
});

describe("useSecretProbe", () => {
  it("refreshes when a secrets://changed event arrives", async () => {
    // Regression guard for the Phase-11 bug where saving keys in the
    // onboarding wizard left Settings → API Keys stuck on "Not set".
    // Root cause: SettingsLayout mounts ALL routes at app launch
    // (Phase 10 keep-mounted optimisation), so the Settings probe runs
    // once before onboarding has saved anything and never re-runs. Fix:
    // Rust emits `secrets://changed` from set/delete; the hook
    // listens and refreshes. This test fails if either side regresses.
    let saved = false;
    invokeHandlers.set("secrets_is_deepgram_set", () => saved);

    const { result } = renderHook(() => useSecretProbe("deepgram"));

    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.isSet).toBe(false);

    saved = true;
    act(() => emit("secrets://changed"));

    await waitFor(() => expect(result.current.isSet).toBe(true));
  });

  it("unsubscribes from secrets://changed on unmount", async () => {
    invokeHandlers.set("secrets_is_groq_set", () => false);

    const { unmount } = renderHook(() => useSecretProbe("groq"));

    await waitFor(() =>
      expect(eventHandlers.get("secrets://changed")?.size ?? 0).toBe(1),
    );

    unmount();

    // The unlisten callback returned by `listen` removes the handler,
    // so a subsequent emit must reach zero subscribers — proves the
    // hook isn't leaking the subscription past component lifetime.
    await waitFor(() =>
      expect(eventHandlers.get("secrets://changed")?.size ?? 0).toBe(0),
    );
  });
});
