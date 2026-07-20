/**
 * Phase 9 — First-run wizard, mounted as the entire `onboarding`
 * window. Eight linear steps; the macOS-permission steps gate
 * Continue. API Keys and Launch at Login are both skippable — the
 * runtime raises a Loud `Deepgram/GroqMissingApiKey` with a macOS
 * notification on the first hotkey press without a key, and the user
 * can flip Launch at Login any time from Settings → General.
 *
 * The per-service editor on the API Keys step lives in the shared
 * `components/ApiKeyEditor`, so this surface and Settings → API Keys
 * cannot drift visually or behaviourally.
 *
 * Each TCC-gated step (mic / accessibility / input monitoring) starts
 * a 500ms poller while the user is on it, so a grant performed via
 * System Settings flips the UI without an in-app click. Polling stops
 * automatically when the user navigates to another step or closes
 * the window.
 *
 * The Launch-at-Login step defaults ON (a hotkey-driven dictation app
 * is only useful while it's running) but shows the toggle so the choice
 * is the user's, not a silent default. `SMAppService` (server-side)
 * means flipping it fires no Automation prompt, so the step can apply
 * the choice live as the user toggles.
 */
import { getName } from "@tauri-apps/api/app";
import {
  type MutableRefObject,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import {
  Check,
  ChevronLeft,
  ChevronRight,
  ExternalLink,
  Keyboard,
  LogIn,
  Mic,
  Sparkles,
} from "lucide-react";

import { ApiKeyEditor } from "@/components/ApiKeyEditor";
import { MuniLogo } from "@/components/MuniLogo";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Progress } from "@/components/ui/progress";
import { Switch } from "@/components/ui/switch";
import {
  type InputMonitoringStatus,
  type MicrophoneStatus,
  completeOnboarding,
  getStoredLaunchAtLoginPref,
  openSystemSettings,
  setLaunchAtLogin,
  trackOnboardingStarted,
  trackPermissionGranted,
  useAccessibilityPermission,
  useInputMonitoringPermission,
  useMicrophonePermission,
} from "@/hooks/usePermissions";
import { useSecretProbe } from "@/hooks/useSecrets";
import { TelemetryConsentStep } from "./steps/TelemetryConsentStep";

const STEP_IDS = [
  "welcome",
  "telemetry-consent",
  "microphone",
  "accessibility",
  "inputMonitoring",
  "apiKeys",
  "launchAtLogin",
  "hotkeyTest",
] as const;
type StepId = (typeof STEP_IDS)[number];

type StepMeta = { id: StepId; title: string };

const STEPS: StepMeta[] = [
  { id: "welcome", title: "Welcome" },
  { id: "telemetry-consent", title: "Help improve Muni" },
  { id: "microphone", title: "Microphone" },
  { id: "accessibility", title: "Accessibility" },
  { id: "inputMonitoring", title: "Input Monitoring" },
  { id: "apiKeys", title: "API Keys" },
  { id: "launchAtLogin", title: "Launch at Login" },
  { id: "hotkeyTest", title: "Try the hotkey" },
];

