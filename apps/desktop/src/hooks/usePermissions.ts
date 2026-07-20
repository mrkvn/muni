/**
 * Phase 9 — Wrappers around the macOS permission IPC commands. The
 * onboarding wizard uses these to render a status pill, fire the
 * system prompt, and poll until the user grants the permission via
 * System Settings.
 *
 * All three permission steps (Microphone, Accessibility, Input
 * Monitoring) share the same shape: an initial probe + a `request`
 * action + a `startPolling` interval that cleans up on step exit.
 * macOS doesn't fire callbacks when the user toggles a TCC switch in
 * System Settings, so polling is the only reliable way to detect a
 * grant performed outside the app.
 */
import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useRef, useState } from "react";

export type MicrophoneStatus =
  | "notDetermined"
  | "restricted"
  | "denied"
  | "authorized";

export type InputMonitoringStatus =
  | "notDetermined"
  | "denied"
  | "granted"
  | "unknown";

export type SystemSettingsTarget =
  | "microphone"
  | "accessibility"
  | "input-monitoring";

export async function readMicrophoneStatus(): Promise<MicrophoneStatus> {
  return invoke<MicrophoneStatus>("microphone_status");
}

export async function requestMicrophoneAccess(): Promise<boolean> {
  return invoke<boolean>("request_microphone_access");
}

export async function readAccessibilityTrusted(): Promise<boolean> {
  return invoke<boolean>("is_accessibility_trusted");
}

export async function promptAccessibility(): Promise<boolean> {
  return invoke<boolean>("prompt_accessibility");
}

export async function readInputMonitoringStatus(): Promise<InputMonitoringStatus> {
  return invoke<InputMonitoringStatus>("input_monitoring_status");
}

export async function requestInputMonitoringAccess(): Promise<InputMonitoringStatus> {
  return invoke<InputMonitoringStatus>("request_input_monitoring");
}

export async function openSystemSettings(
  target: SystemSettingsTarget,
): Promise<void> {
  await invoke("open_system_settings", { target });
}

export async function completeOnboarding(): Promise<void> {
  await invoke("complete_onboarding");
}

/**
 * Register (or unregister) Muni as a macOS Login Item. Backed by
 * `SMAppService` server-side, so it never shows an Automation prompt —
 * the app registers itself, with no second app to authorize. Used by
 * both the onboarding step and the Settings → General switch.
 */
export async function setLaunchAtLogin(enabled: boolean): Promise<void> {
  await invoke("set_launch_at_login", { enabled });
}

/**
 * Plan 039 task 39(c) — raw read of the persisted `general.launch_at_login`
 * preference, with no default-merge. `null` means the key has never been
 * written (a genuinely fresh store); any other value is an explicit prior
 * choice — including an opt-out — that must not be silently overwritten.
 * Used by the onboarding wizard's `LaunchAtLoginStep` to decide whether its
 * default-on lock-in should run on this mount.
 */
export async function getStoredLaunchAtLoginPref(): Promise<boolean | null> {
  const value = await invoke<boolean | null>("get_stored_launch_at_login_pref");
  return value ?? null;
}

/**
 * Feature 033 (Phase 3, task 21) — activation-funnel telemetry. Frontend
 * events route through the SAME durable Rust PostHog queue the backend uses
 * (no `posthog-js`), so the funnel shares one transport, one install-UUID
 * distinct_id, and one offline buffer.
 *
 * The Rust `telemetry_track` command validates `event` against a fixed
 * vocabulary and is a no-op when the analytics toggle is off — so this is
 * always safe to call unconditionally. Fire-and-forget: errors are swallowed
 * (a telemetry failure must never surface to the user or break the wizard).
 */
type FunnelPermission = "microphone" | "accessibility" | "input_monitoring";

export function trackOnboardingStarted(): void {
  void invoke("telemetry_track", { event: "onboarding_started" }).catch(
    () => {},
  );
}

export function trackPermissionGranted(permission: FunnelPermission): void {
  void invoke("telemetry_track", {
    event: "permission_granted",
    permission,
  }).catch(() => {});
}

// Default polling interval — 500ms is fast enough to feel reactive when
// the user toggles a TCC switch in System Settings, slow enough that an
// always-on interval costs nothing measurable.
const POLL_INTERVAL_MS = 500;

/**
 * Internal helper: builds a polling state machine for any of the three
 * TCC-style permissions. The hook owns the interval ref + cleanup; the
 * caller decides when polling is active via `useEffect(startPolling)`.
 *
 * No timeout cap — the caller's `useEffect` cleanup is the lifecycle.
 * If the user takes 5 minutes to find the right toggle in System
 * Settings, the wizard still notices when they finally flip it.
 */
