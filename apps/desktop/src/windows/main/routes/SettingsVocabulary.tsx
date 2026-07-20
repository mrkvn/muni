/**
 * Vocabulary settings tab — structured `{term, note}` list that softly
 * biases Muni toward the user's spellings via the cleanup LLM.
 *
 * Data layer + rendering are shared with Settings → Substitutions (My
 * Words) via {@link useEditableRowList} / {@link RowListEditor} (plan 039
 * task 59) — this file only supplies Vocabulary's specific IPC commands,
 * field config, and copy.
 */
import { RowListEditor, type RowFieldSpec } from "@/components/RowListEditor";
import {
  useEditableRowList,
  type EditableRowListConfig,
} from "@/hooks/useEditableRowList";
import {
  VOCABULARY_MAX_NOTE_LEN,
  VOCABULARY_MAX_TERM_LEN,
  type VocabularyEntry,
} from "@/hooks/useVocabulary";

const EMPTY_ENTRY: VocabularyEntry = { term: "", note: "" };

const CONFIG: EditableRowListConfig<VocabularyEntry> = {
  getCommand: "vocabulary_get",
  saveCommand: "vocabulary_save",
  fieldKeys: ["term", "note"],
  emptyEntry: EMPTY_ENTRY,
  primaryField: "term",
  // The note is intentionally optional — pruning only requires `term`.
  requiredFieldsForPrune: ["term"],
  // The backend trims + collapses whitespace in both `term` and `note`
  // (see `vocabulary.rs`'s `sanitize_snapshot`).
  trimOnSaveFields: ["term", "note"],
  loadErrorLabel: "vocabulary",
};

const FIELDS: readonly [
  RowFieldSpec<VocabularyEntry>,
  RowFieldSpec<VocabularyEntry>,
] = [
  {
    key: "term",
    ariaLabel: "Term",
    required: true,
    maxLength: VOCABULARY_MAX_TERM_LEN,
  },
  {
    key: "note",
    ariaLabel: "Note (optional)",
    required: false,
    maxLength: VOCABULARY_MAX_NOTE_LEN,
  },
];

const PLACEHOLDERS: ReadonlyArray<VocabularyEntry> = [
  { term: "Acme", note: "my company" },
  { term: "Jiro", note: "" },
];

export function SettingsVocabulary() {
  const state = useEditableRowList(CONFIG);

  return (
    <RowListEditor
      state={state}
      fields={FIELDS}
      headingId="vocabulary-heading"
      heading="Vocabulary"
      subheading="Words you say often that tend to get misheard."
      toggleId="vocabulary-enabled"
      toggleLabel="Enable vocabulary hints"
      columnLabels={["Term", "Note (optional)"]}
      placeholders={PLACEHOLDERS}
      placeholderAriaLabel="Example terms (not saved)"
      deleteButtonAriaLabel="Delete term"
      incompleteMessage="Term is required to save."
      savedToastMessage="Vocabulary saved"
      routePath="/settings/vocabulary"
      searchPlaceholder="Search vocabulary…  (⌘F)"
      searchAriaLabel="Search vocabulary"
      noMatchesMessage="No terms match your search."
    />
  );
}
