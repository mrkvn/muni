/**
 * Feature 005 — dev-only reader for the `price_history` table. Powers
 * the "Open prices" viewer in the Cost & Usage panel's dev pane so
 * `cargo run --bin muni` can verify the bootstrap seed and the
 * refresher's latest fetch without touching SQLite directly.
 *
 * Production builds short-circuit to an empty list so the IPC call
 * never fires and the panel hides the section behind
 * `import.meta.env.DEV`.
 */
import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useState } from "react";

/** Mirrors `usage_store::PriceRow`. */
export interface PriceRow {
  effectiveMonth: string;
  provider: string;
  model: string;
  /** `"per_audio_second"` or `"per_token"`. */
  kind: string;
  usdPerSecond: number | null;
  usdPerInputToken: number | null;
  usdPerOutputToken: number | null;
  sourceUrl: string | null;
  /** Unix epoch seconds (UTC). */
  fetchedAt: number;
}

/** Mirrors `usage_store::ModelSummary`. */
export interface ModelSummary {
  provider: string;
  model: string;
  totalUsd: number | null;
  callCount: number;
  audioSeconds: number | null;
  inputTokens: number | null;
  outputTokens: number | null;
}

interface UseDevPricesResult {
  prices: PriceRow[];
  perModel: ModelSummary[];
  loading: boolean;
  error: string | null;
  refresh: () => Promise<void>;
}

export function useDevPrices(): UseDevPricesResult {
  const [prices, setPrices] = useState<PriceRow[]>([]);
  const [perModel, setPerModel] = useState<ModelSummary[]>([]);
  const [loading, setLoading] = useState<boolean>(true);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    if (!import.meta.env.DEV) {
      setLoading(false);
      return;
    }
    try {
      const [rows, models] = await Promise.all([
        invoke<PriceRow[]>("usage_prices_list_current_month"),
        invoke<ModelSummary[]>("usage_summary_get_per_model_current_month"),
      ]);
      setPrices(rows);
      setPerModel(models);
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return { prices, perModel, loading, error, refresh };
}
