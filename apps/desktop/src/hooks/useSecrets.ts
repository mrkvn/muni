/**
 * Phase 8 — Helpers for the API-keys tab. The hook owns the
 * "is the key set?" probe so the UI can show a "key configured"
 * indicator without ever pulling the value back across IPC. Mirrors
 * Swift v1's `APIKeysTab` `load()` plus `keychain.get` calls.
 *
 * The actual key value never round-trips to JS — the Save action
 * sends the user's typed input one-way to Rust, and Test sends it
 * one-way to `validate_api_key`. This keeps secrets out of any
 * webview-resident state.
 */
import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useState } from "react";

import { useTauriListen } from "@/hooks/useTauriListen";

export type SecretService = "deepgram" | "groq" | "gemini" | "gladia";

const PROBE_COMMAND: Record<SecretService, string> = {
  deepgram: "secrets_is_deepgram_set",
  groq: "secrets_is_groq_set",
  gemini: "secrets_is_gemini_set",
  gladia: "secrets_is_gladia_set",
};

const SAVE_COMMAND: Record<SecretService, string> = {
  deepgram: "secrets_set_deepgram",
  groq: "secrets_set_groq",
  gemini: "secrets_set_gemini",
  gladia: "secrets_set_gladia",
};

const DELETE_COMMAND: Record<SecretService, string> = {
  deepgram: "secrets_delete_deepgram",
  groq: "secrets_delete_groq",
  gemini: "secrets_delete_gemini",
  gladia: "secrets_delete_gladia",
};

/**
 * Fired by Rust after any successful secret set/delete. The Settings →
 * API Keys probe must listen for this because SettingsLayout mounts ALL
 * routes at app launch (Phase 10 keep-mounted optimisation), which is
 * BEFORE the onboarding wizard even shows. Without a refresh signal,
 * the probe runs once with no keys, gets `false`, and stays stuck on
 * "Not set" even after the wizard saves the keys.
 */
const SECRETS_CHANGED_EVENT = "secrets://changed";

export function useSecretProbe(service: SecretService): {
  isSet: boolean;
  refresh: () => Promise<void>;
  loading: boolean;
  /** Set when the last probe failed — never thrown, so callers can render a
   * "check failed" pill instead of leaving the caller with an unhandled
   * rejection (plan 039 task 61a). */
  error: string | null;
} {
  const [isSet, setIsSet] = useState<boolean>(false);
  const [loading, setLoading] = useState<boolean>(true);
  const [error, setError] = useState<string | null>(null);

  // Never rejects — a probe failure resolves to an `error` state rather
  // than an unhandled rejection at any of this function's several
  // fire-and-forget call sites (mount effect, the changed-event listener).
  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const result = await invoke<boolean>(PROBE_COMMAND[service]);
      setIsSet(result);
      setError(null);
    } catch (e) {
      setIsSet(false);
      setError(`Check failed: ${String(e)}`);
    } finally {
      setLoading(false);
    }
  }, [service]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Auto-refresh whenever Rust signals a secret was set/deleted in
  // ANY window. `tauri::Emitter::emit` is window-broadcast, so the
  // onboarding-window save reaches the (mounted-but-hidden) main
  // window's probe and the Settings pill flips before the user ever
  // sees it.
  useTauriListen(SECRETS_CHANGED_EVENT, () => {
    void refresh();
  });

  return { isSet, refresh, loading, error };
}

export async function saveSecret(service: SecretService, value: string): Promise<void> {
  await invoke(SAVE_COMMAND[service], { value });
}

export async function deleteSecret(service: SecretService): Promise<void> {
  await invoke(DELETE_COMMAND[service]);
}

export async function validateApiKey(
  service: SecretService,
  key: string,
): Promise<boolean> {
  return invoke<boolean>("validate_api_key", { service, key });
}
