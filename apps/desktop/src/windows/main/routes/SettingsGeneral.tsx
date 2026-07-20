/**
 * Phase 8 — General settings tab. Mirrors Swift v1's `GeneralTab`.
 *
 * Phase 10 wired the launch-at-login switch to `set_launch_at_login`,
 * which flips the OS launch agent and persists the preference in one
 * step. The other settings continue to use the generic `useSetting`
 * hook (Tauri Store round-trip with optimistic UI).
 *
 * Phase 11 added the Permissions card. It reuses the onboarding hooks
 * + IPC commands (`useMicrophonePermission` / `useAccessibilityPermission`
 * / `useInputMonitoringPermission`) to surface live OS permission state
 * outside the first-run wizard. This is the only place in Settings
 * where a denied macOS permission can be re-granted, and it's where
 * the error presenter routes loud `MicrophoneDenied` /
 * `AccessibilityDenied` / `InputMonitoringDenied` notifications.
 */
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { openUrl } from "@tauri-apps/plugin-opener";
import { ExternalLink } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { useLocation } from "react-router-dom";
import { toast } from "sonner";

import { SettingsSection } from "@/components/SettingsSection";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Slider } from "@/components/ui/slider";
import { Switch } from "@/components/ui/switch";
import {
  type MicrophoneStatus,
  openSystemSettings,
  useAccessibilityPermission,
  useInputMonitoringPermission,
  useMicrophonePermission,
} from "@/hooks/usePermissions";
import { useSetting } from "@/hooks/useSettings";

export function SettingsGeneral() {
  const launchAtLogin = useLaunchAtLogin();
  const retentionDays = useSetting("history.retention_days");
  const crashReporting = useSetting("telemetry.crash_reporting");
  const analytics = useSetting("telemetry.analytics");

  // Plan 039 task 60 — the slider must not fire a `settings_set` IPC write
  // on every drag tick. `retentionDisplay` is the LOCAL, uncommitted drag
  // position (`onValueChange`); the committed setting only advances via
  // `onValueCommit` (drag-end / keyboard step), which fires exactly once
  // per interaction. Stays in sync with the committed value whenever it
  // changes from elsewhere (e.g. another window, or the initial load).
  const [retentionDisplay, setRetentionDisplay] = useState<number>(
    retentionDays.value,
  );
  useEffect(() => {
    setRetentionDisplay(retentionDays.value);
  }, [retentionDays.value]);

  return (
    <section aria-labelledby="general-heading" className="flex flex-col">
      <header className="mb-5">
        <h1
          id="general-heading"
          className="text-xl font-semibold tracking-tight"
        >
          General
        </h1>
      </header>

      {/*
        V9 layout: each section is a bold proper-case heading above its
        rows, separated by vertical margin instead of the old inset-
        legend cards. Sections must stay direct siblings of this
        container so `last:mb-0` drops the trailing gap on History.
      */}
      <div>
        <PermissionsCard />

        <SettingsSection title="Startup" titleId="startup-group">
          <div className="flex items-center justify-between gap-3 py-2.5">
            <Label htmlFor="launch-at-login" className="font-normal">
              Launch at login
            </Label>
            <Switch
              id="launch-at-login"
              checked={launchAtLogin.value}
              onCheckedChange={(next) => void launchAtLogin.set(next)}
              disabled={launchAtLogin.loading}
            />
          </div>
        </SettingsSection>

        <SettingsSection title="History" titleId="history-group">
          <div className="flex items-center gap-3 py-2.5">
            <Label
              htmlFor="retention-days"
              className="shrink-0 text-sm font-normal"
            >
              Retain for
            </Label>
            <Slider
              id="retention-days"
              aria-label="History retention period in days"
              min={1}
              max={365}
              step={1}
              value={[retentionDisplay]}
              onValueChange={(values) => {
                const [next] = values;
                if (typeof next === "number") setRetentionDisplay(next);
              }}
              onValueCommit={(values) => {
                const [next] = values;
                if (typeof next === "number") void retentionDays.set(next);
              }}
              disabled={retentionDays.loading}
              className="flex-1"
            />
            <span className="shrink-0 text-right text-sm tabular-nums text-muted-foreground min-w-[4.5rem]">
              {retentionDisplay} day{retentionDisplay === 1 ? "" : "s"}
            </span>
          </div>
        </SettingsSection>

        <SettingsSection title="Privacy & diagnostics" titleId="privacy-group">
          <div className="flex min-h-[46px] items-center justify-between gap-3 py-2.5">
            <Label htmlFor="telemetry-crash-reporting" className="font-normal">
              Send crash reports
            </Label>
            <Switch
              id="telemetry-crash-reporting"
              checked={crashReporting.value}
              onCheckedChange={(next) => void crashReporting.set(next)}
              disabled={crashReporting.loading}
            />
          </div>
          <div className="flex min-h-[46px] items-center justify-between gap-3 py-2.5">
            <Label htmlFor="telemetry-analytics" className="font-normal">
              Send usage analytics
            </Label>
            <Switch
              id="telemetry-analytics"
              checked={analytics.value}
              onCheckedChange={(next) => void analytics.set(next)}
              disabled={analytics.loading}
            />
          </div>
          <p className="text-muted-foreground text-xs leading-relaxed pt-1">
            Anonymous performance &amp; crash data. Never your transcripts.
          </p>
        </SettingsSection>

        <FeedbackCard />
      </div>
    </section>
  );
}

