/**
 * Plan 039 task 59 — generic two-field editable row list, shared by
 * Settings → Substitutions (My Words) and Settings → Vocabulary. Pairs
 * with {@link useEditableRowList} for the data layer; this component owns
 * only rendering + row-level keyboard interaction.
 *
 * Fix over the two cloned originals: Escape used to unconditionally
 * delete the row under the cursor, including one the user had already
 * filled in — a stray Escape (e.g. dismissing an IME candidate, or a
 * habitual "cancel" reflex) silently destroyed real data. Escape now:
 *   - on an EMPTY row (both fields blank): removes it — canceling a draft
 *     the user hasn't started filling in is exactly what Escape should do.
 *   - on a row with ANY content: blurs the field and clears the inline
 *     error, but never deletes. Destructive removal stays on the explicit
 *     delete button.
 */
import { ArrowRight, Plus, Search, Trash2 } from "lucide-react";
import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { useLocation } from "react-router-dom";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import type { EditableRow, UseEditableRowList } from "@/hooks/useEditableRowList";

export type RowFieldSpec<TEntry extends Record<string, string>> = {
  key: keyof TEntry & string;
  ariaLabel: string;
  /** Whether this field must be non-empty for Enter-to-save validation and
   * the inline invalid/error styling. (Vocabulary's `note` is optional.) */
  required: boolean;
  maxLength?: number;
};

export type RowListEditorProps<TEntry extends Record<string, string>> = {
  state: UseEditableRowList<TEntry>;
  /** Exactly two fields, rendered left-to-right with an arrow glyph between
   * them (both panes are `{ trigger|term → replacement|note }` shaped). */
  fields: readonly [RowFieldSpec<TEntry>, RowFieldSpec<TEntry>];
  headingId: string;
  heading: string;
  subheading: string;
  toggleId: string;
  toggleLabel: string;
  columnLabels: readonly [string, string];
  placeholders: ReadonlyArray<TEntry>;
  placeholderAriaLabel: string;
  deleteButtonAriaLabel: string;
  incompleteMessage: string;
  savedToastMessage: string;
  /** Route path this pane renders at — Settings keeps every pane mounted
   * (hidden via CSS), so "leaving the tab" is detected via `useLocation`
   * rather than an unmount. */
  routePath: string;
  /** Placeholder shown in the filter box (which appears once the list has
   * at least one row). Copy is per-pane so it names what's being searched. */
  searchPlaceholder?: string;
  searchAriaLabel?: string;
  /** Shown when a filter is active but no row matches it. */
  noMatchesMessage?: string;
};

