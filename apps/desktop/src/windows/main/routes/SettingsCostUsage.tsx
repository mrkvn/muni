/**
 * Feature 005 — "Cost & Usage" Settings tab.
 *
 * Always visible: a one-line "estimated" disclaimer under the heading,
 * then a per-month rollup (in the user's local timezone) — current month
 * expanded, prior months collapsed.
 *
 * The "Prices last reviewed … may differ ~1–2% from provider invoices"
 * freshness footer is parked until a live pricing API is wired in (see
 * .claude/backlogs.md); `lastPricedSuccessAt` is still carried on the IPC
 * payload so it can be restored without a backend change.
 *
 * Dev-only (`import.meta.env.DEV`): a per-model breakdown plus an
 * "Open prices" viewer that dumps `price_history` for the current
 * month. Production builds drop the dev section entirely.
 */
import { ChevronRight } from "lucide-react";
import { useMemo } from "react";

import { useDevPrices } from "@/hooks/useDevPrices";
import {
  type MonthlySummary,
  type ProviderSummary,
  useUsageSummary,
} from "@/hooks/useUsageSummary";

const MONTH_NAMES = [
  "January",
  "February",
  "March",
  "April",
  "May",
  "June",
  "July",
  "August",
  "September",
  "October",
  "November",
  "December",
];

/**
 * Format a `YYYY-MM` string as a human-readable header ("May 2026"). The
 * backend buckets spend by the machine's local timezone, which is the
 * user's own clock, so no timezone suffix is shown. Falls back to the
 * raw value when parsing fails so a future schema change doesn't paint a
 * blank header.
 */
function formatMonth(yearMonth: string): string {
  const match = /^(\d{4})-(\d{2})$/.exec(yearMonth);
  if (!match) return yearMonth;
  const [, year, monthStr] = match;
  const monthIdx = Number.parseInt(monthStr, 10) - 1;
  const name = MONTH_NAMES[monthIdx] ?? monthStr;
  return `${name} ${year}`;
}

function formatUsd(value: number | null): string {
  if (value === null) return "—";
  // Four decimals so sub-cent providers (Deepgram nova-3 at $8e-5/s,
  // Gemini LID at fractions of a cent per call) don't all round to
  // $0.00 in the user-facing rollup. Tighter than the dev pane's
  // raw figure but loose enough to communicate non-zero spend.
  return `$${value.toFixed(4)}`;
}

function totalUsd(rows: ProviderSummary[]): number | null {
  let any = false;
  let sum = 0;
  for (const row of rows) {
    if (row.totalUsd !== null) {
      sum += row.totalUsd;
      any = true;
    }
  }
  return any ? sum : null;
}

export function SettingsCostUsage() {
  const { summary, loading, error } = useUsageSummary();

  return (
    <section aria-labelledby="cost-usage-heading" className="flex flex-col gap-4">
      <header className="flex flex-col gap-1">
        <h1
          id="cost-usage-heading"
          className="text-xl font-semibold tracking-tight"
        >
          Cost &amp; Usage
        </h1>
        <p className="text-sm text-muted-foreground">All costs are estimated.</p>
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
        <p className="text-sm text-muted-foreground">Loading…</p>
      ) : summary ? (
        <>
          {summary.months.map((month, index) => (
            <MonthSection
              key={month.yearMonth}
              month={month}
              isCurrent={index === 0}
            />
          ))}
        </>
      ) : null}

      {import.meta.env.DEV ? <DevDetails /> : null}
    </section>
  );
}

/**
 * One month's block, rendered as a native `<details>` disclosure so it's
 * keyboard-operable for free. The `<summary>` is the always-visible
 * header row (month name + grand total) that tints on hover; expanding it
 * reveals the per-provider rows. The current month (`isCurrent`) starts open; past
 * months start collapsed so the panel doesn't flood with history. The
 * empty state reads "this month" for the current month rather than
 * implying a past month had no activity.
 */
