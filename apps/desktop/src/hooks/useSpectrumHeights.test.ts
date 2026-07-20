// @vitest-environment jsdom
import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { AMPLITUDE_EVENT } from "./useAudioAmplitude";
import { useSpectrumHeights } from "./useSpectrumHeights";
import { MIN_BAR_PX } from "@/windows/hud/waveform";

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

/** Flush the microtask that resolves the async `listen()` subscription. */
async function flushSubscription() {
  await act(async () => {
    await Promise.resolve();
  });
}

beforeEach(() => {
  eventHandlers.clear();
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  eventHandlers.clear();
});

describe("useSpectrumHeights", () => {
  it("rests at MIN heights before the first repick tick", () => {
    const { result } = renderHook(() => useSpectrumHeights());
    expect(result.current).toHaveLength(10);
    expect(result.current.every((h) => h === MIN_BAR_PX)).toBe(true);
  });

  it("subscribes to the amplitude event on mount and unsubscribes on unmount", async () => {
    const { unmount } = renderHook(() => useSpectrumHeights());
    await flushSubscription();

    expect(eventHandlers.get(AMPLITUDE_EVENT)?.size).toBe(1);

    unmount();
    expect(eventHandlers.get(AMPLITUDE_EVENT)?.size ?? 0).toBe(0);
  });

  it("does NOT re-render when amplitude events fire (the whole point)", async () => {
    let renders = 0;
    renderHook(() => {
      renders += 1;
      return useSpectrumHeights();
    });
    await flushSubscription();

    const baseline = renders;

    // A burst of ~25 Hz amplitude events must land in the ref WITHOUT
    // triggering a single render — that's the regression this guards.
    act(() => {
      for (let i = 0; i < 25; i++) emit(AMPLITUDE_EVENT, 0.5);
    });

    expect(renders).toBe(baseline);
  });

  it("re-renders only on the repick interval, using the latest amplitude", async () => {
    let renders = 0;
    const { result } = renderHook(() => {
      renders += 1;
      return useSpectrumHeights();
    });
    await flushSubscription();
    const baseline = renders;

    // Feed a loud amplitude through events (no render yet), then let one
    // repick tick fire — that single tick is the only re-render.
    act(() => emit(AMPLITUDE_EVENT, 0.9));
    expect(renders).toBe(baseline);

    act(() => {
      vi.advanceTimersByTime(80);
    });

    expect(renders).toBe(baseline + 1);
    // amplitude 0.9 → shaped to a full envelope → every bar lifts above MIN.
    expect(Math.max(...result.current)).toBeGreaterThan(MIN_BAR_PX);
  });

  it("collapses back to MIN heights when amplitude returns to silence", async () => {
    const { result } = renderHook(() => useSpectrumHeights());
    await flushSubscription();

    act(() => emit(AMPLITUDE_EVENT, 0.9));
    act(() => vi.advanceTimersByTime(80));
    expect(Math.max(...result.current)).toBeGreaterThan(MIN_BAR_PX);

    act(() => emit(AMPLITUDE_EVENT, 0));
    act(() => vi.advanceTimersByTime(80));
    expect(result.current.every((h) => h === MIN_BAR_PX)).toBe(true);
  });
});
