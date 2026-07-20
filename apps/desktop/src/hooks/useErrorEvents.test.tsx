// @vitest-environment jsdom
import { renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useErrorEvents } from "./useErrorEvents";

type EventHandler = (event: { event: string; payload: unknown }) => void;

const eventHandlers = new Map<string, Set<EventHandler>>();
const navigateSpy = vi.fn();

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

vi.mock("react-router-dom", () => ({
  useNavigate: () => navigateSpy,
}));

vi.mock("sonner", () => ({
  toast: { warning: vi.fn() },
}));

function emit(event: string, payload?: unknown) {
  const set = eventHandlers.get(event);
  if (!set) return;
  for (const handler of set) handler({ event, payload });
}

beforeEach(() => {
  eventHandlers.clear();
  navigateSpy.mockClear();
});

afterEach(() => {
  eventHandlers.clear();
  navigateSpy.mockClear();
});

describe("useErrorEvents — navigate-to-tab routing", () => {
  it("routes an apiKeys settingsTab to /settings/api-keys", async () => {
    renderHook(() => useErrorEvents());

    // Wait for the async listen subscription to register.
    await waitFor(() =>
      expect(eventHandlers.has("error://navigate-to-tab")).toBe(true),
    );

    emit("error://navigate-to-tab", {
      kind: "deepgramKeyRejected",
      severity: "loud",
      userMessage: "Your Deepgram API key was rejected.",
      settingsTab: "apiKeys",
    });

    expect(navigateSpy).toHaveBeenCalledWith("/settings/api-keys");
  });

  it("does not navigate when settingsTab is null", async () => {
    renderHook(() => useErrorEvents());

    await waitFor(() =>
      expect(eventHandlers.has("error://navigate-to-tab")).toBe(true),
    );

    emit("error://navigate-to-tab", {
      kind: "nothingToPaste",
      severity: "quiet",
      userMessage: "Nothing to paste.",
      settingsTab: null,
    });

    expect(navigateSpy).not.toHaveBeenCalled();
  });
});
