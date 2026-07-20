// @vitest-environment jsdom
import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useTrailingTrue } from "./useTrailingTrue";

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.runOnlyPendingTimers();
  vi.useRealTimers();
});

describe("useTrailingTrue", () => {
  it("returns true immediately while the value is true", () => {
    const { result } = renderHook(
      ({ v }: { v: boolean }) => useTrailingTrue(v, 200),
      { initialProps: { v: true } },
    );
    expect(result.current).toBe(true);
  });

  it("holds true for the trailing window after the value goes false", () => {
    const { result, rerender } = renderHook(
      ({ v }: { v: boolean }) => useTrailingTrue(v, 200),
      { initialProps: { v: true } },
    );
    act(() => rerender({ v: false }));
    expect(result.current).toBe(true);

    act(() => vi.advanceTimersByTime(199));
    expect(result.current).toBe(true);

    act(() => vi.advanceTimersByTime(1));
    expect(result.current).toBe(false);
  });

  it("cancels the pending fall if the value goes true again within the window", () => {
    const { result, rerender } = renderHook(
      ({ v }: { v: boolean }) => useTrailingTrue(v, 200),
      { initialProps: { v: true } },
    );
    act(() => rerender({ v: false }));
    act(() => vi.advanceTimersByTime(100));
    act(() => rerender({ v: true }));

    // Past where the original fall would have fired — still held.
    act(() => vi.advanceTimersByTime(200));
    expect(result.current).toBe(true);
  });
});