export function OnboardingWizard() {
  const [stepIndex, setStepIndex] = useState<number>(0);
  const [finishing, setFinishing] = useState<boolean>(false);
  const [finishError, setFinishError] = useState<string | null>(null);

  const step = STEPS[stepIndex];
  const progress = ((stepIndex + 1) / STEPS.length) * 100;

  const mic = useMicrophonePermission();
  const a11y = useAccessibilityPermission();
  const inputMon = useInputMonitoringPermission();

  // Feature 033 (Phase 3, task 21) — activation funnel. The wizard is the
  // funnel's home, so it owns the emits (the permission hooks are also used in
  // Settings, where a grant isn't a funnel step). All three route through the
  // Rust telemetry queue and are no-ops when analytics is off.

  // Top of the funnel: the wizard mounted. Fire exactly once.
  useEffect(() => {
    trackOnboardingStarted();
  }, []);

  // One `permission_granted` per permission, fired on the first granted
  // transition (whether the grant came from the in-app prompt or a poll that
  // caught a System-Settings toggle). The ref guards against the re-fires that
  // polling would otherwise cause once a permission sits in the granted state.
  const grantedSent = useRef({
    microphone: false,
    accessibility: false,
    input_monitoring: false,
  });
  useEffect(() => {
    if (mic.status === "authorized" && !grantedSent.current.microphone) {
      grantedSent.current.microphone = true;
      trackPermissionGranted("microphone");
    }
  }, [mic.status]);
  useEffect(() => {
    if (a11y.trusted && !grantedSent.current.accessibility) {
      grantedSent.current.accessibility = true;
      trackPermissionGranted("accessibility");
    }
  }, [a11y.trusted]);
  useEffect(() => {
    if (inputMon.status === "granted" && !grantedSent.current.input_monitoring) {
      grantedSent.current.input_monitoring = true;
      trackPermissionGranted("input_monitoring");
    }
  }, [inputMon.status]);

  // The final step adapts its title + body based on whether the user
  // skipped the API Keys step. Lifted to the parent so both surfaces
  // (the H1 header and the body content) stay in lockstep. Deepgram +
  // Groq are the two keys the wizard's ApiKeysStep covers, so those
  // are the bar.
  const deepgramProbe = useSecretProbe("deepgram");
  const groqProbe = useSecretProbe("groq");
  const keysReady = deepgramProbe.isSet && groqProbe.isSet;

  // When the user reaches the final step without API keys, the step's
  // canonical "Try the hotkey" title would lie — pressing the hotkey
  // would just raise a Loud MissingApiKey notification. Surface the
  // real next action in the H1 instead, but phrase it as a reminder
  // ("after Finish") rather than an imperative ("do this now"), since
  // the active CTA is still the Finish button below.
  const stepTitle =
    step.id === "hotkeyTest" && !keysReady
      ? "Don't forget your API keys"
      : step.title;

  // Polling: while the user is on a TCC-gated step, run a 500ms
  // interval that re-reads the live OS status. We deliberately keep
  // polling running even AFTER the permission flips to granted —
  // earlier we'd stop on grant, but that left the wizard showing a
  // stale "granted" pill if the user later toggled the permission
  // off in System Settings while still on the same step. Polling is
  // one IPC call per 500ms; the cost is negligible. Cleanup happens
  // when the user navigates to a different step or closes the
  // window, so the interval can never leak.
  const { startPolling: startMicPolling } = mic;
  useEffect(() => {
    if (step.id !== "microphone") return;
    return startMicPolling();
  }, [step.id, startMicPolling]);

  const { startPolling: startA11yPolling } = a11y;
  useEffect(() => {
    if (step.id !== "accessibility") return;
    return startA11yPolling();
  }, [step.id, startA11yPolling]);

  const { startPolling: startInputMonPolling } = inputMon;
  useEffect(() => {
    if (step.id !== "inputMonitoring") return;
    return startInputMonPolling();
  }, [step.id, startInputMonPolling]);

  // Plan 039 task 39(c) — Launch-at-Login state, lifted here (mirrors
  // `grantedSent` above) because `LaunchAtLoginStep` itself gets
  // unmounted/remounted every time `stepIndex` changes (the render below is
  // a ternary, not a persistent tree). Keeping `enabled` and the "have we
  // locked in the default yet" flag on the parent means a Back → Forward
  // revisit reflects the user's last choice instead of re-running the
  // default-on lock-in and silently re-enabling an explicit opt-out.
  const [launchAtLoginEnabled, setLaunchAtLoginEnabled] = useState(true);
  const launchAtLoginLockedIn = useRef(false);

  const canAdvance = useMemo<boolean>(() => {
    switch (step.id) {
      case "welcome":
        return true;
      case "telemetry-consent":
        // Never gates Continue: both toggles default ON and persist live,
        // so "Continue without touching them" lands the informed default.
        return true;
      case "microphone":
        return mic.status === "authorized";
      case "accessibility":
        return a11y.trusted;
      case "inputMonitoring":
        return inputMon.status === "granted";
      case "apiKeys":
        // Optional: users can finish onboarding without keys. The runtime
        // raises `Deepgram/GroqMissingApiKey` (Loud) on the first hotkey
        // press without a key, which surfaces a macOS notification and
        // deep-links to Settings → API Keys.
        return true;
      case "launchAtLogin":
        // Never gates Continue: the step defaults ON and applies live,
        // so "Continue without touching it" lands the informed default.
        // Either choice is valid and reversible from Settings → General.
        return true;
      case "hotkeyTest":
        return true;
    }
  }, [step.id, mic.status, a11y.trusted, inputMon.status]);

  const isLastStep = stepIndex === STEPS.length - 1;

  const handleBack = useCallback(() => {
    setStepIndex((current) => Math.max(0, current - 1));
  }, []);

  const handleNext = useCallback(() => {
    setStepIndex((current) => Math.min(STEPS.length - 1, current + 1));
  }, []);

  const handleFinish = useCallback(async () => {
    setFinishing(true);
    setFinishError(null);
    try {
      await completeOnboarding();
    } catch (e) {
      setFinishError(String(e));
      setFinishing(false);
    }
  }, []);

  return (
    <main
      role="main"
      aria-labelledby="onboarding-title"
      className="flex h-screen w-screen flex-col bg-background text-foreground"
    >
      <header className="flex flex-col gap-2 border-b border-border px-8 pb-4 pt-6">
        <p className="text-muted-foreground text-xs uppercase tracking-wide">
          Step {stepIndex + 1} of {STEPS.length}
        </p>
        <h1
          id="onboarding-title"
          className="text-xl font-semibold tracking-tight"
        >
          {stepTitle}
        </h1>
        <Progress
          value={progress}
          aria-label={`Onboarding progress: step ${stepIndex + 1} of ${STEPS.length}`}
        />
      </header>

      <section aria-live="polite" className="flex-1 overflow-y-auto px-8 py-6">
        {step.id === "welcome" ? <WelcomeStep /> : null}
        {step.id === "telemetry-consent" ? <TelemetryConsentStep /> : null}
        {step.id === "microphone" ? (
          <MicrophoneStep
            status={mic.status}
            onRequest={() => void mic.request()}
            onOpenSettings={() => void openSystemSettings("microphone")}
          />
        ) : null}
        {step.id === "accessibility" ? (
          <AccessibilityStep
            trusted={a11y.trusted}
            onOpenSettings={() => void openSystemSettings("accessibility")}
          />
        ) : null}
        {step.id === "inputMonitoring" ? (
          <InputMonitoringStep
            status={inputMon.status}
            onRequest={() => void inputMon.request()}
            onOpenSettings={() => void openSystemSettings("input-monitoring")}
          />
        ) : null}
        {step.id === "apiKeys" ? <ApiKeysStep /> : null}
        {step.id === "launchAtLogin" ? (
          <LaunchAtLoginStep
            enabled={launchAtLoginEnabled}
            onEnabledChange={setLaunchAtLoginEnabled}
            lockedInRef={launchAtLoginLockedIn}
          />
        ) : null}
        {step.id === "hotkeyTest" ? (
          <HotkeyTestStep keysReady={keysReady} />
        ) : null}
      </section>

      <footer className="flex items-center justify-between gap-3 border-t border-border px-8 py-4">
        <Button
          variant="ghost"
          onClick={handleBack}
          disabled={stepIndex === 0 || finishing}
        >
          <ChevronLeft aria-hidden="true" />
          Back
        </Button>
        <div className="flex flex-col items-end gap-1">
          {finishError ? (
            <span role="alert" className="text-destructive text-xs">
              Couldn't finish onboarding: {finishError}
            </span>
          ) : null}
          {isLastStep ? (
            <Button
              onClick={() => void handleFinish()}
              disabled={!canAdvance || finishing}
            >
              {finishing ? "Finishing…" : "Finish"}
              <Check aria-hidden="true" />
            </Button>
          ) : (
            <Button onClick={handleNext} disabled={!canAdvance}>
              Continue
              <ChevronRight aria-hidden="true" />
            </Button>
          )}
        </div>
      </footer>
    </main>
  );
}

