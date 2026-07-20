// @vitest-environment jsdom
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { OnboardingWizard } from "./OnboardingWizard";

// In-memory IPC stub. Each test seeds whichever responses it needs.
type Handler = (args?: Record<string, unknown>) => unknown | Promise<unknown>;
const handlers = new Map<string, Handler>();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async (name: string, args?: Record<string, unknown>) => {
    const handler = handlers.get(name);
    if (!handler) {
      throw new Error(`unmocked invoke("${name}")`);
    }
    return handler(args);
  },
}));

// Event bridge stub. `useSecretProbe` (secrets://changed) subscribes here.
// We capture each listener by event name so a test can fire it directly.
// Keyed by name; the only repeated subscriber is `useSecretProbe` (deepgram
// + groq), which no test fires, so last-writer-wins is harmless.
const { eventListeners } = vi.hoisted(() => ({
  eventListeners: new Map<string, (event?: unknown) => void>(),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: async (name: string, cb: (event?: unknown) => void) => {
    eventListeners.set(name, cb);
    return () => eventListeners.delete(name);
  },
}));

// `FindAppHint` reads productName via `getName` so the System Settings
// hint reads "Muni.app" in prod and "Muni-dev.app" in dev. The Tauri
// app module hits its own internal `invoke` binding that the core mock
// above can't intercept, so stub it directly.
vi.mock("@tauri-apps/api/app", () => ({
  getName: async () => "Muni",
}));

beforeEach(() => {
  handlers.clear();
  eventListeners.clear();
});

afterEach(() => {
  cleanup();
  handlers.clear();
  eventListeners.clear();
});

function seedDefaults(overrides?: Partial<Record<string, Handler>>) {
  const defaults: Record<string, Handler> = {
    microphone_status: () => "notDetermined",
    request_microphone_access: () => true,
    is_accessibility_trusted: () => false,
    prompt_accessibility: () => false,
    input_monitoring_status: () => "notDetermined",
    request_input_monitoring: () => "notDetermined",
    secrets_is_deepgram_set: () => false,
    secrets_is_groq_set: () => false,
    secrets_set_deepgram: () => undefined,
    secrets_set_groq: () => undefined,
    validate_api_key: () => true,
    open_system_settings: () => undefined,
    complete_onboarding: () => undefined,
    set_launch_at_login: () => undefined,
    // Plan 039 task 39(c) — a fresh store (the common case in these
    // tests) has never had this key written, so the raw-read command
    // returns null and the default-on lock-in proceeds as before.
    get_stored_launch_at_login_pref: () => null,
    // Feature 033 (Phase 3) — the wizard fires activation-funnel events
    // (`onboarding_started` on mount, `permission_granted` per grant) through
    // this fire-and-forget command. Stub it so the unmocked-invoke guard
    // doesn't trip; the wizard swallows any rejection regardless.
    telemetry_track: () => undefined,
  };
  for (const [name, handler] of Object.entries(defaults)) {
    handlers.set(name, handler);
  }
  if (overrides) {
    for (const [name, handler] of Object.entries(overrides)) {
      if (handler) handlers.set(name, handler);
    }
  }
}

async function flushAsync() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

function findContinue() {
  return screen.getByRole("button", { name: /^continue$/i });
}

// Eight steps: Welcome → Telemetry consent → Microphone → Accessibility →
// Input Monitoring → API Keys → Launch at Login → Hotkey test. The
// telemetry-consent step (Feature 033) never gates Continue, so walking past
// it is one extra Continue click with no interaction.
const TOTAL_STEPS = 8;