/**
 * Plan 040 — Muni is open source, so feature requests and bug reports live on
 * the public GitHub issue tracker. No backend command is involved: the
 * shell-opener plugin hands the URL to the OS default browser directly.
 */
export const GITHUB_ISSUES_URL = "https://github.com/mrkvn/muni/issues";

function FeedbackCard() {
  async function handleOpenIssues() {
    try {
      await openUrl(GITHUB_ISSUES_URL);
    } catch (e) {
      // The opener only fails when the OS has no handler for the URL, which
      // the user can always work around by visiting the link themselves.
      toast.error("Couldn't open GitHub. Visit github.com/mrkvn/muni/issues.");
      console.error("open github issues failed", e);
    }
  }

  return (
    <SettingsSection title="Feedback" titleId="feedback-group">
      <div className="flex flex-wrap items-center justify-between gap-3 py-2.5">
        <p className="text-sm text-muted-foreground">
          Report a bug or request a feature on GitHub.
        </p>
        <Button
          variant="outline"
          size="sm"
          onClick={() => void handleOpenIssues()}
        >
          Open GitHub Issues
          <ExternalLink className="ml-1 h-3.5 w-3.5" aria-hidden="true" />
        </Button>
      </div>
    </SettingsSection>
  );
}

/**
 * Phase 11 — live macOS permission status with a per-row "Open System
 * Settings" deep link. Polls only while the user is on this route so we
 * don't burn IPC cycles when Settings → General is hidden but mounted
 * (per the Phase 10 always-mounted SettingsLayout).
 *
 * Why polling at all: macOS doesn't fire callbacks when the user
 * toggles a TCC switch, so we re-read the status every 500 ms — same
 * cadence the onboarding wizard uses. The interval cleans up via the
 * `useEffect` return whenever the route changes or the component
 * unmounts, so it can't leak across tab switches.
 */
function PermissionsCard() {
  const { pathname } = useLocation();
  const onThisRoute = pathname === "/settings/general";

  const mic = useMicrophonePermission();
  const a11y = useAccessibilityPermission();
  const inputMon = useInputMonitoringPermission();
  const micSilenced = useMicSilenced(onThisRoute);
  // Once the user clicks Open System Settings on the Mic row, the
  // pill flips Denied → Pending. AVFoundation's per-process cache
  // can't refresh until restart, so we trust the user's intent and
  // surface "Pending" until the next process restart resolves it.
  const [micOpenedSettings, setMicOpenedSettings] = useState(false);

  const { startPolling: startMicPolling } = mic;
  useEffect(() => {
    if (!onThisRoute) return;
    return startMicPolling();
  }, [onThisRoute, startMicPolling]);

  const { startPolling: startA11yPolling } = a11y;
  useEffect(() => {
    if (!onThisRoute) return;
    return startA11yPolling();
  }, [onThisRoute, startA11yPolling]);

  const { startPolling: startInputMonPolling } = inputMon;
  useEffect(() => {
    if (!onThisRoute) return;
    return startInputMonPolling();
  }, [onThisRoute, startInputMonPolling]);

  const micPill = computeMicPill(mic.status, micSilenced, micOpenedSettings);
  const a11yPill: PillState = a11y.trusted
    ? { kind: "ok", label: "Granted" }
    : { kind: "error", label: "Not granted" };
  const inputMonPill: PillState =
    inputMon.status === "granted"
      ? { kind: "ok", label: "Granted" }
      : {
          kind: "error",
          label: inputMon.status === "denied" ? "Denied" : "Not granted",
        };

  return (
    <SettingsSection title="Permissions" titleId="permissions-group">
      <PermissionRow
        label="Microphone"
        pill={micPill}
        onOpenSettings={() => {
          setMicOpenedSettings(true);
          void openSystemSettings("microphone");
        }}
      />
      <PermissionRow
        label="Accessibility"
        pill={a11yPill}
        onOpenSettings={() => void openSystemSettings("accessibility")}
      />
      <PermissionRow
        label="Input Monitoring"
        pill={inputMonPill}
        onOpenSettings={() => void openSystemSettings("input-monitoring")}
      />
    </SettingsSection>
  );
}

