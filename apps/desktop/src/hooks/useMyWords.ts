/**
 * Feature 013 — My Words: per-user deterministic substitution rules.
 *
 * Wraps the `my_words_get` / `my_words_save` IPC commands.
 *
 * Plan 039 task 59 — the row-editing hook itself now lives in
 * {@link "@/hooks/useEditableRowList"} (shared with Vocabulary); this
 * module keeps only the wire-format types `SettingsMyWords.tsx` builds its
 * `EditableRowListConfig` from.
 */

export type MyWordsEntry = {
  trigger: string;
  replacement: string;
};

export type MyWordsSnapshot = {
  enabled: boolean;
  entries: MyWordsEntry[];
};
