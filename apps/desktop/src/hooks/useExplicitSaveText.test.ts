// @vitest-environment jsdom
import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useExplicitSaveText } from "./useExplicitSaveText";

type InvokeHandler = (args?: Record<string, unknown>) => unknown | Promise<unknown>;

const invokeHandlers = new Map<string, InvokeHandler>();
const invokeCalls: { name: string; args?: Record<string, unknown> }[] = [];

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (name: string, args?: Record<string, unknown>) => {
    invokeCalls.push({ name, args });
    const handler = invokeHandlers.get(name);
    if (!handler) throw new Error(`unmocked invoke("${name}")`);
    return handler(args);
  },
}));

const CONFIG = {
  getCommand: "demo_get",
  saveCommand: "demo_save",
  maxLen: 100,
  label: "demo text",
};

beforeEach(() => {
  invokeHandlers.clear();
  invokeCalls.length = 0;
});

afterEach(() => {
  invokeHandlers.clear();
  invokeCalls.length = 0;
});

describe("useExplicitSaveText", () => {
  it("loads the persisted value on mount", async () => {
    invokeHandlers.set("demo_get", () => "hello");

    const { result } = renderHook(() => useExplicitSaveText(CONFIG));

    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.text).toBe("hello");
    expect(result.current.dirty).toBe(false);
  });

  it("surfaces a load error without crashing", async () => {
    invokeHandlers.set("demo_get", () => {
      throw new Error("boom");
    });

    const { result } = renderHook(() => useExplicitSaveText(CONFIG));

    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.loadError).toMatch(/Failed to load demo text/);
  });

  it("regression: keystrokes typed during an in-flight save survive", async () => {
    invokeHandlers.set("demo_get", () => "");
    // Resolve `demo_save` only once the test explicitly releases it, so we
    // can type while the save is still in flight.
    let releaseSave: (() => void) | null = null;
    invokeHandlers.set(
      "demo_save",
      () =>
        new Promise<void>((resolve) => {
          releaseSave = () => resolve();
        }),
    );

    const { result } = renderHook(() => useExplicitSaveText(CONFIG));
    await waitFor(() => expect(result.current.loading).toBe(false));

    act(() => result.current.setText("first draft"));
    expect(result.current.text).toBe("first draft");

    // Kick off save with "first draft" in flight...
    let saveDone: Promise<void> | undefined;
    act(() => {
      saveDone = result.current.save();
    });

    // ...the user keeps typing before the round-trip resolves.
    act(() => result.current.setText("first draft, plus more"));
    expect(result.current.text).toBe("first draft, plus more");

    // Now let the save resolve.
    await act(async () => {
      releaseSave?.();
      await saveDone;
    });

    // The live text must NOT have been clobbered back to "first draft" —
    // the user's later keystrokes survive.
    expect(result.current.text).toBe("first draft, plus more");
    // Dirty tracking still advances: "first draft" is now the persisted
    // baseline, and the still-unsaved "plus more" edit reads as dirty.
    expect(result.current.dirty).toBe(true);
    expect(invokeCalls.find((c) => c.name === "demo_save")?.args).toEqual({
      text: "first draft",
    });
  });

  it("applies the round-tripped (trimmed) value when no keystrokes landed mid-save", async () => {
    invokeHandlers.set("demo_get", () => "");
    invokeHandlers.set("demo_save", () => undefined);

    const { result } = renderHook(() => useExplicitSaveText(CONFIG));
    await waitFor(() => expect(result.current.loading).toBe(false));

    act(() => result.current.setText("trailing space   "));
    await act(async () => {
      await result.current.save();
    });

    expect(result.current.text).toBe("trailing space");
    expect(result.current.dirty).toBe(false);
    expect(result.current.status).toEqual({ kind: "saved" });
  });

  it("surfaces a save error and leaves the live text untouched", async () => {
    invokeHandlers.set("demo_get", () => "");
    invokeHandlers.set("demo_save", () => {
      throw new Error("network down");
    });

    const { result } = renderHook(() => useExplicitSaveText(CONFIG));
    await waitFor(() => expect(result.current.loading).toBe(false));

    act(() => result.current.setText("unsaved edit"));
    await act(async () => {
      await result.current.save();
    });

    expect(result.current.text).toBe("unsaved edit");
    expect(result.current.status.kind).toBe("error");
  });

  it("refuses to save when the text exceeds maxLen", async () => {
    invokeHandlers.set("demo_get", () => "");
    const saveSpy = vi.fn(() => undefined);
    invokeHandlers.set("demo_save", saveSpy);

    const { result } = renderHook(() =>
      useExplicitSaveText({ ...CONFIG, maxLen: 5 }),
    );
    await waitFor(() => expect(result.current.loading).toBe(false));

    act(() => result.current.setText("way too long"));
    expect(result.current.tooLong).toBe(true);

    await act(async () => {
      await result.current.save();
    });

    expect(saveSpy).not.toHaveBeenCalled();
  });
});
