import { useEffect, useRef, useState } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";

const SESSION_STATE_EVENT = "session://state-changed";

/**
 * Wire-format payload of the Rust `session://state-changed` event. Mirrors
 * `session::SessionState::as_wire()` (camelCase via serde). Keep this list in
 * lockstep with `SessionState` in `src-tauri/src/session.rs` — divergence
 * would cause the HUD to default to `idle` after a Rust-side rename.
 */
export type SessionStateWire =
  | "idle"
  | "listening"
  | "listeningLocked"
  | "cleaning"
  | "recovering"
  | "error";

const KNOWN_STATES: ReadonlySet<SessionStateWire> = new Set([
  "idle",
  "listening",
  "listeningLocked",
  "cleaning",
  "recovering",
  "error",
]);

/**
 * Subscribe to the session orchestrator's coarse state.
 *
 * Returns `idle` until either the first `session://state-changed` event
 * arrives or the boot-time seed from `get_session_state` resolves —
 * whichever comes first.
 *
 * The seed exists because Tauri events are pub/sub with no replay, and on
 * macOS the HUD window is created with `visible: false` so its WKWebView
 * may not run JS — including this hook's `listen()` registration — until
 * the OS first shows the window. That show is triggered by the same
 * `Listening` transition the HUD needs to observe, so the very first
 * `listening` event is consistently missed on a fresh launch. The bug
 * surfaced as "HUD pill doesn't appear during Listening on the first
 * press after restart — only flashes during Cleaning." Seeding from the
 * Rust-side `SessionStateTracker` on mount closes that gap. See
 * `src-tauri/src/session.rs::SessionStateTracker`.
 *
 * Used by:
 *   - HUD waveform — fades in on `listening`, fades out otherwise.
 *   - Future settings pages that need to disable controls during a session.
 */
export function useSessionState(): SessionStateWire {
  const [state, setState] = useState<SessionStateWire>("idle");
  // Ref-guarded so an event arriving between `listen()` registration and
  // `invoke()` resolution wins over a stale seed value. The race window
  // is small but real: if the user presses the hotkey while the seed is
  // in flight, the listener observes the new state first; the seed must
  // not clobber it with whatever the tracker held when the IPC entered.
  const eventReceivedRef = useRef(false);

  useEffect(() => {
    let cancelled = false;
    let unlisten: UnlistenFn | null = null;

    void (async () => {
      const off = await listen<string>(SESSION_STATE_EVENT, (event) => {
        const candidate = event.payload;
        if (typeof candidate !== "string") return;
        if (!KNOWN_STATES.has(candidate as SessionStateWire)) return;
        eventReceivedRef.current = true;
        setState(candidate as SessionStateWire);
      });
      if (cancelled) {
        off();
        return;
      }
      unlisten = off;

      try {
        const seed = await invoke<string>("get_session_state");
        if (cancelled) return;
        if (eventReceivedRef.current) return;
        if (typeof seed !== "string") return;
        if (!KNOWN_STATES.has(seed as SessionStateWire)) return;
        setState(seed as SessionStateWire);
      } catch {
        // Tracker isn't reachable (older Rust build, IPC not registered).
        // The listener stays armed; the next state transition will land
        // correctly even without the seed.
      }
    })();

    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, []);

  return state;
}
