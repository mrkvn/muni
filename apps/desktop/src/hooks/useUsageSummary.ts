/**
 * Feature 005 — typed wrapper around the `usage_summary_list_all_months`
 * IPC command. The Rust side (apps/desktop/src-tauri/src/usage_store.rs)
 * owns the SQLite-backed `api_calls` table; the writer task lands a row
 * for every successful provider call. This hook re-fetches whenever a
 * dictation completes (subscribed via `history://changed`) so the
 * Cost & Usage panel reflects the latest spend without a manual refresh.
 */
import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useState } from "react";
import { friendlyInvokeError } from "../lib/friendlyInvokeError";
import { useTauriListen } from "./useTauriListen";

/** Mirrors `usage_store::ProviderSummary` (camelCase serde). */
export interface ProviderSummary {
  provider: string;
  /** `null` only when every row in the rollup had `cost_usd = NULL`. */
  totalUsd: number | null;
  callCount: number;
}

/** Mirrors `usage_store::MonthlySummary` — one month's per-provider rollup. */
export interface MonthlySummary {
  /** `YYYY-MM` in the machine's local timezone — the display month. */
  yearMonth: string;
  providerTotals: ProviderSummary[];
}

/** Mirrors `commands::UsageHistory`. */
export interface UsageSummary {
  /** Newest month first; `months[0]` is always the current local month. */
  months: MonthlySummary[];
  /** Unix epoch seconds (UTC) of the last successful prices fetch. */
  lastPricedSuccessAt: number | null;
}

/**
 * Same event the History tab listens to — fires after every
 * dictation row insert, which is also when a fresh batch of
 * `api_calls` rows have just landed for the press.
 */
const HISTORY_CHANGED_EVENT = "history://changed";

interface UseUsageSummaryResult {
  summary: UsageSummary | null;
  loading: boolean;
  error: string | null;
  refresh: () => Promise<void>;
}

export function useUsageSummary(): UseUsageSummaryResult {
  const [summary, setSummary] = useState<UsageSummary | null>(null);
  const [loading, setLoading] = useState<boolean>(true);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const next = await invoke<UsageSummary>("usage_summary_list_all_months");
      setSummary(next);
      setError(null);
    } catch (e) {
      setError(friendlyInvokeError(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useTauriListen<string>(HISTORY_CHANGED_EVENT, () => {
    void refresh();
  });

  return { summary, loading, error, refresh };
}
