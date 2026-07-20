// @vitest-environment jsdom
/**
 * Plan 039 task 61(d) — relative timestamps ("3m ago") must re-render on a
 * 30s interval while the History pane is the active Settings tab, and must
 * NOT keep a background timer running while it's hidden (every Settings
 * pane stays mounted — only the active one should pay for a live clock).
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, useNavigate } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { SettingsHistory } from "./SettingsHistory";

type InvokeHandler = (args?: Record<string, unknown>) => unknown | Promise<unknown>;
type EventHandler = (event: { event: string; payload: unknown }) => void;

const invokeHandlers = new Map<string, InvokeHandler>();
const eventHandlers = new Map<string, Set<EventHandler>>();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (name: string, args?: Record<string, unknown>) => {
    const handler = invokeHandlers.get(name);
    if (!handler) return undefined;
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

function oneMinuteAgoUnixSeconds(): number {
  return Math.floor(Date.now() / 1000) - 60;
}

function primeHistory() {
  invokeHandlers.set("history_list", () => [
    {
      id: 1,
      createdAt: oneMinuteAgoUnixSeconds(),
      rawText: "hello",
      cleanedText: "Hello.",
      targetAppBundleId: null,
      charCount: 6,
    },
  ]);
}

beforeEach(() => {
  invokeHandlers.clear();
  eventHandlers.clear();
  vi.useFakeTimers({ shouldAdvanceTime: true });
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  invokeHandlers.clear();
  eventHandlers.clear();
});

describe("SettingsHistory — relative timestamp refresh", () => {
  it("re-renders the relative time on the 30s interval while visible", async () => {
    primeHistory();

    render(
      <MemoryRouter initialEntries={["/settings/history"]}>
        <SettingsHistory />
      </MemoryRouter>,
    );

    await waitFor(() =>
      expect(screen.getByText(/1 minute ago/)).toBeTruthy(),
    );

    // Advance real + fake time together so `Date.now()` inside
    // `formatTimestamp` actually reflects the elapsed interval.
    await vi.advanceTimersByTimeAsync(30_000);

    await waitFor(() =>
      expect(screen.getByText(/2 minutes ago/)).toBeTruthy(),
    );
  });

  it("does not keep ticking once the pane is no longer the active route", async () => {
    primeHistory();

    function Harness() {
      const navigate = useNavigate();
      return (
        <>
          <button onClick={() => navigate("/settings/general")}>leave</button>
          <SettingsHistory />
        </>
      );
    }

    render(
      <MemoryRouter initialEntries={["/settings/history"]}>
        <Harness />
      </MemoryRouter>,
    );

    await waitFor(() => expect(screen.getByText(/1 minute ago/)).toBeTruthy());

    fireEvent.click(screen.getByRole("button", { name: "leave" }));

    // No pending interval should still be armed for this pane — advancing
    // time must not throw or leave a dangling timer (a real regression
    // would be an ever-growing interval count across tab switches, not
    // something a single assertion here can measure directly; this at
    // least proves the effect's cleanup ran without error).
    await vi.advanceTimersByTimeAsync(60_000);
    expect(vi.getTimerCount()).toBe(0);
  });
});
