/**
 * Phase 10 — History tab. Reads the SQLite-backed `dictation_records`
 * table via the `history_*` IPC commands; new rows land here
 * automatically because the underlying hook listens for
 * `transcript://final` events.
 */
import { Copy, Trash2 } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { useLocation } from "react-router-dom";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Separator } from "@/components/ui/separator";
import { useAppDisplayName } from "@/hooks/useAppDisplayName";
import { type DictationRecord, useHistory } from "@/hooks/useHistory";

/**
 * Plan 039 task 61(d) — how often relative timestamps ("3m ago") are
 * recomputed. Coarse enough to cost nothing measurable, fine enough that
 * the pane never visibly freezes on a stale relative time.
 */
const RELATIVE_TIMESTAMP_REFRESH_MS = 30_000;

export function SettingsHistory() {
  const { records, loading, error, remove, wipe } = useHistory();
  const [confirmingWipe, setConfirmingWipe] = useState(false);

  // Relative timestamps go stale the instant they're memoized — without a
  // periodic re-render they'd freeze at whatever relative time first
  // rendered until some unrelated prop change happened to force a
  // recompute. Tick only while this pane is the active Settings tab
  // (every pane stays mounted-but-hidden, so gating on visibility avoids a
  // background timer ticking away in every hidden pane).
  const { pathname } = useLocation();
  const isVisible = pathname === "/settings/history";
  const [relativeTimeTick, setRelativeTimeTick] = useState(0);
  useEffect(() => {
    if (!isVisible) return;
    const id = window.setInterval(
      () => setRelativeTimeTick((t) => t + 1),
      RELATIVE_TIMESTAMP_REFRESH_MS,
    );
    return () => window.clearInterval(id);
  }, [isVisible]);

  const onCopy = useCallback(async (record: DictationRecord) => {
    try {
      await navigator.clipboard.writeText(record.cleanedText);
      toast.success("Copied to clipboard");
    } catch (e) {
      toast.error(`Copy failed: ${String(e)}`);
    }
  }, []);

  const onDelete = useCallback(
    async (record: DictationRecord) => {
      try {
        await remove(record.id);
      } catch (e) {
        toast.error(`Delete failed: ${String(e)}`);
      }
    },
    [remove],
  );

  const onWipeConfirmed = useCallback(async () => {
    try {
      await wipe();
      toast.success("History cleared");
    } catch (e) {
      toast.error(`Wipe failed: ${String(e)}`);
    } finally {
      setConfirmingWipe(false);
    }
  }, [wipe]);

  return (
    <section aria-labelledby="history-heading" className="flex flex-col gap-4">
      <header className="flex items-center justify-between gap-3">
        <h1 id="history-heading" className="text-xl font-semibold tracking-tight">
          History
        </h1>
        <Button
          variant="outline"
          size="sm"
          onClick={() => setConfirmingWipe(true)}
          disabled={records.length === 0 || loading}
        >
          Wipe history
        </Button>
      </header>

      {error ? (
        <div
          role="alert"
          className="rounded-lg border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive"
        >
          {error}
        </div>
      ) : null}

      {loading ? (
        <p className="text-muted-foreground text-sm" role="status">
          Loading…
        </p>
      ) : records.length === 0 ? (
        <EmptyState />
      ) : (
        <ul
          aria-label="Dictation history"
          className="flex flex-col gap-2"
        >
          {records.map((record) => (
            <li key={record.id}>
              <HistoryRow
                record={record}
                onCopy={onCopy}
                onDelete={onDelete}
                relativeTimeTick={relativeTimeTick}
              />
            </li>
          ))}
        </ul>
      )}

      <Dialog open={confirmingWipe} onOpenChange={setConfirmingWipe}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Wipe history?</DialogTitle>
            <DialogDescription>This can't be undone.</DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setConfirmingWipe(false)}>
              Cancel
            </Button>
            <Button variant="destructive" onClick={() => void onWipeConfirmed()}>
              Wipe history
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </section>
  );
}

function EmptyState() {
  return (
    <div
      role="status"
      className="flex min-h-[120px] items-center justify-center rounded-lg border border-dashed border-border p-6 text-center"
    >
      <p className="text-muted-foreground text-sm">No dictations yet</p>
    </div>
  );
}

interface HistoryRowProps {
  record: DictationRecord;
  onCopy: (record: DictationRecord) => Promise<void>;
  onDelete: (record: DictationRecord) => Promise<void>;
  /** Bumped every {@link RELATIVE_TIMESTAMP_REFRESH_MS} while the History
   * pane is visible — included as a memo dependency purely to force
   * `formatTimestamp` to recompute on a schedule; its value is otherwise
   * unused. */
  relativeTimeTick: number;
}

function HistoryRow({ record, onCopy, onDelete, relativeTimeTick }: HistoryRowProps) {
  const formatted = useMemo(
    () => formatTimestamp(record.createdAt),
    [record.createdAt, relativeTimeTick],
  );
  const displayName = useAppDisplayName(record.targetAppBundleId);
  const target =
    displayName ?? record.targetAppBundleId ?? "Unknown app";

  return (
    <article
      className="flex flex-col gap-2 rounded-lg border border-border bg-card/40 p-3"
      aria-label={`Dictation from ${formatted}`}
    >
      <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
        <div className="flex flex-wrap items-center gap-2">
          <time dateTime={new Date(record.createdAt * 1000).toISOString()}>
            {formatted}
          </time>
          <Separator orientation="vertical" className="h-3" />
          <span title="Target application">{target}</span>
          <Separator orientation="vertical" className="h-3" />
          <span>
            {record.charCount} char{record.charCount === 1 ? "" : "s"}
          </span>
        </div>
        <div className="flex items-center gap-1">
          <Button
            variant="ghost"
            size="icon-sm"
            onClick={() => void onCopy(record)}
            aria-label="Copy cleaned text"
          >
            <Copy className="size-4" />
          </Button>
          <Button
            variant="ghost"
            size="icon-sm"
            onClick={() => void onDelete(record)}
            aria-label="Delete dictation"
          >
            <Trash2 className="size-4" />
          </Button>
        </div>
      </div>
      <p className="whitespace-pre-wrap text-sm">{record.cleanedText}</p>
      {record.rawText !== record.cleanedText ? (
        <details className="group">
          <summary className="cursor-pointer text-xs text-muted-foreground hover:text-foreground">
            Show raw transcript
          </summary>
          <p className="mt-1 whitespace-pre-wrap rounded bg-muted/40 px-2 py-1 text-xs text-muted-foreground">
            {record.rawText}
          </p>
        </details>
      ) : null}
    </article>
  );
}

/**
 * Render a Unix-second timestamp relative to "now" for the recent past
 * ("3m ago"), absolute thereafter ("Mar 12, 14:32"). Mirrors what
 * users expect from a chat-style timeline without dragging in a
 * date-fns dependency.
 */
function formatTimestamp(unixSeconds: number): string {
  const now = Date.now();
  const then = unixSeconds * 1000;
  const deltaSec = Math.max(0, Math.floor((now - then) / 1000));

  if (deltaSec < 45) return "just now";
  if (deltaSec < 90) return "1 minute ago";
  const deltaMin = Math.round(deltaSec / 60);
  if (deltaMin < 45) return `${deltaMin} minutes ago`;
  if (deltaMin < 90) return "1 hour ago";
  const deltaHr = Math.round(deltaMin / 60);
  if (deltaHr < 24) return `${deltaHr} hours ago`;

  const date = new Date(then);
  return date.toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}