function usePermissionState<T>(
  read: () => Promise<T>,
  initial: T,
): {
  value: T;
  loading: boolean;
  refresh: () => Promise<void>;
  set: (value: T) => void;
  startPolling: (intervalMs?: number) => () => void;
  /** Set when the most recent read failed (mount, `refresh()`, or a poll
   * tick) — never thrown, so callers can render a "check failed" pill
   * instead of leaving an unhandled rejection (plan 039 task 61a). */
  error: string | null;
} {
  const [value, setValue] = useState<T>(initial);
  const [loading, setLoading] = useState<boolean>(true);
  const [error, setError] = useState<string | null>(null);
  const intervalRef = useRef<number | null>(null);
  const readRef = useRef(read);
  readRef.current = read;

  useEffect(() => {
    let cancelled = false;
    void readRef
      .current()
      .then((next) => {
        if (cancelled) return;
        setValue(next);
        setError(null);
      })
      .catch((e: unknown) => {
        if (cancelled) return;
        setError(`Check failed: ${String(e)}`);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const stopPolling = useCallback(() => {
    if (intervalRef.current !== null) {
      window.clearInterval(intervalRef.current);
      intervalRef.current = null;
    }
  }, []);

  // Always release the interval on unmount, regardless of caller
  // discipline.
  useEffect(() => stopPolling, [stopPolling]);

  // Never rejects — a failed read resolves to the `error` state above
  // rather than propagating to the caller.
  const refresh = useCallback(async () => {
    try {
      const next = await readRef.current();
      setValue(next);
      setError(null);
    } catch (e) {
      setError(`Check failed: ${String(e)}`);
    }
  }, []);

  const startPolling = useCallback(
    (intervalMs?: number) => {
      stopPolling();
      intervalRef.current = window.setInterval(() => {
        readRef
          .current()
          .then((next) => {
            setValue(next);
            setError(null);
          })
          .catch((e: unknown) => {
            setError(`Check failed: ${String(e)}`);
          });
      }, intervalMs ?? POLL_INTERVAL_MS);
      return stopPolling;
    },
    [stopPolling],
  );

  return { value, loading, refresh, set: setValue, startPolling, error };
}

/**
 * Microphone status with a `request` that re-reads the status after
 * the OS prompt resolves, plus polling so a grant performed via
 * System Settings (which forces a "Quit & Reopen" dialog in
 * production) is detected without the user having to click anything
 * inside the wizard.
 */
export function useMicrophonePermission(): {
  status: MicrophoneStatus;
  loading: boolean;
  request: () => Promise<MicrophoneStatus>;
  refresh: () => Promise<void>;
  startPolling: (intervalMs?: number) => () => void;
  error: string | null;
} {
  const state = usePermissionState<MicrophoneStatus>(
    readMicrophoneStatus,
    "notDetermined",
  );

  const request = useCallback(async () => {
    await requestMicrophoneAccess();
    const next = await readMicrophoneStatus();
    state.set(next);
    return next;
  }, [state]);

  return {
    status: state.value,
    loading: state.loading,
    request,
    refresh: state.refresh,
    startPolling: state.startPolling,
    error: state.error,
  };
}

/**
 * Accessibility-trusted state with `prompt` and unbounded polling
 * (cleanup happens via `useEffect`, never a hard timeout cap).
 */
export function useAccessibilityPermission(): {
  trusted: boolean;
  loading: boolean;
  prompt: () => Promise<boolean>;
  refresh: () => Promise<void>;
  startPolling: (intervalMs?: number) => () => void;
  error: string | null;
} {
  const state = usePermissionState<boolean>(readAccessibilityTrusted, false);

  const prompt = useCallback(async () => {
    const next = await promptAccessibility();
    state.set(next);
    return next;
  }, [state]);

  return {
    trusted: state.value,
    loading: state.loading,
    prompt,
    refresh: state.refresh,
    startPolling: state.startPolling,
    error: state.error,
  };
}

/**
 * Input Monitoring (kIOHIDRequestTypeListenEvent) status. Required
 * for the modifier-only hotkey listener that drives push-to-talk.
 *
 * `request` triggers the system "Keystroke Receiving" prompt the
 * first time it's called for this app; subsequent calls short-circuit
 * to the cached TCC decision (macOS does not re-prompt). Recovery
 * after a missed prompt: open System Settings → Privacy & Security →
 * Input Monitoring and toggle Muni on manually.
 */
export function useInputMonitoringPermission(): {
  status: InputMonitoringStatus;
  loading: boolean;
  request: () => Promise<InputMonitoringStatus>;
  refresh: () => Promise<void>;
  startPolling: (intervalMs?: number) => () => void;
  error: string | null;
} {
  const state = usePermissionState<InputMonitoringStatus>(
    readInputMonitoringStatus,
    "notDetermined",
  );

  const request = useCallback(async () => {
    const next = await requestInputMonitoringAccess();
    state.set(next);
    return next;
  }, [state]);

  return {
    status: state.value,
    loading: state.loading,
    request,
    refresh: state.refresh,
    startPolling: state.startPolling,
    error: state.error,
  };
}