function WelcomeStep() {
  return (
    <p className="text-base leading-relaxed">
      Hold the hotkey and speak, or quick-tap to toggle. Muni transcribes,
      cleans up, and pastes.
    </p>
  );
}

type MicrophoneStepProps = {
  status: MicrophoneStatus;
  onRequest: () => void;
  onOpenSettings: () => void;
};

function MicrophoneStep({
  status,
  onRequest,
  onOpenSettings,
}: MicrophoneStepProps) {
  const denied = status === "denied" || status === "restricted";
  const authorized = status === "authorized";

  const pillKind = authorized ? "ok" : denied ? "error" : "pending";
  const pillLabel = authorized
    ? "Granted"
    : denied
      ? "Denied — re-grant in System Settings."
      : "Not granted yet.";

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center gap-3">
        <span className="flex size-9 items-center justify-center rounded-md bg-muted">
          <Mic className="size-4" aria-hidden="true" />
        </span>
        <p className="text-base font-medium">Allow microphone access</p>
      </div>
      <StatusPill kind={pillKind} label={pillLabel} />
      <div className="flex flex-wrap items-center gap-2">
        {status === "notDetermined" ? (
          <Button onClick={onRequest}>Grant microphone access</Button>
        ) : null}
        {denied ? (
          // No in-app "re-check" button: AVFoundation caches the
          // denied decision at the process level, so neither
          // `requestAccess` nor `authorizationStatus` can flip the
          // status without a process restart. The honest path is
          // System Settings + relaunch.
          <Button variant="outline" onClick={onOpenSettings}>
            Open System Settings
            <ExternalLink aria-hidden="true" />
          </Button>
        ) : null}
      </div>
    </div>
  );
}

