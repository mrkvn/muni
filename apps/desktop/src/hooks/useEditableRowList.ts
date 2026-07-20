/**
 * Plan 039 task 59 — generic replacement for the `useMyWords` /
 * `useVocabulary` clone pair (~identical two-field row editors that
 * persist an `{ enabled, entries }` snapshot via a get/save IPC pair,
 * client-side-buffered until an explicit Save).
 *
 * Each row gets a client-only `id` (keying by array index would jump
 * focus to the wrong input on deletion); ids are stripped before
 * persistence and regenerated from the fresh snapshot on load/save.
 */
import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useMemo, useState } from "react";

export type EditableRow<TEntry> = TEntry & { id: string };

function freshId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return `row-${Math.random().toString(36).slice(2)}-${Date.now()}`;
}

function toRows<TEntry>(entries: TEntry[]): EditableRow<TEntry>[] {
  return entries.map((e) => ({ ...e, id: freshId() }));
}

function stripIds<TEntry>(rows: EditableRow<TEntry>[]): TEntry[] {
  return rows.map(({ id: _id, ...rest }) => rest as unknown as TEntry);
}

function rowsEqual<TEntry extends Record<string, string>>(
  a: EditableRow<TEntry>[],
  b: EditableRow<TEntry>[],
  fieldKeys: readonly (keyof TEntry)[],
): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    for (const key of fieldKeys) {
      if (a[i][key] !== b[i][key]) return false;
    }
  }
  return true;
}

export type EditableRowListConfig<TEntry extends Record<string, string>> = {
  /** IPC command returning `{ enabled, entries }`. */
  getCommand: string;
  /** IPC command persisting `{ snapshot: { enabled, entries } }` — also
   * used for the eager toggle write. */
  saveCommand: string;
  /** All string field keys on `TEntry`, in a stable order. */
  fieldKeys: readonly (keyof TEntry)[];
  /** Blank entry template used when a row is added via `addRow()`. */
  emptyEntry: TEntry;
  /** Field checked for non-empty-ness before a row survives the Save
   * cleanup filter (rows failing this are dropped rather than persisted
   * as phantom empty drafts). Mirrors each pane's historical behavior:
   * My Words keys off `trigger`, Vocabulary off `term`. */
  primaryField: keyof TEntry;
  /** Fields that must ALL be non-empty for `pruneIncompleteRows` to KEEP a
   * row (tab-leave defense against abandoned half-filled drafts). My Words
   * requires both sides; Vocabulary's `note` is intentionally optional. */
  requiredFieldsForPrune: readonly (keyof TEntry)[];
  /** Fields `.trim()`-ed when the canonical post-save row is re-keyed.
   * Vocabulary trims both `term` and `note` (backend does too); My Words
   * trims only `trigger` — `replacement` is preserved verbatim by design
   * (see `my_words.rs`'s `sanitize_snapshot` doc). Listing the wrong set
   * here was the task-59 "comment says canonical, code doesn't" bug. */
  trimOnSaveFields: readonly (keyof TEntry)[];
  /** Used in the load-error message, e.g. `"substitutions"` / `"vocabulary"`. */
  loadErrorLabel: string;
};

export type UseEditableRowList<TEntry extends Record<string, string>> = {
  enabled: boolean;
  rows: EditableRow<TEntry>[];
  loading: boolean;
  loadError: string | null;
  saving: boolean;
  saveError: string | null;
  dirty: boolean;
  setEnabled: (next: boolean) => Promise<void>;
  addRow: () => string;
  updateRow: (id: string, partial: Partial<TEntry>) => void;
  removeRow: (id: string) => void;
  pruneIncompleteRows: () => void;
  save: () => Promise<void>;
  revert: () => void;
};

