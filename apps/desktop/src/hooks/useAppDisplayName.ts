/**
 * Resolve a macOS bundle id (e.g. `com.mitchellh.ghostty`) to its
 * user-visible display name (`Ghostty`) via the `app_display_name`
 * IPC. Results are cached in module scope so re-rendering the
 * History list doesn't re-hit Launch Services for every row.
 */
import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";

const cache = new Map<string, string | null>();
const inflight = new Map<string, Promise<string | null>>();

async function lookup(bundleId: string): Promise<string | null> {
  if (cache.has(bundleId)) return cache.get(bundleId) ?? null;
  const existing = inflight.get(bundleId);
  if (existing) return existing;

  const p = invoke<string | null>("app_display_name", { bundleId })
    .then((name) => {
      cache.set(bundleId, name ?? null);
      return name ?? null;
    })
    .catch(() => {
      cache.set(bundleId, null);
      return null;
    })
    .finally(() => {
      inflight.delete(bundleId);
    });
  inflight.set(bundleId, p);
  return p;
}

/**
 * Returns the display name for `bundleId`, or `null` while loading /
 * when the app isn't installed. Pass `null` to opt out (the hook then
 * always returns `null`).
 */
export function useAppDisplayName(bundleId: string | null): string | null {
  const [name, setName] = useState<string | null>(
    bundleId ? (cache.get(bundleId) ?? null) : null,
  );

  useEffect(() => {
    if (!bundleId) {
      setName(null);
      return;
    }
    let cancelled = false;
    if (cache.has(bundleId)) {
      setName(cache.get(bundleId) ?? null);
      return;
    }
    void lookup(bundleId).then((resolved) => {
      if (!cancelled) setName(resolved);
    });
    return () => {
      cancelled = true;
    };
  }, [bundleId]);

  return name;
}