type AccessibilityStepProps = {
  trusted: boolean;
  onOpenSettings: () => void;
};

function AccessibilityStep({
  trusted,
  onOpenSettings,
}: AccessibilityStepProps) {
  // We intentionally skip the macOS Accessibility prompt
  // (`AXIsProcessTrustedWithOptions(prompt:true)`): on macOS its only
  // useful affordance is an "Open System Settings" deep-link, so showing
  // it would force the user through one extra click for no information
  // gain. Drop straight to System Settings instead — same destination,
  // one fewer dismiss.
  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center gap-3">
        <span className="flex size-9 items-center justify-center rounded-md bg-muted">
          <Sparkles className="size-4" aria-hidden="true" />
        </span>
        <p className="text-base font-medium">Allow Accessibility</p>
      </div>
      <StatusPill
        kind={trusted ? "ok" : "pending"}
        label={trusted ? "Granted" : "Waiting for grant…"}
      />
      {!trusted ? <FindAppHint /> : null}
      <div className="flex flex-wrap items-center gap-2">
        {!trusted ? (
          <Button onClick={onOpenSettings}>
            Open System Settings
            <ExternalLink aria-hidden="true" />
          </Button>
        ) : null}
      </div>
    </div>
  );
}

/**
 * Visual hint that tells the user what to look for in System Settings →
 * Privacy & Security. macOS lists apps by their `.app` filename + icon;
 * without this, a user staring at a long list of installed apps has to
 * guess which entry is ours. The mini-icon mirrors the look of the
 * sidebar entry (rounded dark square + Muni wordmark) so the brain
 * matches it instantly.
 *
 * The name is fetched at mount via `getName()` so the hint reads
 * "Muni.app" in the prod build and "Muni-dev.app" in the dev shell,
 * matching the on-disk bundle name in either case.
 */
