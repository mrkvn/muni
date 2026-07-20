// @vitest-environment jsdom
import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  HUD_NOTICE_DWELL_MS,
  HUD_NOTICE_SETTLE_MS,
  useHudNotice,
} from "./useHudNotice";

type EventHandler = (event: { event: string; payload: unknown }) => void;

const eventHandlers = new Map<string, Set<EventHandler>>();

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

/** Advance past the settle window so a pending notice paints. */
function settle() {
  act(() => vi.advanceTimersByTime(HUD_NOTICE_SETTLE_MS));
}

beforeEach(() => {
  eventHandlers.clear();
  vi.useFakeTimers();
});

afterEach(() => {
  vi.runOnlyPendingTimers();
  vi.useRealTimers();
  eventHandlers.clear();
});

// `listen()` is async; the hook's effect awaits it to register the handler.
// Under fake timers `waitFor` can't advance its polling clock, so flush the
// pending microtasks directly instead. A few `act`-wrapped microtask turns
// are enough to settle the `await listen(...)` chain.
async function flushListen() {
  for (let i = 0; i < 5; i += 1) {
    await act(async () => {
      await Promise.resolve();
    });
  }
  expect(eventHandlers.get("hud://notice")?.size ?? 0).toBe(1);
}

describe("useHudNotice", () => {
  it("starts with no notice", async () => {
    const { result } = renderHook(() => useHudNotice());
    await flushListen();
    expect(result.current).toBeNull();
  });

  it("paints the notice only after the settle window", async () => {
    const { result } = renderHook(() => useHudNotice());
    await flushListen();

    act(() => emit("hud://notice", { text: "Connection dropped", tone: "neutral" }));
    // Still pending within the settle window — must not paint yet.
    act(() => vi.advanceTimersByTime(HUD_NOTICE_SETTLE_MS - 1));
    expect(result.current).toBeNull();

    act(() => vi.advanceTimersByTime(1));
    expect(result.current).toEqual({ text: "Connection dropped", tone: "neutral" });
  });

  it("auto-clears the notice after the dwell", async () => {
    const { result } = renderHook(() => useHudNotice());
    await flushListen();

    act(() => emit("hud://notice", { text: "No speech detected", tone: "amber" }));
    settle();
    expect(result.current).not.toBeNull();

    act(() => vi.advanceTimersByTime(HUD_NOTICE_DWELL_MS - 1));
    expect(result.current).not.toBeNull();

    act(() => vi.advanceTimersByTime(1));
    expect(result.current).toBeNull();
  });

  it("resets the dwell timer when a new notice arrives mid-dwell", async () => {
    const { result } = renderHook(() => useHudNotice());
    await flushListen();

    act(() => emit("hud://notice", { text: "first", tone: "neutral" }));
    settle();
    act(() => vi.advanceTimersByTime(HUD_NOTICE_DWELL_MS - 100));

    // Second notice settles and replaces the first; the first dwell must not
    // clear it.
    act(() => emit("hud://notice", { text: "second", tone: "amber" }));
    settle();
    expect(result.current).toEqual({ text: "second", tone: "amber" });

    act(() => vi.advanceTimersByTime(HUD_NOTICE_DWELL_MS));
    expect(result.current).toBeNull();
  });

  it("cancels a pending notice when a new press begins (no flash)", async () => {
    // A failed press surfaces its error just before the next press's Listening.
    // The rising edge of `pressActive` must cancel that still-pending notice so
    // it never paints.
    const { result, rerender } = renderHook(
      ({ active }: { active: boolean }) => useHudNotice(active),
      { initialProps: { active: false } },
    );
    await flushListen();

    act(() => emit("hud://notice", { text: "Deepgram key rejected", tone: "neutral" }));
    // New press begins inside the settle window.
    act(() => rerender({ active: true }));
    settle();
    expect(result.current).toBeNull();
  });

  it("still shows a notice that arrives mid-press (no rising edge to cancel)", async () => {
    // A connection blip while already listening: the press is in flight, so
    // there's no rising edge — the notice settles and paints normally.
    const { result } = renderHook(
      ({ active }: { active: boolean }) => useHudNotice(active),
      { initialProps: { active: true } },
    );
    await flushListen();

    act(() => emit("hud://notice", { text: "Connection dropped", tone: "neutral" }));
    settle();
    expect(result.current).toEqual({ text: "Connection dropped", tone: "neutral" });
  });

  it("leaves an already-painted chip alone when a new press begins", async () => {
    // Once a chip has settled and painted, a new press does NOT tear it down
    // (that would re-introduce the exit-fade blink) — it rides out its dwell.
    const { result, rerender } = renderHook(
      ({ active }: { active: boolean }) => useHudNotice(active),
      { initialProps: { active: false } },
    );
    await flushListen();

    act(() => emit("hud://notice", { text: "Pasted without cleanup", tone: "neutral" }));
    settle();
    expect(result.current).not.toBeNull();

    act(() => rerender({ active: true }));
    expect(result.current).toEqual({ text: "Pasted without cleanup", tone: "neutral" });
  });

  it("ignores malformed payloads", async () => {
    const { result } = renderHook(() => useHudNotice());
    await flushListen();

    act(() => emit("hud://notice", { text: "missing tone" }));
    act(() => emit("hud://notice", { text: 42, tone: "neutral" }));
    act(() => emit("hud://notice", { text: "bad tone", tone: "purple" }));
    act(() => emit("hud://notice", null));
    settle();

    expect(result.current).toBeNull();
  });

  it("unsubscribes on unmount", async () => {
    const { unmount } = renderHook(() => useHudNotice());
    await flushListen();

    unmount();

    expect(eventHandlers.get("hud://notice")?.size ?? 0).toBe(0);
  });
});
