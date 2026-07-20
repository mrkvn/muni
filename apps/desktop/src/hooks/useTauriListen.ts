/**
 * Plan 039 task 57 — shared "listen with cancellation" boilerplate.
 *
 * Every hook/component that subscribes to a Tauri event repeats the same
 * dance: `listen()` is async, so under React's `<StrictMode>` double-invoke
 * (and any other fast mount/unmount) the effect can tear down *before* the
 * awaited `unlisten` handle resolves. Without a cancellation flag that would
 * leak the registration and double-deliver events after unmount. This hook
 * is the single place that gets the dance right so callers just supply an
 * event name and a handler.
 *
 * The handler is captured through a ref so callers can pass an inline
 * closure (reading the latest props/state) without needing to memoize it
 * with `useCallback` — only a change to `event` itself re-subscribes.
 */
import { listen } from "@tauri-apps/api/event";
import { useEffect, useRef } from "react";

/**
 * Subscribe to a Tauri event for the lifetime of the calling component.
 * Automatically unsubscribes on unmount (or when `event` changes) and
 * tolerates environments with no Tauri event bridge (e.g. plain jsdom/unit
 * tests that don't mock `@tauri-apps/api/event`) by swallowing the
 * subscription failure — the caller simply never receives events.
 */
export function useTauriListen<T>(
  event: string,
  handler: (payload: T) => void,
): void {
  const handlerRef = useRef(handler);
  handlerRef.current = handler;

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;

    void (async () => {
      try {
        const off = await listen<T>(event, (e) => {
          handlerRef.current(e.payload);
        });
        if (cancelled) {
          off();
          return;
        }
        unlisten = off;
      } catch {
        // No Tauri event bridge available — the caller degrades to
        // "never receives this event" rather than an unhandled rejection.
      }
    })();

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [event]);
}