export function useEditableRowList<TEntry extends Record<string, string>>(
  config: EditableRowListConfig<TEntry>,
): UseEditableRowList<TEntry> {
  const {
    getCommand,
    saveCommand,
    fieldKeys,
    emptyEntry,
    primaryField,
    requiredFieldsForPrune,
    trimOnSaveFields,
    loadErrorLabel,
  } = config;

  const [enabled, setEnabledState] = useState<boolean>(true);
  const [rows, setRows] = useState<EditableRow<TEntry>[]>([]);
  const [savedRows, setSavedRows] = useState<EditableRow<TEntry>[]>([]);
  const [loading, setLoading] = useState<boolean>(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [saving, setSaving] = useState<boolean>(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    invoke<{ enabled: boolean; entries: TEntry[] }>(getCommand)
      .then((snap) => {
        if (cancelled) return;
        const fresh = toRows(snap.entries);
        setEnabledState(snap.enabled);
        setRows(fresh);
        setSavedRows(fresh);
      })
      .catch((e) => {
        if (cancelled) return;
        setLoadError(`Failed to load ${loadErrorLabel}: ${String(e)}`);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [getCommand, loadErrorLabel]);

  const dirty = useMemo(
    () => !rowsEqual(rows, savedRows, fieldKeys),
    [rows, savedRows, fieldKeys],
  );

  const setEnabled = useCallback(
    async (next: boolean) => {
      setSaveError(null);
      const prev = enabled;
      setEnabledState(next);
      try {
        // Persist the toggle immediately alongside the last-saved entries
        // so the kill switch feels like a typical OS toggle (no separate
        // Save click required).
        await invoke(saveCommand, {
          snapshot: { enabled: next, entries: stripIds(savedRows) },
        });
      } catch (e) {
        setEnabledState(prev);
        setSaveError(`Failed to update toggle: ${String(e)}`);
        throw e;
      }
    },
    [enabled, savedRows, saveCommand],
  );

  const addRow = useCallback((): string => {
    const id = freshId();
    // Prepend so a newly added row lands at the top of the list, in view
    // next to the Add button.
    setRows((prev) => [{ ...emptyEntry, id }, ...prev]);
    return id;
  }, [emptyEntry]);

  const updateRow = useCallback((id: string, partial: Partial<TEntry>) => {
    setRows((prev) =>
      prev.map((r) => (r.id === id ? { ...r, ...partial } : r)),
    );
  }, []);

  const removeRow = useCallback((id: string) => {
    setRows((prev) => prev.filter((r) => r.id !== id));
  }, []);

  const pruneIncompleteRows = useCallback(() => {
    setRows((prev) =>
      prev.filter((r) =>
        requiredFieldsForPrune.every((key) => r[key].trim().length > 0),
      ),
    );
  }, [requiredFieldsForPrune]);

  const save = useCallback(async () => {
    setSaving(true);
    setSaveError(null);
    // Drop rows missing the primary field BEFORE persisting so an empty
    // draft row doesn't survive a save → reload as a phantom entry.
    const cleaned = rows.filter((r) => r[primaryField].trim().length > 0);
    try {
      const persisted = stripIds(cleaned);
      await invoke(saveCommand, {
        snapshot: { enabled, entries: persisted },
      });
      // Re-key so the UI reflects the canonical form the backend just
      // round-tripped (trimming only the fields this pane's backend
      // actually trims — see `trimOnSaveFields`'s doc).
      const canonical = persisted.map((entry) => {
        const next = { ...entry };
        for (const key of trimOnSaveFields) {
          next[key] = entry[key].trim() as TEntry[typeof key];
        }
        return next;
      });
      const next = toRows(canonical);
      setRows(next);
      setSavedRows(next);
    } catch (e) {
      setSaveError(`Save failed: ${String(e)}`);
      throw e;
    } finally {
      setSaving(false);
    }
  }, [enabled, rows, primaryField, saveCommand, trimOnSaveFields]);

  const revert = useCallback(() => {
    setRows(savedRows.map((r) => ({ ...r })));
    setSaveError(null);
  }, [savedRows]);

  return {
    enabled,
    rows,
    loading,
    loadError,
    saving,
    saveError,
    dirty,
    setEnabled,
    addRow,
    updateRow,
    removeRow,
    pruneIncompleteRows,
    save,
    revert,
  };
}
