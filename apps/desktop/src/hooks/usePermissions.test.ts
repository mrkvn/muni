// @vitest-environment jsdom
/**
 * Plan 039 task 61(a) — `useMicrophonePermission` / `useAccessibilityPermission`
 * / `useInputMonitoringPermission` (all built on the shared
 * `usePermissionState`) must resolve a failed read to an `error` state
 * instead of leaving an unhandled rejection at the mount effect, `refresh()`,
 * or a poll tick.
 */
import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useAccessibilityPermission } from "./usePermissions";

type InvokeHandler = (args?: Record<string, unknown>) => unknown | Promise<unknown>;
const invokeHandlers = new Map<string, InvokeHandler>();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (name: string, args?: Record<string, unknown>) => {
    const handler = invokeHandlers.get(name);
    if (!handler) throw new Error(`unmocked invoke("${name}")`);
    return handler(args);
  },
}));

beforeEach(() => {
  invokeHandlers.clear();
});

afterEach(() => {
  invokeHandlers.clear();
  vi.useRealTimers();
});

describe("useAccessibilityPermission — failure handling", () => {
  it("resolves a failed mount-time probe to an error state (no unhandled rejection)", async () => {
    invokeHandlers.set("is_accessibility_trusted", () => {
      throw new Error("IPC unreachable");
    });

    const { result } = renderHook(() => useAccessibilityPermission());

    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.error).toMatch(/Check failed/);
    // Fail-safe default: an unreadable permission reads as NOT trusted.
    expect(result.current.trusted).toBe(false);
  });

  it("clears the error once a later refresh succeeds", async () => {
    let shouldFail = true;
    invokeHandlers.set("is_accessibility_trusted", () => {
      if (shouldFail) throw new Error("still down");
      return true;
    });

    const { result } = renderHook(() => useAccessibilityPermission());
    await waitFor(() => expect(result.current.error).not.toBeNull());

    shouldFail = false;
    await act(async () => {
      await result.current.refresh();
    });

    expect(result.current.error).toBeNull();
    expect(result.current.trusted).toBe(true);
  });

  it("a failing refresh() call resolves rather than rejecting", async () => {
    invokeHandlers.set("is_accessibility_trusted", () => true);
    const { result } = renderHook(() => useAccessibilityPermission());
    await waitFor(() => expect(result.current.loading).toBe(false));

    invokeHandlers.set("is_accessibility_trusted", () => {
      throw new Error("flaky");
    });

    // Must not throw — a rejecting refresh() would surface as an unhandled
    // rejection at every fire-and-forget call site.
    await expect(
      act(async () => {
        await result.current.refresh();
      }),
    ).resolves.not.toThrow();

    expect(result.current.error).toMatch(/Check failed/);
  });

  it("a failing poll tick sets error without throwing", async () => {
    vi.useFakeTimers();
    let shouldFail = false;
    invokeHandlers.set("is_accessibility_trusted", () => {
      if (shouldFail) throw new Error("poll flake");
      return true;
    });

    const { result } = renderHook(() => useAccessibilityPermission());
    await act(async () => {
      await Promise.resolve();
    });

    act(() => {
      result.current.startPolling(1_000);
    });

    shouldFail = true;
    await act(async () => {
      vi.advanceTimersByTime(1_000);
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(result.current.error).toMatch(/Check failed/);
  });
});