function FindAppHint() {
  const [name, setName] = useState("Muni");
  useEffect(() => {
    void getName().then(setName);
  }, []);
  return (
    <div className="flex items-center gap-2 rounded-md border border-border bg-muted/40 px-3 py-2 text-muted-foreground text-xs">
      <span>Find</span>
      <span
        className="flex size-5 items-center justify-center rounded-[5px] bg-gradient-to-b from-slate-600 to-slate-900"
        aria-hidden="true"
      >
        <MuniLogo className="size-3 text-white" />
      </span>
      <span className="text-foreground font-medium">{name}.app</span>
      <span>in the list.</span>
    </div>
  );
}

type InputMonitoringStepProps = {
  status: InputMonitoringStatus;
  onRequest: () => void;
  onOpenSettings: () => void;
};

function InputMonitoringStep({
  status,
  onRequest,
  onOpenSettings,
}: InputMonitoringStepProps) {
  const granted = status === "granted";
  const denied = status === "denied";

  const pillKind = granted ? "ok" : denied ? "error" : "pending";
  const pillLabel = granted
    ? "Granted"
    : denied
      ? "Denied — toggle Muni on in System Settings."
      : "Waiting for grant…";

  // Mirror the Microphone step exactly: when the OS hasn't seen this
  // process before, `IOHIDRequestAccess` is enough on its own — it
  // either lands the grant directly or surfaces a system prompt that
  // the user can act on. Only fall back to the System-Settings deep
  // link if the cached decision is `denied`, because TCC won't re-prompt
  // a denied process.
  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center gap-3">
        <span className="flex size-9 items-center justify-center rounded-md bg-muted">
          <Keyboard className="size-4" aria-hidden="true" />
        </span>
        <p className="text-base font-medium">Allow Input Monitoring</p>
      </div>
      <StatusPill kind={pillKind} label={pillLabel} />
      <div className="flex flex-wrap items-center gap-2">
        {!granted && !denied ? (
          <Button onClick={onRequest}>Grant Input Monitoring access</Button>
        ) : null}
        {denied ? (
          <Button variant="outline" onClick={onOpenSettings}>
            Open System Settings
            <ExternalLink aria-hidden="true" />
          </Button>
        ) : null}
      </div>
    </div>
  );
}

function ApiKeysStep() {
  return (
    <div className="flex flex-col gap-4">
      <ApiKeyEditor
        service="deepgram"
        label="Deepgram"
        helpUrl="https://console.deepgram.com/"
      />
      <ApiKeyEditor
        service="groq"
        label="Groq"
        helpUrl="https://console.groq.com/keys"
      />
    </div>
  );
}