type PillKind = "ok" | "warn" | "error";
type PillState = { kind: PillKind; label: string };

// Mic pill resolution lives in its own pure function so the rules are
// readable in one place. Order matters: silenced (cache lying about
// authorized) wins over a granted reading, and an opened-settings
// intent flips a denied pill to pending until restart.
function computeMicPill(
  status: MicrophoneStatus,
  silenced: boolean,
  openedSettings: boolean,
): PillState {
  if (silenced) return { kind: "warn", label: "Stale" };
  if (status === "authorized") return { kind: "ok", label: "Granted" };
  if (status === "denied" || status === "restricted") {
    return openedSettings
      ? { kind: "warn", label: "Pending" }
      : { kind: "error", label: "Denied" };
  }
  return { kind: "error", label: "Not granted" };
}

type PermissionRowProps = {
  label: string;
  pill: PillState;
  onOpenSettings: () => void;
};

function PermissionRow({ label, pill, onOpenSettings }: PermissionRowProps) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-3 py-2.5">
      <span className="text-sm">{label}</span>
      <div className="flex flex-wrap items-center gap-2">
        <span
          role="status"
          className={`rounded-md border px-2.5 py-1 text-xs ${pillClasses(pill.kind)}`}
        >
          {pill.label}
        </span>
        {pill.kind === "ok" ? null : (
          <Button variant="outline" size="sm" onClick={onOpenSettings}>
            Open System Settings
            <ExternalLink className="ml-1 h-3.5 w-3.5" aria-hidden="true" />
          </Button>
        )}
      </div>
    </div>
  );
}

function pillClasses(kind: PillKind): string {
  switch (kind) {
    case "ok":
      return "border-emerald-500/30 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300";
    case "warn":
      return "border-amber-500/30 bg-amber-500/10 text-amber-700 dark:text-amber-300";
    case "error":
      return "border-destructive/30 bg-destructive/10 text-destructive";
  }
}

// Polls the silence-detection flag at the same cadence as the
// permission probes. Once true it stays true until process restart, so
// a single confirmed flip is the steady state.
function useMicSilenced(active: boolean): boolean {
  const [silenced, setSilenced] = useState(false);

  useEffect(() => {
    if (!active) return;
    let cancelled = false;
    const tick = () => {
      void invoke<boolean>("is_mic_likely_silenced").then((next) => {
        if (!cancelled) setSilenced(next);
      });
    };
    tick();
    const id = window.setInterval(tick, 500);
    return () => {
      cancelled = true;
      window.clearInterval(id);
    };
  }, [active]);

  return silenced;
}

/**
 * Reads the OS launch-at-login state and flips it via the typed IPC
 * command on toggle. Toast-surfaces failures so a silent OS-side
 * rejection doesn't leave the switch lying.
 *
 * Re-reads on every window focus, not just mount. The Main webview
 * pre-renders this pane at app boot — *before* onboarding registers the
 * login item — so the first read is often a stale `false`. Refreshing on
 * focus repairs that the moment the window appears post-onboarding, and
 * also catches the user flipping the item directly in System Settings →
 * Login Items while Muni is open.
 */
function useLaunchAtLogin() {
  const [value, setValue] = useState(false);
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(async () => {
    try {
      setValue(await invoke<boolean>("is_launch_at_login_enabled"));
    } catch (e) {
      toast.error(`Couldn't read launch-at-login: ${String(e)}`);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
    let unlisten: (() => void) | undefined;
    let disposed = false;
    void getCurrentWindow()
      .onFocusChanged(({ payload: focused }) => {
        if (focused) void refresh();
      })
      .then((fn) => {
        if (disposed) fn();
        else unlisten = fn;
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [refresh]);

  const set = useCallback(
    async (next: boolean) => {
      const previous = value;
      setValue(next);
      try {
        await invoke("set_launch_at_login", { enabled: next });
      } catch (e) {
        setValue(previous);
        toast.error(`Couldn't update launch-at-login: ${String(e)}`);
      }
      // Eslint hint for the closure capture; `value` is intentionally read
      // at call time so a rapid double-toggle reverts to the captured
      // pre-click state on failure rather than a stale optimistic value.
      // eslint-disable-next-line react-hooks/exhaustive-deps
    },
    [value],
  );

  return { value, set, loading };
}
