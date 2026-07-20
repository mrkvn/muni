/**
 * Phase 10 — typed wrapper around the `history_*` IPC commands.
 *
 * The Rust side (apps/desktop/src-tauri/src/history_store.rs) owns the
 * SQLite-backed store and exposes list / count / delete / wipe verbs
 * via `commands.rs`. This hook keeps the React side reactive: it
 * refetches on mount, after every mutation, and whenever a new
 * dictation lands (subscribed via `transcript://final`).
 */
import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useRef, useState } from "react";
import { friendlyInvokeError } from "../lib/friendlyInvokeError";
import { useTauriListen } from "./useTauriListen";

/**
 * Wire shape of `DictationRecord` from `history_store.rs`. Mirrors the
 * `#[serde(rename_all = "camelCase")]` representation so this stays a
 * straight `JSON.parse`-style value.
 */
export interface DictationRecord {
  id: number;
  /** Unix epoch seconds (UTC). */
  createdAt: number;
  rawText: string;
  cleanedText: string;
  targetAppBundleId: string | null;
  charCount: number;
}

/**
 * Fired by Rust AFTER a successful insert into the history store.
 * Distinct from `transcript://final` (which fires before the SQLite
 * write completes) — using this avoids the refetch racing the writer
 * and showing stale data on the History tab.
 */
const HISTORY_CHANGED_EVENT = "history://changed";

interface UseHistoryResult {
  records: DictationRecord[];
  loading: boolean;
  error: string | null;
  refresh: () => Promise<void>;
  remove: (id: number) => Promise<void>;
  wipe: () => Promise<void>;
}

/**
 * Subscribe to the dictation history. `limit` caps the number of rows
 * returned (defaults to 200 — generous enough for casual use, well
 * inside Tauri's IPC payload budget).
 */
export function useHistory(limit = 200): UseHistoryResult {
  const [records, setRecords] = useState<DictationRecord[]>([]);
  const [loading, setLoading] = useState<boolean>(true);
  const [error, setError] = useState<string | null>(null);

  // Bumped every time a SERVER-authoritative snapshot lands (a completed
  // `refresh()`). `remove`/`wipe` capture the generation before their
  // optimistic update; if a newer server snapshot has landed by the time
  // their `invoke` rejects, that snapshot must win over rolling back to the
  // stale pre-optimistic `previous` — otherwise a `history://changed`
  // refresh racing an in-flight delete would be silently undone.
  const generationRef = useRef(0);

  const refresh = useCallback(async () => {
    try {
      const next = await invoke<DictationRecord[]>("history_list", { limit });
      setRecords(next);
      generationRef.current += 1;
      setError(null);
    } catch (e) {
      setError(friendlyInvokeError(e));
    } finally {
      setLoading(false);
    }
  }, [limit]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Auto-refresh whenever Rust finishes writing a new history row.
  // Listening for `history://changed` (rather than the broader
  // `transcript://final`) guarantees the SQLite insert has landed
  // before we refetch — otherwise the new row would be missing from
  // the very refresh it was supposed to trigger.
  useTauriListen<string>(HISTORY_CHANGED_EVENT, () => {
    void refresh();
  });

  const remove = useCallback(
    async (id: number) => {
      const previous = records;
      const generationAtStart = generationRef.current;
      setRecords((rows) => rows.filter((r) => r.id !== id));
      try {
        await invoke("history_delete", { id });
        setError(null);
      } catch (e) {
        // Only roll back if no fresher server snapshot has landed since —
        // otherwise we'd clobber that newer state with a stale one.
        if (generationRef.current === generationAtStart) {
          setRecords(previous);
        }
        setError(friendlyInvokeError(e));
        throw e;
      }
    },
    [records],
  );

  const wipe = useCallback(async () => {
    const previous = records;
    const generationAtStart = generationRef.current;
    setRecords([]);
    try {
      await invoke("history_wipe");
      setError(null);
    } catch (e) {
      if (generationRef.current === generationAtStart) {
        setRecords(previous);
      }
      setError(friendlyInvokeError(e));
      throw e;
    }
  }, [records]);

  return { records, loading, error, refresh, remove, wipe };
}