export function RowListEditor<TEntry extends Record<string, string>>({
  state,
  fields,
  headingId,
  heading,
  subheading,
  toggleId,
  toggleLabel,
  columnLabels,
  placeholders,
  placeholderAriaLabel,
  deleteButtonAriaLabel,
  incompleteMessage,
  savedToastMessage,
  routePath,
  searchPlaceholder = "Search…",
  searchAriaLabel = "Search list",
  noMatchesMessage = "No matches.",
}: RowListEditorProps<TEntry>) {
  const {
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
  } = state;

  const [firstField, secondField] = fields;

  // Auto-focus the first field of a newly added row. We track the pending
  // id and consume it from an effect rather than calling focus() inside the
  // click handler, since the row's <input> doesn't exist in the DOM until
  // React commits the next render.
  const firstFieldRefs = useRef<Map<string, HTMLInputElement>>(new Map());
  const [pendingFocusId, setPendingFocusId] = useState<string | null>(null);

  useEffect(() => {
    if (!pendingFocusId) return;
    const el = firstFieldRefs.current.get(pendingFocusId);
    if (el) {
      el.focus();
      setPendingFocusId(null);
    }
  }, [pendingFocusId, rows]);

  // Filter box: the list grows large over time, so a substring search lets
  // the user check whether a rule already exists before adding a duplicate.
  // `filter` narrows only what's *rendered* — the full `rows` list stays the
  // source of truth for dirty-checking, save, and prune, so hidden rows are
  // never dropped.
  const [filter, setFilter] = useState("");
  const searchRef = useRef<HTMLInputElement>(null);

  const trimmedFilter = filter.trim().toLowerCase();
  const visibleRows = trimmedFilter
    ? rows.filter((row) =>
        fields.some((f) => row[f.key].toLowerCase().includes(trimmedFilter)),
      )
    : rows;

  function onAdd() {
    // A new row is empty, so it wouldn't match an active filter and would
    // appear to "do nothing". Clear the filter so the fresh row is visible
    // (and focusable) right away.
    setFilter("");
    const id = addRow();
    setPendingFocusId(id);
  }

  // The Settings layout keeps every pane mounted (hidden via CSS), so
  // "leaving the tab" is detected by watching the pathname rather than an
  // unmount.
  const { pathname } = useLocation();
  const isActive = pathname === routePath;
  const wasActiveRef = useRef(isActive);
  useEffect(() => {
    if (wasActiveRef.current && !isActive) {
      pruneIncompleteRows();
    }
    wasActiveRef.current = isActive;
  }, [isActive, pruneIncompleteRows]);

  // Cmd/Ctrl+F focuses the filter box. Gated on `isActive` because every
  // Settings pane stays mounted (hidden via CSS) — an ungated window
  // listener would fire for all panes at once. `preventDefault` suppresses
  // the webview's native find bar so ours takes over.
  useEffect(() => {
    if (!isActive) return;
    const onKeyDown = (e: globalThis.KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "f") {
        if (!searchRef.current) return; // no filter box when the list is empty
        e.preventDefault();
        searchRef.current.focus();
        searchRef.current.select();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [isActive]);

  async function onToggle(next: boolean) {
    try {
      await setEnabled(next);
    } catch (e) {
      toast.error(`Toggle failed: ${String(e)}`);
    }
  }

  async function onSave() {
    try {
      await save();
      toast.success(savedToastMessage);
    } catch {
      // Inline saveError pill surfaces the message; toast would be noisy.
    }
  }

  // Enter-to-save: pressing Enter in either field commits the whole list
  // if the row satisfies every `required` field. Otherwise it's marked
  // invalid with an inline message instead of toasting.
  const [errorRowId, setErrorRowId] = useState<string | null>(null);

  function rowSatisfiesRequired(row: EditableRow<TEntry>): boolean {
    return fields
      .filter((f) => f.required)
      .every((f) => row[f.key].trim().length > 0);
  }

  // Clear the row-level error as soon as the user fills the missing
  // field(s), so the red ring + message don't linger after they fix it.
  useEffect(() => {
    if (!errorRowId) return;
    const row = rows.find((r) => r.id === errorRowId);
    if (!row) {
      setErrorRowId(null);
      return;
    }
    if (rowSatisfiesRequired(row)) {
      setErrorRowId(null);
    }
  }, [rows, errorRowId]);

  function onRowEnter(rowId: string) {
    const row = rows.find((r) => r.id === rowId);
    if (!row) return;
    if (!rowSatisfiesRequired(row)) {
      setErrorRowId(rowId);
      return;
    }
    setErrorRowId(null);
    if (saving || !dirty) return;
    void onSave();
  }

  function onRowEscape(rowId: string, event: KeyboardEvent<HTMLInputElement>) {
    if (errorRowId === rowId) setErrorRowId(null);
    const row = rows.find((r) => r.id === rowId);
    if (!row) return;
    const isEmpty = fields.every((f) => row[f.key].trim().length === 0);
    if (isEmpty) {
      // Canceling a draft the user hasn't started filling in — safe to
      // remove.
      removeRow(rowId);
      return;
    }
    // A row with real content must never disappear from an Escape
    // keystroke — only back out of editing.
    event.currentTarget.blur();
  }

  return (
    <section aria-labelledby={headingId} className="flex flex-col gap-4">
      <header>
        <h1 id={headingId} className="text-xl font-semibold tracking-tight">
          {heading}
        </h1>
        <p className="text-muted-foreground text-sm">{subheading}</p>
      </header>

      {loadError ? (
        <p role="alert" className="text-destructive text-sm">
          {loadError}
        </p>
      ) : null}

      <div className="flex items-center justify-between gap-3 rounded-md border border-border bg-card/40 px-3 py-2">
        <Label htmlFor={toggleId} className="text-sm font-medium">
          {toggleLabel}
        </Label>
        <Switch
          id={toggleId}
          checked={enabled}
          onCheckedChange={(next) => void onToggle(next)}
          disabled={loading}
        />
      </div>

      <div className="flex items-center gap-3">
        <Button variant="outline" size="sm" onClick={onAdd} disabled={loading}>
          <Plus className="h-4 w-4" aria-hidden="true" />
          Add
        </Button>
        {/* Filter box lives in the toolbar row (between Add and Save) so it
         * never costs the list a dedicated row of height. It only appears
         * once there are rows to search; a spacer keeps Add left / actions
         * right when the list is empty. */}
        {rows.length > 0 ? (
          <div className="relative min-w-0 flex-1">
            <Search
              className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground"
              aria-hidden="true"
            />
            <Input
              ref={searchRef}
              type="search"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  e.preventDefault();
                  setFilter("");
                  e.currentTarget.blur();
                }
              }}
              placeholder={searchPlaceholder}
              aria-label={searchAriaLabel}
              // h-8 matches the toolbar buttons (app baseline is Button
              // size="sm" = h-8); the Input default h-9 sat 4px too tall.
              className="h-8 pl-9"
            />
          </div>
        ) : (
          <div className="flex-1" />
        )}
        <div className="flex items-center gap-3">
          {saveError ? (
            <span role="alert" className="text-destructive text-xs">
              {saveError}
            </span>
          ) : null}
          <Button variant="outline" onClick={revert} disabled={!dirty || saving}>
            Revert changes
          </Button>
          <Button onClick={() => void onSave()} disabled={!dirty || saving}>
            Save
          </Button>
        </div>
      </div>

      <div className="flex flex-col gap-2">
        <div
          className="grid grid-cols-[1fr_auto_1fr_auto] items-center gap-2 text-muted-foreground text-xs px-1"
          aria-hidden="true"
        >
          <span>{columnLabels[0]}</span>
          <span />
          <span>{columnLabels[1]}</span>
          <span />
        </div>

        {rows.length === 0 ? (
          <PlaceholderRows
            fields={fields}
            placeholders={placeholders}
            ariaLabel={placeholderAriaLabel}
          />
        ) : visibleRows.length === 0 ? (
          <p role="status" className="px-1 py-2 text-muted-foreground text-sm">
            {noMatchesMessage}
          </p>
        ) : (
          <ul className="flex flex-col gap-2">
            {visibleRows.map((row) => {
              const rowHasError = errorRowId === row.id;
              const errorId = `${headingId}-error-${row.id}`;
              const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  onRowEnter(row.id);
                } else if (e.key === "Escape") {
                  e.preventDefault();
                  onRowEscape(row.id, e);
                }
              };
              return (
                <li key={row.id} className="flex flex-col gap-1">
                  <div className="grid grid-cols-[1fr_auto_1fr_auto] items-center gap-2">
                    <Input
                      aria-label={firstField.ariaLabel}
                      value={row[firstField.key]}
                      maxLength={firstField.maxLength}
                      aria-invalid={
                        (rowHasError &&
                          firstField.required &&
                          row[firstField.key].trim().length === 0) ||
                        undefined
                      }
                      aria-describedby={rowHasError ? errorId : undefined}
                      ref={(el) => {
                        if (el) {
                          firstFieldRefs.current.set(row.id, el);
                        } else {
                          firstFieldRefs.current.delete(row.id);
                        }
                      }}
                      onChange={(e) =>
                        updateRow(row.id, {
                          [firstField.key]: e.target.value,
                        } as Partial<TEntry>)
                      }
                      onKeyDown={onKeyDown}
                    />
                    <ArrowRight
                      className="h-4 w-4 text-muted-foreground"
                      aria-hidden="true"
                    />
                    <Input
                      aria-label={secondField.ariaLabel}
                      value={row[secondField.key]}
                      maxLength={secondField.maxLength}
                      aria-invalid={
                        (rowHasError &&
                          secondField.required &&
                          row[secondField.key].trim().length === 0) ||
                        undefined
                      }
                      aria-describedby={rowHasError ? errorId : undefined}
                      onChange={(e) =>
                        updateRow(row.id, {
                          [secondField.key]: e.target.value,
                        } as Partial<TEntry>)
                      }
                      onKeyDown={onKeyDown}
                    />
                    <Button
                      variant="ghost"
                      size="icon"
                      aria-label={deleteButtonAriaLabel}
                      onClick={() => removeRow(row.id)}
                    >
                      <Trash2 className="h-4 w-4" aria-hidden="true" />
                    </Button>
                  </div>
                  {rowHasError ? (
                    <p
                      id={errorId}
                      role="alert"
                      className="px-1 text-destructive text-xs"
                    >
                      {incompleteMessage}
                    </p>
                  ) : null}
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </section>
  );
}

function PlaceholderRows<TEntry extends Record<string, string>>({
  fields,
  placeholders,
  ariaLabel,
}: {
  fields: readonly [RowFieldSpec<TEntry>, RowFieldSpec<TEntry>];
  placeholders: ReadonlyArray<TEntry>;
  ariaLabel: string;
}) {
  const [firstField, secondField] = fields;
  return (
    <ul className="flex flex-col gap-2 opacity-60" aria-label={ariaLabel}>
      {placeholders.map((p, index) => (
        <li
          // Placeholders are static example copy, not user data — index is
          // a stable key here.
          key={index}
          className="grid grid-cols-[1fr_auto_1fr_auto] items-center gap-2"
        >
          <Input
            aria-hidden="true"
            tabIndex={-1}
            disabled
            value=""
            placeholder={p[firstField.key]}
            readOnly
          />
          <ArrowRight className="h-4 w-4 text-muted-foreground" aria-hidden="true" />
          <Input
            aria-hidden="true"
            tabIndex={-1}
            disabled
            value=""
            placeholder={p[secondField.key]}
            readOnly
          />
          <span />
        </li>
      ))}
    </ul>
  );
}