function MonthSection({
  month,
  isCurrent,
}: {
  month: MonthlySummary;
  isCurrent: boolean;
}) {
  const grandTotal = useMemo(
    () => totalUsd(month.providerTotals),
    [month.providerTotals],
  );

  return (
    <details className="group flex flex-col gap-2" open={isCurrent}>
      <summary className="flex cursor-pointer list-none items-center gap-2 rounded-md px-2 py-2 transition-colors hover:bg-accent/50 [&::-webkit-details-marker]:hidden">
        <ChevronRight
          aria-hidden="true"
          className="size-4 shrink-0 text-muted-foreground transition-transform group-open:rotate-90"
        />
        <span className="text-[15px] font-medium">
          {formatMonth(month.yearMonth)}
        </span>
        {/* `ml-auto` pushes the total to the right edge so it shares the
            cost column's right edge with the per-provider rows below. */}
        <span className="ml-auto text-[15px] font-semibold tabular-nums">
          {formatUsd(grandTotal)}
        </span>
      </summary>

      {month.providerTotals.length === 0 ? (
        // Indented to `pl-8` so it lines up under the month name (chevron
        // width + gap), not under the chevron.
        <p className="pr-2 pl-8 text-sm text-muted-foreground">
          {isCurrent
            ? "No API calls recorded this month yet."
            : "No API calls recorded."}
        </p>
      ) : (
        <ul
          aria-label={`Per-provider totals for ${formatMonth(month.yearMonth)}`}
          className="flex flex-col gap-1"
        >
          {month.providerTotals.map((row) => (
            // `pl-8` aligns the provider name under the month name (past the
            // chevron); the cost is the trailing element so every month's
            // dollar figures share a right edge, with the call count to its
            // left.
            <li
              key={row.provider}
              className="flex items-center justify-between rounded-md py-1.5 pr-2 pl-8 hover:bg-accent/40"
            >
              <span className="text-sm">{row.provider}</span>
              <span className="text-sm tabular-nums">
                <span className="mr-2 text-xs text-muted-foreground">
                  ({row.callCount} call{row.callCount === 1 ? "" : "s"})
                </span>
                {formatUsd(row.totalUsd)}
              </span>
            </li>
          ))}
        </ul>
      )}
    </details>
  );
}

/**
 * Dev-only expansion: per-model rollup + raw `price_history` rows.
 * Hidden from production builds via the `import.meta.env.DEV` gate
 * in the parent component.
 */
function DevDetails() {
  const { prices, perModel, loading, error } = useDevPrices();
  return (
    <details className="rounded-lg border border-dashed border-border/60 px-3 py-2">
      <summary className="cursor-pointer text-sm font-medium">
        Dev: per-model + price history
      </summary>
      <div className="mt-3 flex flex-col gap-4">
        {error ? (
          <div role="alert" className="text-xs text-destructive">
            {error}
          </div>
        ) : null}
        {loading ? (
          <p className="text-xs text-muted-foreground">Loading dev details…</p>
        ) : (
          <>
            <div>
              <h3 className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
                Per-model totals
              </h3>
              {perModel.length === 0 ? (
                <p className="mt-1 text-xs text-muted-foreground">
                  No rows for this month.
                </p>
              ) : (
                <table className="mt-2 w-full text-xs tabular-nums">
                  <thead className="text-muted-foreground">
                    <tr>
                      <th className="text-left">Provider/Model</th>
                      <th className="text-right">Calls</th>
                      <th className="text-right">Audio s</th>
                      <th className="text-right">In tok</th>
                      <th className="text-right">Out tok</th>
                      <th className="text-right">Cost</th>
                    </tr>
                  </thead>
                  <tbody>
                    {perModel.map((row) => (
                      <tr key={`${row.provider}/${row.model}`}>
                        <td className="text-left">
                          {row.provider} / {row.model}
                        </td>
                        <td className="text-right">{row.callCount}</td>
                        <td className="text-right">
                          {row.audioSeconds !== null
                            ? row.audioSeconds.toFixed(1)
                            : "—"}
                        </td>
                        <td className="text-right">
                          {row.inputTokens ?? "—"}
                        </td>
                        <td className="text-right">
                          {row.outputTokens ?? "—"}
                        </td>
                        <td className="text-right">
                          {row.totalUsd !== null
                            ? `$${row.totalUsd.toFixed(4)}`
                            : "—"}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              )}
            </div>

            <div>
              <h3 className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
                Price history (current month)
              </h3>
              {prices.length === 0 ? (
                <p className="mt-1 text-xs text-muted-foreground">
                  No price rows seeded yet.
                </p>
              ) : (
                <ul className="mt-2 flex flex-col gap-0.5 text-xs tabular-nums">
                  {prices.map((row) => (
                    <li key={`${row.provider}/${row.model}`}>
                      <span className="font-medium">
                        {row.provider}/{row.model}
                      </span>{" "}
                      <span className="text-muted-foreground">{row.kind}</span>{" "}
                      {row.usdPerSecond !== null ? (
                        <span>${row.usdPerSecond.toExponential(2)}/s</span>
                      ) : null}
                      {row.usdPerInputToken !== null ? (
                        <span>
                          {" "}
                          in ${row.usdPerInputToken.toExponential(2)}/tok
                        </span>
                      ) : null}
                      {row.usdPerOutputToken !== null ? (
                        <span>
                          {" "}
                          out ${row.usdPerOutputToken.toExponential(2)}/tok
                        </span>
                      ) : null}
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </>
        )}
      </div>
    </details>
  );
}