describe("OnboardingWizard", () => {
  it("starts on Welcome and lets the user advance immediately", async () => {
    seedDefaults();
    render(<OnboardingWizard />);
    await flushAsync();

    expect(screen.queryByText(`Step 1 of ${TOTAL_STEPS}`)).not.toBeNull();
    const continueButton = findContinue();
    expect((continueButton as HTMLButtonElement).disabled).toBe(false);

    // Welcome → Telemetry consent (step 2, never gates Continue).
    await userEvent.click(continueButton);
    expect(screen.queryByText(`Step 2 of ${TOTAL_STEPS}`)).not.toBeNull();
  });

  it("blocks Continue on the Microphone step until access is granted", async () => {
    let micStatus: "notDetermined" | "authorized" = "notDetermined";
    seedDefaults({
      microphone_status: () => micStatus,
      request_microphone_access: async () => {
        micStatus = "authorized";
        return true;
      },
    });
    render(<OnboardingWizard />);
    await flushAsync();

    // Welcome → Telemetry consent → Microphone
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    expect(screen.queryByText(`Step 3 of ${TOTAL_STEPS}`)).not.toBeNull();

    expect((findContinue() as HTMLButtonElement).disabled).toBe(true);

    await userEvent.click(
      screen.getByRole("button", { name: /grant microphone access/i }),
    );
    await flushAsync();
    await waitFor(() =>
      expect((findContinue() as HTMLButtonElement).disabled).toBe(false),
    );
  });

  it("blocks Continue on Input Monitoring until granted", async () => {
    seedDefaults({
      microphone_status: () => "authorized",
      is_accessibility_trusted: () => true,
      input_monitoring_status: () => "notDetermined",
    });
    render(<OnboardingWizard />);
    await flushAsync();
    // Welcome → Telemetry consent → Mic → Accessibility → Input Monitoring
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());

    expect(screen.queryByText(`Step 5 of ${TOTAL_STEPS}`)).not.toBeNull();
    expect(screen.queryByText(/allow input monitoring/i)).not.toBeNull();
    expect((findContinue() as HTMLButtonElement).disabled).toBe(true);
  });

  it("allows Continue on the API Keys step even when neither key is saved", async () => {
    // API keys are optional during onboarding — the runtime raises a
    // Loud `Deepgram/GroqMissingApiKey` (macOS notification + deep-link
    // to Settings → API Keys) on the first hotkey press without a key,
    // so blocking the user here would be redundant.
    seedDefaults({
      microphone_status: () => "authorized",
      is_accessibility_trusted: () => true,
      input_monitoring_status: () => "granted",
      secrets_is_deepgram_set: () => false,
      secrets_is_groq_set: () => false,
    });

    render(<OnboardingWizard />);
    await flushAsync();
    // Welcome → Telemetry consent → Mic → Accessibility → IM → API Keys
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());

    expect(screen.queryByText(`Step 6 of ${TOTAL_STEPS}`)).not.toBeNull();
    expect((findContinue() as HTMLButtonElement).disabled).toBe(false);
  });

  it("renders Finish on the last step and invokes complete_onboarding", async () => {
    const completeMock = vi.fn();
    seedDefaults({
      microphone_status: () => "authorized",
      is_accessibility_trusted: () => true,
      input_monitoring_status: () => "granted",
      secrets_is_deepgram_set: () => true,
      secrets_is_groq_set: () => true,
      complete_onboarding: completeMock,
    });

    render(<OnboardingWizard />);
    await flushAsync();
    // Welcome → Telemetry consent → Mic → Accessibility → IM →
    // API Keys → Launch at Login → Hotkey Test
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());

    expect(
      screen.queryByText(`Step ${TOTAL_STEPS} of ${TOTAL_STEPS}`),
    ).not.toBeNull();
    const finishButton = screen.getByRole("button", { name: /^finish$/i });
    expect((finishButton as HTMLButtonElement).disabled).toBe(false);
    await userEvent.click(finishButton);
    await waitFor(() => expect(completeMock).toHaveBeenCalled());
  });

  it("Launch-at-Login step defaults ON and applies the choice live", async () => {
    const setLaunchMock = vi.fn();
    seedDefaults({
      microphone_status: () => "authorized",
      is_accessibility_trusted: () => true,
      input_monitoring_status: () => "granted",
      set_launch_at_login: setLaunchMock,
    });

    render(<OnboardingWizard />);
    await flushAsync();
    // Welcome → Telemetry consent → Mic → Accessibility → IM →
    // API Keys → Launch at Login
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());

    // Default-on is persisted on mount so "Continue without touching it"
    // still lands the user in Login Items.
    const toggle = await screen.findByRole("switch", {
      name: /launch muni at login/i,
    });
    expect((toggle as HTMLButtonElement).getAttribute("aria-checked")).toBe(
      "true",
    );
    await waitFor(() =>
      expect(setLaunchMock).toHaveBeenCalledWith({ enabled: true }),
    );

    // Opting out applies immediately — no prompt, no silent default.
    setLaunchMock.mockClear();
    await userEvent.click(toggle);
    await waitFor(() =>
      expect(setLaunchMock).toHaveBeenCalledWith({ enabled: false }),
    );
  });

  it("Launch-at-Login step: revisiting after opting out does not re-enable it", async () => {
    // Plan 039 task 39(c) — the default-on lock-in must fire at most once
    // per onboarding session. Navigating Back → Continue past this step a
    // second time must reflect the opt-out, not silently flip it back on.
    const setLaunchMock = vi.fn();
    seedDefaults({
      microphone_status: () => "authorized",
      is_accessibility_trusted: () => true,
      input_monitoring_status: () => "granted",
      set_launch_at_login: setLaunchMock,
    });

    render(<OnboardingWizard />);
    await flushAsync();
    // Welcome → Telemetry consent → Mic → Accessibility → IM →
    // API Keys → Launch at Login
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());

    const toggle = await screen.findByRole("switch", {
      name: /launch muni at login/i,
    });
    await waitFor(() =>
      expect(setLaunchMock).toHaveBeenCalledWith({ enabled: true }),
    );

    setLaunchMock.mockClear();
    await userEvent.click(toggle);
    await waitFor(() =>
      expect(setLaunchMock).toHaveBeenCalledWith({ enabled: false }),
    );

    // Back one step, then Continue again — this remounts LaunchAtLoginStep.
    setLaunchMock.mockClear();
    await userEvent.click(screen.getByRole("button", { name: /^back$/i }));
    await userEvent.click(findContinue());

    const toggleAgain = await screen.findByRole("switch", {
      name: /launch muni at login/i,
    });
    expect(
      (toggleAgain as HTMLButtonElement).getAttribute("aria-checked"),
    ).toBe("false");
    expect(setLaunchMock).not.toHaveBeenCalledWith({ enabled: true });
  });

  it("Launch-at-Login step: a fresh wizard mount adopts a persisted opt-out instead of re-enabling it", async () => {
    // Plan 039 task 39(c) regression — the in-memory lock-in ref only
    // survives one mount of the wizard. If the user opted out, then quit
    // before `complete_onboarding` fired (or the wizard re-runs for any
    // other reason), the *next* wizard mount must read the persisted
    // store before deciding to lock in the default, not just re-run the
    // default-on lock-in against a fresh ref/state. This test simulates
    // that cross-session case directly: the store already holds an
    // explicit `false` when a brand-new `OnboardingWizard` mounts.
    const setLaunchMock = vi.fn();
    seedDefaults({
      microphone_status: () => "authorized",
      is_accessibility_trusted: () => true,
      input_monitoring_status: () => "granted",
      set_launch_at_login: setLaunchMock,
      get_stored_launch_at_login_pref: () => false,
    });

    render(<OnboardingWizard />);
    await flushAsync();
    // Welcome → Telemetry consent → Mic → Accessibility → IM →
    // API Keys → Launch at Login
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());

    const toggle = await screen.findByRole("switch", {
      name: /launch muni at login/i,
    });
    await flushAsync();

    expect((toggle as HTMLButtonElement).getAttribute("aria-checked")).toBe(
      "false",
    );
    // The stored opt-out must be adopted as-is — never re-persisted to
    // true by the default-on lock-in.
    expect(setLaunchMock).not.toHaveBeenCalledWith({ enabled: true });
  });

  it("opens System Settings via the closed-set IPC argument", async () => {
    const openMock = vi.fn();
    seedDefaults({
      microphone_status: () => "denied",
      open_system_settings: openMock,
    });

    render(<OnboardingWizard />);
    await flushAsync();
    // Welcome → Telemetry consent → Microphone
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(
      screen.getByRole("button", { name: /open system settings/i }),
    );
    await flushAsync();
    expect(openMock).toHaveBeenCalledWith({ target: "microphone" });
  });

  it("Accessibility step renders a single Open System Settings button (no in-app prompt)", async () => {
    // Regression guard for the manual-QA finding that
    // `AXIsProcessTrustedWithOptions(prompt:true)` was useless on macOS:
    // its only actionable affordance is an "Open System Settings" link,
    // so wrapping it in our own button just added an extra dismiss for
    // no information gain. We skip the prompt entirely and deep-link
    // straight to the right pane. `prompt_accessibility` MUST NOT be
    // invoked from this step.
    const promptMock = vi.fn();
    const openMock = vi.fn();
    seedDefaults({
      microphone_status: () => "authorized",
      is_accessibility_trusted: () => false,
      prompt_accessibility: promptMock,
      open_system_settings: openMock,
    });

    render(<OnboardingWizard />);
    await flushAsync();
    // Welcome → Telemetry consent → Microphone → Accessibility
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());

    expect(screen.queryByText(`Step 4 of ${TOTAL_STEPS}`)).not.toBeNull();
    expect(
      screen.queryByRole("button", { name: /grant accessibility/i }),
    ).toBeNull();
    expect(
      screen.queryByRole("button", { name: /show macos prompt/i }),
    ).toBeNull();

    await userEvent.click(
      screen.getByRole("button", { name: /open system settings/i }),
    );
    await flushAsync();
    expect(openMock).toHaveBeenCalledWith({ target: "accessibility" });
    expect(promptMock).not.toHaveBeenCalled();
  });

  it("Input Monitoring step shows a single Grant button when not yet determined", async () => {
    // Regression guard mirroring the Accessibility collapse: when TCC has
    // never seen the process, IOHIDRequestAccess on its own is enough —
    // either it lands the grant or it surfaces the system prompt the
    // user can act on. A second standalone "Open System Settings" button
    // is noise in that path. The denied state is covered separately
    // because TCC won't re-prompt a denied process.
    const requestMock = vi.fn().mockReturnValue("notDetermined");
    seedDefaults({
      microphone_status: () => "authorized",
      is_accessibility_trusted: () => true,
      input_monitoring_status: () => "notDetermined",
      request_input_monitoring: requestMock,
    });

    render(<OnboardingWizard />);
    await flushAsync();
    // Welcome → Telemetry consent → Microphone → Accessibility →
    // Input Monitoring
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());

    expect(screen.queryByText(`Step 5 of ${TOTAL_STEPS}`)).not.toBeNull();
    const grantButton = screen.getByRole("button", {
      name: /grant input monitoring access/i,
    });
    expect(
      screen.queryByRole("button", { name: /open system settings/i }),
    ).toBeNull();

    await userEvent.click(grantButton);
    await flushAsync();
    expect(requestMock).toHaveBeenCalled();
  });

  it("Input Monitoring step falls back to Open System Settings when denied", async () => {
    // TCC won't re-prompt a denied process via IOHIDRequestAccess, so
    // the denied state must surface the System-Settings deep-link
    // exclusively (mirrors the Microphone-denied path).
    const openMock = vi.fn();
    seedDefaults({
      microphone_status: () => "authorized",
      is_accessibility_trusted: () => true,
      input_monitoring_status: () => "denied",
      open_system_settings: openMock,
    });

    render(<OnboardingWizard />);
    await flushAsync();
    // Welcome → Telemetry consent → Mic → Accessibility →
    // Input Monitoring
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());

    expect(
      screen.queryByRole("button", { name: /grant input monitoring access/i }),
    ).toBeNull();
    await userEvent.click(
      screen.getByRole("button", { name: /open system settings/i }),
    );
    await flushAsync();
    expect(openMock).toHaveBeenCalledWith({ target: "input-monitoring" });
  });

  it("does not render a Grant button when microphone status is denied", async () => {
    // Regression guard. AVFoundation caches the denied decision at
    // the process level — calling `requestAccess` or polling
    // `authorizationStatus` cannot flip it back to authorized
    // mid-process. Showing any "Grant"/"Re-check" button here would
    // be misleading; the only honest path is System Settings + a
    // process relaunch. This test fails if a future change adds
    // such a button back.
    seedDefaults({ microphone_status: () => "denied" });
    render(<OnboardingWizard />);
    await flushAsync();
    // Welcome → Telemetry consent → Microphone
    await userEvent.click(findContinue());
    await userEvent.click(findContinue());

    expect(
      screen.queryByRole("button", { name: /grant microphone access/i }),
    ).toBeNull();
    expect(
      screen.queryByRole("button", { name: /re-check microphone access/i }),
    ).toBeNull();
    // Open System Settings is still rendered as the recovery path.
    expect(
      screen.queryByRole("button", { name: /open system settings/i }),
    ).not.toBeNull();
  });
});