function LaunchAtLoginStep({
  enabled,
  onEnabledChange,
  lockedInRef,
}: {
  enabled: boolean;
  onEnabledChange: (next: boolean) => void;
  lockedInRef: MutableRefObject<boolean>;
}) {
  // Default ON — opt-out, but shown. A hotkey-driven dictation app is
  // only useful while it's running, so launch-at-login is load-bearing,
  // not a nicety. We default it on and let the user opt out *here* rather
  // than bury it. `SMAppService` fires no prompt, so we apply the choice
  // live as the toggle flips.
  const [error, setError] = useState<string | null>(null);

  const persist = useCallback(async (next: boolean) => {
    try {
      await setLaunchAtLogin(next);
      setError(null);
    } catch (err) {
      // Dev builds refuse to *enable* (unstable, un-code-signed bundle
      // path); that's expected and not worth alarming the user mid-wizard.
      // The default-on choice still applies once they install Muni.app.
      const message = String(err);
      if (next && /development builds/i.test(message)) return;
      setError(message);
    }
  }, []);

  // Lock in the default-on choice exactly once per onboarding session, the
  // first time this step is reached, so "Continue without touching this"
  // still registers the login item. `lockedInRef` lives on the parent (see
  // `OnboardingWizard`), so a later revisit — Back, then Continue again —
  // finds it already tripped and just reflects `enabled` (whatever the user
  // last chose) instead of re-persisting `true` over an explicit opt-out.
  //
  // That in-memory ref only survives within one mount of the wizard, though
  // — it can't tell a fresh onboarding session from a *relaunch* of one that
  // never reached `complete_onboarding` (quit mid-wizard, or the wizard
  // re-runs for any other reason). Plan 039 task 39(c): before locking in
  // the default, check the persisted store directly (bypassing the
  // default-merged `settings_get`, which can't distinguish "never set" from
  // an explicit `false`). A stored value — any value — means a prior pass
  // already made this choice; adopt it as-is and skip the lock-in entirely,
  // so an explicit opt-out can never be silently overwritten by a
  // subsequent mount's default-on persist.
  useEffect(() => {
    if (lockedInRef.current) return;
    lockedInRef.current = true;
    let cancelled = false;
    void (async () => {
      const stored = await getStoredLaunchAtLoginPref().catch(() => null);
      if (cancelled) return;
      if (stored !== null) {
        onEnabledChange(stored);
        return;
      }
      onEnabledChange(true);
      void persist(true);
    })();
    return () => {
      cancelled = true;
    };
  }, [persist, lockedInRef, onEnabledChange]);

  const onToggle = useCallback(
    (next: boolean) => {
      onEnabledChange(next);
      void persist(next);
    },
    [onEnabledChange, persist],
  );

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between gap-4">
        <div className="flex items-center gap-3">
          <span className="flex size-9 items-center justify-center rounded-md bg-muted">
            <LogIn className="size-4" aria-hidden="true" />
          </span>
          <div className="flex flex-col">
            <Label
              htmlFor="onboarding-launch-at-login"
              className="text-base font-medium"
            >
              Launch Muni at login
            </Label>
            <p className="text-muted-foreground text-sm">
              Keeps your dictation hotkey ready the moment you log in.
            </p>
          </div>
        </div>
        <Switch
          id="onboarding-launch-at-login"
          checked={enabled}
          onCheckedChange={onToggle}
          aria-label="Launch Muni at login"
        />
      </div>
      {error ? (
        <StatusPill kind="error" label={`Couldn't update: ${error}`} />
      ) : null}
    </div>
  );
}

function HotkeyTestStep({ keysReady }: { keysReady: boolean }) {
  // When `keysReady` is false the parent already swapped the H1 to
  // "Add your API keys" — the body just needs the where-to-go pointer,
  // no redundant "Almost set" header. Deepgram + Groq are the two keys
  // the wizard's ApiKeysStep covers, so those are the bar.
  if (!keysReady) {
    return (
      <p className="text-muted-foreground text-sm leading-relaxed">
        Add them in Settings → API Keys to start using Muni.
      </p>
    );
  }

  const keyCombo = (
    <>
      <kbd className="rounded bg-muted px-1.5 py-0.5 text-xs">Control</kbd>
      {" + "}
      <kbd className="rounded bg-muted px-1.5 py-0.5 text-xs">Option</kbd>
    </>
  );
  return (
    <div className="flex flex-col gap-3">
      <p className="text-base leading-relaxed">You're set.</p>
      <p className="text-muted-foreground text-sm leading-relaxed">
        Hold {keyCombo}, speak, release — or quick-tap to start, tap again to
        stop. Muni lives in the menu bar.
      </p>
    </div>
  );
}

type StatusPillProps = {
  kind: "ok" | "error" | "pending";
  label: string;
  inline?: boolean;
};

function StatusPill({ kind, label, inline = false }: StatusPillProps) {
  const palette =
    kind === "ok"
      ? "border-emerald-500/30 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
      : kind === "error"
        ? "border-destructive/30 bg-destructive/10 text-destructive"
        : "border-border bg-muted/40 text-muted-foreground";
  return (
    <p
      role={kind === "error" ? "alert" : "status"}
      className={`${inline ? "ml-auto " : ""}rounded-md border px-3 py-1.5 text-xs ${palette}`}
    >
      {label}
    </p>
  );
}
