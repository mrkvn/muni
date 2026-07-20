// @vitest-environment jsdom
import { renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useRegistrationFailureToast } from "./SettingsShortcuts";

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

const toastError = vi.fn();
vi.mock("sonner", () => ({
  toast: { error: (...args: unknown[]) => toastError(...args) },
}));

// The hook module also pulls in the Tauri core invoke via useSettings; stub it
// so importing the route module doesn't try to reach a real backend.
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn().mockResolvedValue(undefined),
}));

function emit(event: string, payload?: unknown) {
  const set = eventHandlers.get(event);
  if (!set) return;
  for (const handler of set) handler({ event, payload });
}

beforeEach(() => {
  eventHandlers.clear();
  toastError.mockClear();
});

afterEach(() => {
  eventHandlers.clear();
  toastError.mockClear();
});

describe("useRegistrationFailureToast (plan 039 task 51a)", () => {
  it("toasts when a dictation registration fails", async () => {
    renderHook(() => useRegistrationFailureToast());
    await waitFor(() =>
      expect(eventHandlers.has("hotkey://registration-failed")).toBe(true),
    );

    emit("hotkey://registration-failed", {
      target: "dictation",
      accel: "Control+Shift+KeyR",
    });

    expect(toastError).toHaveBeenCalledTimes(1);
    expect(String(toastError.mock.calls[0][0])).toContain("dictation");
  });

  it("toasts when a re-paste registration fails", async () => {
    renderHook(() => useRegistrationFailureToast());
    await waitFor(() =>
      expect(eventHandlers.has("hotkey://registration-failed")).toBe(true),
    );

    emit("hotkey://registration-failed", {
      target: "repaste",
      accel: "Control+Command+KeyV",
    });

    expect(toastError).toHaveBeenCalledTimes(1);
    expect(String(toastError.mock.calls[0][0])).toContain("re-paste");
  });

  it("only reacts to registration-failed, ignoring other hotkey events", async () => {
    // A normal commit emits `settings://changed` (and the backend's idempotent
    // re-register emits no registration-failed at all). The listener must be
    // scoped to registration-failed alone — an unrelated hotkey/settings event
    // must never surface a toast. This would fail if the subscription were ever
    // widened to a broader event name.
    renderHook(() => useRegistrationFailureToast());
    await waitFor(() =>
      expect(eventHandlers.has("hotkey://registration-failed")).toBe(true),
    );

    // An unrelated event (the shape a successful commit emits) must not toast.
    emit("settings://changed", {
      key: "hotkey.repaste_binding",
      value: { mods: ["control", "command"], key: "KeyV" },
    });
    emit("hotkey://some-other-event", { target: "repaste" });

    expect(toastError).not.toHaveBeenCalled();

    // And the registration-failed event still toasts — proving the listener is
    // live, so the assertion above is a real filter, not a dead subscription.
    emit("hotkey://registration-failed", {
      target: "repaste",
      accel: "Control+Command+KeyV",
    });
    expect(toastError).toHaveBeenCalledTimes(1);
  });
});
