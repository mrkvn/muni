/**
 * Feature 013 — My Words settings pane.
 *
 * Two-column editable list of `trigger → replacement` rules plus a global
 * on/off switch. Data layer + rendering are shared with Settings →
 * Vocabulary via {@link useEditableRowList} / {@link RowListEditor} (plan
 * 039 task 59) — this file only supplies My Words' specific IPC commands,
 * field config, and copy.
 */
import { RowListEditor, type RowFieldSpec } from "@/components/RowListEditor";
import {
  useEditableRowList,
  type EditableRowListConfig,
} from "@/hooks/useEditableRowList";
import type { MyWordsEntry } from "@/hooks/useMyWords";

const EMPTY_ENTRY: MyWordsEntry = { trigger: "", replacement: "" };

const CONFIG: EditableRowListConfig<MyWordsEntry> = {
  getCommand: "my_words_get",
  saveCommand: "my_words_save",
  fieldKeys: ["trigger", "replacement"],
  emptyEntry: EMPTY_ENTRY,
  primaryField: "trigger",
  // A substitution rule needs both sides to be meaningful — pruning drops
  // a row missing either.
  requiredFieldsForPrune: ["trigger", "replacement"],
  // `replacement` is preserved verbatim by the backend (see
  // `my_words.rs`'s `sanitize_snapshot` doc) — only `trigger` is trimmed.
  trimOnSaveFields: ["trigger"],
  loadErrorLabel: "substitutions",
};

const FIELDS: readonly [RowFieldSpec<MyWordsEntry>, RowFieldSpec<MyWordsEntry>] = [
  { key: "trigger", ariaLabel: "Trigger phrase", required: true },
  { key: "replacement", ariaLabel: "Replacement text", required: true },
];

const PLACEHOLDERS: ReadonlyArray<MyWordsEntry> = [
  { trigger: "cloud code", replacement: "Claude Code" },
];

export function SettingsMyWords() {
  const state = useEditableRowList(CONFIG);

  return (
    <RowListEditor
      state={state}
      fields={FIELDS}
      headingId="my-words-heading"
      heading="Substitutions"
      subheading="Fix recurring mishears and expand shorthand."
      toggleId="my-words-enabled"
      toggleLabel="Enable substitutions"
      columnLabels={["When I say", "Paste"]}
      placeholders={PLACEHOLDERS}
      placeholderAriaLabel="Example rules (not saved)"
      deleteButtonAriaLabel="Delete rule"
      incompleteMessage="Fill in both fields to save."
      savedToastMessage="Substitutions saved"
      routePath="/settings/my-words"
      searchPlaceholder="Search substitutions…  (⌘F)"
      searchAriaLabel="Search substitutions"
      noMatchesMessage="No substitutions match your search."
    />
  );
}
