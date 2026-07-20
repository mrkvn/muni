// @vitest-environment jsdom
/**
 * Plan 040 — Feedback section in Settings → General.
 *
 * Renders the real `SettingsGeneral` pane against in-memory Tauri IPC +
 * event stubs (mirrors `useSettings.test.ts`). The full pane reaches into
 * many IPC commands (settings, permissions, launch-at-login), so the
 * `invoke` mock returns sensible defaults for the ambient probes and we
 * assert only on the Feedback-specific behaviour:
 *  - the section + button render,
 *  - clicking the button hands the GitHub Issues URL to the shell-opener
 *    (no backend command — the Fider vouch flow is gone).
 */
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { toast } from "sonner";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { GITHUB_ISSUES_URL, SettingsGeneral } from "./SettingsGeneral";

type InvokeHandler = (args?: Record<string, unknown>) => unknown | Promise<unknown>;
type EventHandler = (event: { event: string; payload: unknown }) => void;

const invokeHandlers = new Map<string, InvokeHandler>();
const invokeCalls: { name: string; args?: Record<string, unknown> }[] = [];
const eventHandlers = new Map<string, Set<EventHandler>>();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (name: string, args?: Record<string, unknown>) => {
    invokeCalls.push({ name, args });
    const handler = invokeHandlers.get(name);
    // Unhandled ambient probes resolve to undefined rather than throwing so
    // a single missing default doesn't mask the assertion under test.
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

// `useLaunchAtLogin` subscribes to window focus changes; stub the window API
// so the hook's effect doesn't reach a real Tauri bridge in jsdom.
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    onFocusChanged: async () => () => {},
  }),
}));

vi.mock("sonner", () => ({
  toast: { error: vi.fn(), success: vi.fn() },
}));

// The shell-opener plugin has no bridge in jsdom; stub it so the click path
// is observable and can be forced to reject.
const openUrlMock = vi.fn<(url: string) => Promise<void>>();
vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: (url: string) => openUrlMock(url),
}));

// The History section's radix Slider reads ResizeObserver on mount, which
// jsdom doesn't implement. A no-op stub keeps the pane renderable without
// pulling in a heavier polyfill dependency.
class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
vi.stubGlobal("ResizeObserver", ResizeObserverStub);

/** Wire the ambient probes the General pane fires on mount. */
function primeAmbientInvokes() {
  invokeHandlers.set("settings_get", () => false);
  invokeHandlers.set("is_launch_at_login_enabled", () => false);
  invokeHandlers.set("is_mic_likely_silenced", () => false);
  invokeHandlers.set("microphone_status", () => "authorized");
  invokeHandlers.set("is_accessibility_trusted", () => true);
  invokeHandlers.set("input_monitoring_status", () => "granted");
}

function renderGeneral() {
  return render(
    <MemoryRouter initialEntries={["/settings/general"]}>
      <SettingsGeneral />
    </MemoryRouter>,
  );
}

function feedbackButton(): HTMLButtonElement {
  // Scope to the Feedback group so the "Open System Settings" permission
  // buttons can't accidentally satisfy the query.
  const section = screen.getByRole("group", { name: "Feedback" });
  return within(section).getByRole("button", {
    name: /open github issues/i,
  }) as HTMLButtonElement;
}

beforeEach(() => {
  invokeHandlers.clear();
  invokeCalls.length = 0;
  eventHandlers.clear();
  openUrlMock.mockReset();
  openUrlMock.mockResolvedValue(undefined);
});

afterEach(() => {
  cleanup();
  invokeHandlers.clear();
  invokeCalls.length = 0;
  eventHandlers.clear();
});

describe("SettingsGeneral — Feedback section", () => {
  it("renders the Feedback section heading and blurb", async () => {
    primeAmbientInvokes();
    renderGeneral();

    const section = screen.getByRole("group", { name: "Feedback" });
    expect(
      within(section).getByText(/report a bug or request a feature on github/i),
    ).toBeTruthy();
  });

  it("renders a discernible-text button with an aria-hidden icon", async () => {
    primeAmbientInvokes();
    renderGeneral();

    const button = feedbackButton();
    // Discernible accessible name (not icon-only).
    expect(button.textContent).toMatch(/open github issues/i);
    // The decorative lucide icon must be hidden from AT.
    expect(button.querySelector("[aria-hidden='true']")).not.toBeNull();
  });

  it("opens the GitHub issue tracker via the shell-opener when clicked", async () => {
    primeAmbientInvokes();

    renderGeneral();

    const button = feedbackButton();
    await waitFor(() => expect(button.disabled).toBe(false));

    const invokesBeforeClick = invokeCalls.length;
    fireEvent.click(button);

    await waitFor(() => expect(openUrlMock).toHaveBeenCalledTimes(1));
    expect(openUrlMock).toHaveBeenCalledWith(GITHUB_ISSUES_URL);
    // Regression guard: the old vouch command is gone, so the click must not
    // add a single IPC round-trip on top of the pane's ambient probes.
    expect(invokeCalls.length).toBe(invokesBeforeClick);
  });

  it("toasts an actionable message when the opener rejects", async () => {
    vi.mocked(toast.error).mockClear();
    primeAmbientInvokes();
    openUrlMock.mockRejectedValue(new Error("no handler for https"));

    renderGeneral();
    const button = feedbackButton();
    await waitFor(() => expect(button.disabled).toBe(false));

    fireEvent.click(button);

    await waitFor(() => expect(toast.error).toHaveBeenCalledTimes(1));
    const shown = vi.mocked(toast.error).mock.calls[0]?.[0] as string;
    // The fallback must name the destination so the user can reach it manually.
    expect(shown).toMatch(/github\.com\/mrkvn\/muni\/issues/);
    // Never leak the raw opener error into the toast.
    expect(shown).not.toMatch(/no handler/);
  });

});
