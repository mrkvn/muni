// @vitest-environment jsdom
import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { SETTINGS_DEFAULTS, useSetting } from "./useSettings";

// In-memory IPC + event stubs (mirrors useSecrets.test.ts). `useSetting`
// calls `invoke("settings_get")` once on mount and registers a `listen`
// callback for `settings://changed`. Tests drive both through these maps.
type InvokeHandler = (args?: Record<string, unknown>) => unknown | Promise<unknown>;
type EventHandler = (event: { event: string; payload: unknown }) => void;

const invokeHandlers = new Map<string, InvokeHandler>();
const eventHandlers = new Map<string, Set<EventHandler>>();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (name: string, args?: Record<string, unknown>) => {
    const handler = invokeHandlers.get(name);
    if (!handler) throw new Error(`unmocked invoke("${name}")`);
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

function emit(event: string, payload?: unknown) {
  const set = eventHandlers.get(event);
  if (!set) return;
  for (const handler of set) handler({ event, payload });
}

beforeEach(() => {
  invokeHandlers.clear();
  eventHandlers.clear();
});

afterEach(() => {
  invokeHandlers.clear();
  eventHandlers.clear();
});

describe("useSetting", () => {
  it("loads the persisted value on mount", async () => {
    invokeHandlers.set("settings_get", () => false);

    const { result } = renderHook(() => useSetting("telemetry.analytics"));

    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.value).toBe(false);
  });

  it("updates live when settings://changed fires for this key", async () => {
    // Regression guard for the feature-033 telemetry bug: the main
    // window's Settings page mounts at app launch (keep-mounted routes),
    // so its `settings_get` runs once BEFORE the onboarding window writes
    // the telemetry toggle — leaving Settings showing a stale value.
    // Fix: Rust emits `settings://changed` from `settings_set`; the hook
    // listens and updates. This test fails if either side regresses.
    invokeHandlers.set("settings_get", () => true);

    const { result } = renderHook(() => useSetting("telemetry.analytics"));
    await waitFor(() => expect(result.current.value).toBe(true));

    // Another window persisted `false` — backend broadcasts the change.
    act(() => {
      emit("settings://changed", { key: "telemetry.analytics", value: false });
    });

    await waitFor(() => expect(result.current.value).toBe(false));
  });

  it("ignores settings://changed for a different key", async () => {
    invokeHandlers.set("settings_get", () => true);

    const { result } = renderHook(() => useSetting("telemetry.analytics"));
    await waitFor(() => expect(result.current.value).toBe(true));

    act(() => {
      emit("settings://changed", {
        key: "telemetry.crash_reporting",
        value: false,
      });
    });

    // Unrelated key must not clobber this hook's value.
    expect(result.current.value).toBe(true);
  });

  it("drops a malformed settings://changed payload without clobbering", async () => {
    // Plan 039 task 44 — a stray or corrupted broadcast (missing `key`, wrong
    // type, or absent `value`) must be ignored, never applied as `undefined`.
    invokeHandlers.set("settings_get", () => true);
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});

    const { result } = renderHook(() => useSetting("telemetry.analytics"));
    await waitFor(() => expect(result.current.value).toBe(true));

    act(() => {
      // No `key` field at all.
      emit("settings://changed", { value: false });
      // `key` present but not a string.
      emit("settings://changed", { key: 42, value: false });
      // Non-object payload.
      emit("settings://changed", "garbage");
    });

    // Value untouched by any of the malformed emits.
    expect(result.current.value).toBe(true);
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });
});

describe("SETTINGS_DEFAULTS drift guard", () => {
  it("pins general.launch_at_login to false, mirroring the Rust default", () => {
    // Plan 039 task 39(b) — the FE optimistic default and the Rust
    // `settings::default_for(KEY_GENERAL_LAUNCH_AT_LOGIN)` default (pinned
    // by `defaults_match_swift_v1_app_storage_values` in `settings.rs`,
    // and by `launch_at_login_pref_falls_back_to_settings_default` in
    // `lib.rs`) must never drift apart. If this default silently flips
    // back to `true`, `useSetting` would flash an "enabled" Login-Item
    // toggle before the real (false) value loads on every surface that
    // reads this key outside the onboarding wizard.
    expect(SETTINGS_DEFAULTS["general.launch_at_login"]).toBe(false);
  });
});
