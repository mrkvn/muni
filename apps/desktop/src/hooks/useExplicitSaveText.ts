/**
 * Plan 039 task 58 — parameterized replacement for the `useAboutMe` /
 * `useUserPrompt` clone pair (byte-identical apart from the IPC command
 * names, max length, and error copy).
 *
 * Persistence is gated behind an explicit Save click; typing only updates
 * client-side state until then.
 *
 * Fix over the cloned originals: `save()` used to unconditionally
 * `setTextState(trimmed)` after the round-trip resolved, clobbering any
 * keystrokes the user typed WHILE the `invoke` was in flight. `save()` now
 * snapshots `text` at call time and only reapplies the trimmed/round-tripped
 * value if the live text is still byte-identical to that snapshot when the
 * write resolves — `original` (the dirty-tracking baseline) always advances,
 * but the visible `text` is never overwritten out from under a still-typing
 * user.
 */
import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useRef, useState } from "react";

export type ExplicitSaveStatus =
  | { kind: "idle" }
  | { kind: "saving" }
  | { kind: "saved" }
  | { kind: "error"; message: string };

export type UseExplicitSaveText = {
  text: string;
  setText: (next: string) => void;
  save: () => Promise<void>;
  status: ExplicitSaveStatus;
  loadError: string | null;
  loading: boolean;
  dirty: boolean;
  tooLong: boolean;
};

export type ExplicitSaveTextConfig = {
  /** IPC command that returns the persisted string (e.g. `"about_me_get"`). */
  getCommand: string;
  /** IPC command that persists `{ text }` (e.g. `"about_me_save"`). */
  saveCommand: string;
  /** Hard cap enforced by both the backend and the UI, in Unicode code points. */
  maxLen: number;
  /** Used in the load-failure message, e.g. `"About Me"` / `"preferences"`. */
  label: string;
};

/**
 * Generic "load once, edit locally, persist on explicit Save" text field —
 * the shape shared by About Me (feature 014) and the cleanup Preferences
 * textarea. See the module doc for the save-clobber fix this centralizes.
 */
export function useExplicitSaveText(
  config: ExplicitSaveTextConfig,
): UseExplicitSaveText {
  const { getCommand, saveCommand, maxLen, label } = config;

  const [text, setTextState] = useState<string>("");
  const [original, setOriginal] = useState<string>("");
  const [loading, setLoading] = useState<boolean>(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [status, setStatus] = useState<ExplicitSaveStatus>({ kind: "idle" });

  // Mirrors the live `text` state without triggering re-renders of its own —
  // `save()` reads this AFTER the await to detect whether the user kept
  // typing during the round-trip, which a stale closure over `text` at
  // call-time cannot do.
  const textRef = useRef(text);
  useEffect(() => {
    textRef.current = text;
  }, [text]);

  useEffect(() => {
    let cancelled = false;
    invoke<string>(getCommand)
      .then((value) => {
        if (cancelled) return;
        setTextState(value);
        setOriginal(value);
      })
      .catch((e) => {
        if (cancelled) return;
        setLoadError(`Failed to load ${label}: ${String(e)}`);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [getCommand, label]);

  const setText = useCallback((next: string) => {
    setTextState(next);
    // Typing after a save clears the "Saved." pill so subsequent edits
    // feel responsive.
    setStatus((prev) => (prev.kind === "saved" ? { kind: "idle" } : prev));
  }, []);

  const dirty = text !== original;
  const tooLong = [...text].length > maxLen;

  const save = useCallback(async () => {
    if (tooLong) return;
    const snapshot = text;
    setStatus({ kind: "saving" });
    try {
      const trimmed = snapshot.replace(/\s+$/u, "");
      await invoke(saveCommand, { text: trimmed });
      // The dirty baseline always advances to what was actually persisted...
      setOriginal(trimmed);
      // ...but the visible text only follows if the user hasn't typed
      // something new since `snapshot` was captured. Overwriting a live
      // edit with the pre-save value is the clobber this hook exists to fix.
      if (textRef.current === snapshot) {
        setTextState(trimmed);
      }
      setStatus({ kind: "saved" });
    } catch (e) {
      setStatus({ kind: "error", message: `Save failed: ${String(e)}` });
    }
  }, [text, tooLong, saveCommand]);

  return {
    text,
    setText,
    save,
    status,
    loadError,
    loading,
    dirty,
    tooLong,
  };
}
