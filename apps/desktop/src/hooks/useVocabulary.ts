/**
 * Feature 015 — Vocabulary: per-user soft-bias word list for the
 * Groq cleanup stage.
 *
 * Wraps the `vocabulary_get` / `vocabulary_save` IPC commands.
 *
 * Bias surface: the cleanup LLM (not the ASR). Empty list, or the
 * toggle off, sends a byte-identical prompt to today's
 * About-Me-only behaviour — see `apps/desktop/src-tauri/src/vocabulary.rs`.
 *
 * Plan 039 task 59 — the row-editing hook itself now lives in
 * {@link "@/hooks/useEditableRowList"} (shared with My Words); this
 * module keeps only the wire-format types + limits `SettingsVocabulary.tsx`
 * builds its `EditableRowListConfig` from.
 */

export const VOCABULARY_MAX_ENTRIES = 200;
export const VOCABULARY_MAX_TERM_LEN = 80;
export const VOCABULARY_MAX_NOTE_LEN = 120;

export type VocabularyEntry = {
  term: string;
  note: string;
};

export type VocabularySnapshot = {
  enabled: boolean;
  entries: VocabularyEntry[];
};
