/**
 * Phase 8 — Typed wrapper around the `settings_get` / `settings_set` IPC
 * commands. Each entry in `SETTINGS_KEYS` is paired with the JS-side
 * type and default that mirrors Swift v1's `@AppStorage` declaration.
 *
 * The Rust backend already enforces an allowlist (`settings::is_allowed_key`)
 * + supplies a default when the store is empty; the frontend mirrors the
 * defaults so optimistic UI doesn't flash empty values on first mount.
 */
import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useState } from "react";

import { useTauriListen } from "./useTauriListen";

/** Mirrors `settings::EVENT_SETTINGS_CHANGED` (Rust). */
const SETTINGS_CHANGED_EVENT = "settings://changed";

/**
 * Wire shape of the `settings://changed` event payload. Rust emits
 * `{ key: <string>, value: <json> }` on every successful `settings_set`.
 */
type SettingsChangedPayload = { key: string; value: unknown };

/**
 * Validate a `settings://changed` payload before applying it (plan 039 task
 * 44), mirroring the defensive shape-check `useSessionState` runs on its own
 * event. A malformed payload (wrong type, missing `key`, or absent `value`)
 * is dropped rather than trusted — a stray or corrupted broadcast must never
 * clobber a hook's value with `undefined`.
 */
function isSettingsChangedPayload(
  payload: unknown,
): payload is SettingsChangedPayload {
  return (
    typeof payload === "object" &&
    payload !== null &&
    "key" in payload &&
    typeof (payload as { key: unknown }).key === "string" &&
    "value" in payload
  );
}

/**
 * Feature 037 — the hotkey binding shapes. These mirror the Rust serde
 * representations in `hotkey_binding.rs` **byte-for-byte** (tag `kind`,
 * lowercase modifier tokens, `button`/`key` fields) so a value written by the
 * `set_dictation_hotkey` / `set_repaste_hotkey` commands round-trips through the
 * store back into these types with no coercion. A drift here silently breaks
 * seeding — keep them in lockstep with the Rust enums.
 *
 * Feature 038 — the dictation trigger gains an optional `key`. A modifier-only
 * chord omits it; a keyed push-to-talk binding (e.g. ⌃⇧R) carries it. It is
 * `key?: string` (optional/undefined, never `null`) to mirror Rust's
 * `#[serde(skip_serializing_if = "Option::is_none")]` — modifier-only bindings
 * serialize with no `key` member at all, not `"key": null`.
 */
export type HotkeyModifier = "control" | "option" | "command" | "shift" | "fn";

/**
 * The main dictation trigger: a modifier-only chord, or a modifier+key combo
 * (push-to-talk) when `key` is present.
 *
 * Plan 039 task 54 — `displayKey` is the `KeyboardEvent.key` captured at
 * record time, a layout-aware display hint for a keyed binding (e.g. `"a"` on
 * AZERTY where `key` is the physical `"KeyQ"`). Matching is always by `key`;
 * `displayKey` only feeds chip/label/aria rendering. Optional/undefined
 * (never `null`) to mirror Rust's `skip_serializing_if` — a binding with no
 * hint (or recorded before this field existed) serializes with no member.
 */
export type DictationBinding = {
  kind: "modifiers";
  mods: HotkeyModifier[];
  key?: string;
  displayKey?: string;
};

/** The re-paste hotkey: a modifier(s)+key combo. `null` at rest means disabled. */
export type RepasteBinding = {
  mods: HotkeyModifier[];
  key: string;
  /** See {@link DictationBinding.displayKey}. */
  displayKey?: string;
};

export type SettingsKey =
  | "history.retention_days"
  | "general.launch_at_login"
  | "hotkey.dictation_binding"
  | "hotkey.repaste_binding"
  | "telemetry.crash_reporting"
  | "telemetry.analytics";

export type SettingsValueOf<K extends SettingsKey> = K extends
  | "general.launch_at_login"
  | "telemetry.crash_reporting"
  | "telemetry.analytics"
  ? boolean
  : K extends "history.retention_days"
    ? number
    : K extends "hotkey.dictation_binding"
      ? DictationBinding
      : K extends "hotkey.repaste_binding"
        ? RepasteBinding | null
        : never;

export const SETTINGS_DEFAULTS: { [K in SettingsKey]: SettingsValueOf<K> } = {
  "history.retention_days": 30,
  // Plan 039 task 39(b) — mirrors the Rust-side `settings::default_for`
  // (default OFF: the onboarding wizard's own Launch-at-Login step is
  // where the user opts in explicitly). Any surface that reads this key
  // via `useSetting` before the onboarding wizard's dedicated flow runs
  // must show the same "off" default the backend hands out, not a stale
  // optimistic `true` that would flash on before the real value loads.
  "general.launch_at_login": false,
  // Feature 037 — mirror the Rust `default_dictation()` / `default_repaste()`
  // so optimistic UI shows the shipped bindings (Ctrl+Option, Ctrl+Cmd+V)
  // before the first `settings_get` resolves.
  "hotkey.dictation_binding": {
    kind: "modifiers",
    mods: ["control", "option"],
  },
  "hotkey.repaste_binding": { mods: ["control", "command"], key: "KeyV" },
  // Feature 033 — telemetry consent defaults ON (informed opt-out). The
  // baked DSN/key is absent in dev/staging, so these only have effect in
  // prod builds even when true.
  "telemetry.crash_reporting": true,
  "telemetry.analytics": true,
};

/**
 * Hook that loads a single settings entry on mount and exposes a
 * `set` callback. Optimistic update on `set` so the UI feels instant
 * even if the Tauri Store flush takes a few ms.
 */
export function useSetting<K extends SettingsKey>(
  key: K,
): {
  value: SettingsValueOf<K>;
  set: (next: SettingsValueOf<K>) => Promise<void>;
  loading: boolean;
  error: string | null;
} {
  const [value, setValue] = useState<SettingsValueOf<K>>(
    SETTINGS_DEFAULTS[key],
  );
  const [loading, setLoading] = useState<boolean>(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    invoke<SettingsValueOf<K> | null>("settings_get", { key })
      .then((result) => {
        if (cancelled) return;
        // The Rust side returns `null` only for unknown keys — the
        // allowlist would have rejected them. Defensive fallback to
        // the JS-side default keeps the UI predictable.
        setValue(result === null ? SETTINGS_DEFAULTS[key] : result);
      })
      .catch((e) => {
        if (cancelled) return;
        setError(String(e));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [key]);

  // Stay in sync when this key is written elsewhere — notably the Settings
  // page reflecting a toggle the onboarding window persisted. `useSetting`
  // otherwise reads once at mount and would show a stale value.
  useTauriListen<unknown>(SETTINGS_CHANGED_EVENT, (payload) => {
    if (!isSettingsChangedPayload(payload)) {
      console.warn(
        "useSettings: dropping malformed settings://changed payload",
        payload,
      );
      return;
    }
    if (payload.key === key) setValue(payload.value as SettingsValueOf<K>);
  });

  const set = useCallback(
    async (next: SettingsValueOf<K>) => {
      const previous = value;
      setValue(next);
      try {
        await invoke("settings_set", { key, value: next });
        setError(null);
      } catch (e) {
        setValue(previous);
        setError(String(e));
        throw e;
      }
    },
    [key, value],
  );

  return { value, set, loading, error };
}
