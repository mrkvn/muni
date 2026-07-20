// @vitest-environment jsdom
/**
 * Plan 039 task 60 — the History retention slider must not fire a
 * `settings_set` IPC write on every drag tick; only the drag-end (or
 * keyboard-step) commit should persist.
 *
 * Driving Radix's actual pointer-capture internals isn't reliable under
 * jsdom (no PointerEvent capture support), so this test doubles
 * `@/components/ui/slider` with a native `<input type="range">` that
 * exposes the same `onValueChange` / `onValueCommit` contract — the thing
 * under test is `SettingsGeneral`'s OWN wiring between those two callbacks
 * and `useSetting`, not Radix's drag mechanics.
 */
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";


import { SettingsGeneral } from "./SettingsGeneral";

type InvokeHandler = (args?: Record<string, unknown>) => unknown | Promise<unknown>;
type EventHandler = (event: { event: string; payload: unknown }) => void;

const invokeHandlers = new Map<string, InvokeHandler>();
const invokeCalls: { name: string; args?: Record<string, unknown> }[] = [];
const eventHandlers = new Map<string, Set<EventHandler>>();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (name: string, args?: Record<string, unknown>) => {
    invokeCalls.push({ name, args });
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

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    onFocusChanged: async () => () => {},
  }),
}));

vi.mock("sonner", () => ({
  toast: { error: vi.fn(), success: vi.fn() },
}));

class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
vi.stubGlobal("ResizeObserver", ResizeObserverStub);

vi.mock("@/components/ui/slider", () => ({
  Slider: (props: {
    id?: string;
    "aria-label"?: string;
    min?: number;
    max?: number;
    value?: number[];
    disabled?: boolean;
    onValueChange?: (values: number[]) => void;
    onValueCommit?: (values: number[]) => void;
  }) => (
    <input
      type="range"
      data-testid="retention-slider-stub"
      id={props.id}
      aria-label={props["aria-label"]}
      min={props.min}
      max={props.max}
      value={props.value?.[0]}
      disabled={props.disabled}
      onChange={(e) => props.onValueChange?.([Number(e.target.value)])}
      onBlur={() =>
        props.value && props.onValueCommit?.([props.value[0] as number])
      }
    />
  ),
}));

function primeAmbientInvokes() {
  invokeHandlers.set("settings_get", (args) => {
    if ((args as { key?: string } | undefined)?.key === "history.retention_days") {
      return 30;
    }
    return false;
  });
  invokeHandlers.set("settings_set", () => undefined);
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

beforeEach(() => {
  invokeHandlers.clear();
  invokeCalls.length = 0;
  eventHandlers.clear();
});

afterEach(() => {
  cleanup();
  invokeHandlers.clear();
  invokeCalls.length = 0;
  eventHandlers.clear();
});

describe("SettingsGeneral — retention slider commit semantics", () => {
  it("passes an accessible name through to the slider", async () => {
    primeAmbientInvokes();
    renderGeneral();

    await waitFor(() =>
      expect(screen.getByTestId("retention-slider-stub").getAttribute("aria-label")).toMatch(
        /retention/i,
      ),
    );
  });

  it("updates the displayed value on every change but does NOT write until commit", async () => {
    primeAmbientInvokes();
    renderGeneral();

    const slider = (await screen.findByTestId(
      "retention-slider-stub",
    )) as HTMLInputElement;
    await waitFor(() => expect(slider.value).toBe("30"));

    invokeCalls.length = 0; // clear the mount-time settings_get calls

    // Simulate several drag ticks — local display state updates, no commit.
    fireEvent.change(slider, { target: { value: "45" } });
    expect(screen.getByText(/45 days/)).toBeTruthy();
    fireEvent.change(slider, { target: { value: "60" } });
    expect(screen.getByText(/60 days/)).toBeTruthy();

    expect(invokeCalls.some((c) => c.name === "settings_set")).toBe(false);

    // Drag-end commit fires exactly one write, with the FINAL value.
    fireEvent.blur(slider);

    await waitFor(() =>
      expect(invokeCalls.filter((c) => c.name === "settings_set").length).toBe(1),
    );
    const commit = invokeCalls.find((c) => c.name === "settings_set");
    expect(commit?.args).toEqual({
      key: "history.retention_days",
      value: 60,
    });
  });
});
