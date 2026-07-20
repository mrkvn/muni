// @vitest-environment jsdom
import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useMinDurationTrue } from "./useMinDurationTrue";

describe("useMinDurationTrue", () => {
  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: false });
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("initial value of false stays false", () => {
    const { result } = renderHook(() => useMinDurationTrue(false, 300));
    expect(result.current).toBe(false);
  });

  it("flips to true immediately when input goes true", () => {
    const { result, rerender } = renderHook(
      ({ value }: { value: boolean }) => useMinDurationTrue(value, 300),
      { initialProps: { value: false } },
    );
    expect(result.current).toBe(false);

    rerender({ value: true });
    expect(result.current).toBe(true);
  });

  it("holds true for the floor duration when input flips false instantly", () => {
    const { result, rerender } = renderHook(
      ({ value }: { value: boolean }) => useMinDurationTrue(value, 300),
      { initialProps: { value: false } },
    );

    rerender({ value: true });
    expect(result.current).toBe(true);

    // ~1 ms later flip false (the bug we're guarding against — Listening →
    // Error transition in microseconds).
    act(() => {
      vi.advanceTimersByTime(1);
    });
    rerender({ value: false });
    // Should still be true — well inside the 300 ms floor.
    expect(result.current).toBe(true);

    // Halfway: still latched.
    act(() => {
      vi.advanceTimersByTime(150);
    });
    expect(result.current).toBe(true);

    // After the floor: latch releases.
    act(() => {
      vi.advanceTimersByTime(160);
    });
    expect(result.current).toBe(false);
  });

  it("releases immediately when value goes false after the floor", () => {
    const { result, rerender } = renderHook(
      ({ value }: { value: boolean }) => useMinDurationTrue(value, 300),
      { initialProps: { value: false } },
    );

    rerender({ value: true });
    act(() => {
      vi.advanceTimersByTime(500);
    });
    rerender({ value: false });
    expect(result.current).toBe(false);
  });

  it("re-latches when value flips back to true mid-floor", () => {
    const { result, rerender } = renderHook(
      ({ value }: { value: boolean }) => useMinDurationTrue(value, 300),
      { initialProps: { value: false } },
    );

    rerender({ value: true });
    act(() => {
      vi.advanceTimersByTime(50);
    });
    rerender({ value: false });
    // Floor timer scheduled — still latched.
    expect(result.current).toBe(true);

    act(() => {
      vi.advanceTimersByTime(100);
    });
    rerender({ value: true });
    // Re-latched; floor timer must be cancelled.
    expect(result.current).toBe(true);

    act(() => {
      vi.advanceTimersByTime(500);
    });
    // Still true because input is still true.
    expect(result.current).toBe(true);
  });

  it("clears its timer on unmount", () => {
    const { result, rerender, unmount } = renderHook(
      ({ value }: { value: boolean }) => useMinDurationTrue(value, 300),
      { initialProps: { value: false } },
    );
    rerender({ value: true });
    rerender({ value: false });
    expect(result.current).toBe(true);

    // Unmount before the floor expires — must not throw or leak.
    unmount();
    act(() => {
      vi.advanceTimersByTime(1000);
    });
    // Nothing to assert post-unmount; the test passes if no act() warnings
    // or unhandled timer callbacks blew up.
  });
});
