/**
 * Feature 033 — frontend (webview) half of the Sentry crash spine.
 *
 * The Rust backend owns native + panic crash capture (`telemetry/mod.rs`); this
 * module captures *uncaught JavaScript errors* in the React webview and routes
 * them to the SAME Sentry project, tagged `surface: "webview"` so they're
 * distinguishable from Rust events (which carry no such tag).
 *
 * ## The privacy line (non-negotiable — mirrors the Rust side)
 *
 * Telemetry carries operational metadata ONLY — never transcript content, audio,
 * or anything derived from what the user said. This module enforces it the same
 * two ways the Rust `before_send` does:
 *   1. `scrubPath` strips the macOS home directory (which contains the OS
 *      username) out of every string field before send, and
 *   2. a per-fingerprint daily throttle drops repeats so a render loop can't
 *      exhaust the free-tier error quota.
 *
 * ## Gating
 *
 * `initFrontendSentry()` is a no-op unless BOTH: a DSN is baked into the build
 * (`import.meta.env.VITE_SENTRY_DSN`; prod only — dev/staging bake none) AND the
 * user's crash-reporting toggle is on (read from the same settings store the
 * Rust side reads). Either missing → Sentry is never initialised.
 */
import * as Sentry from "@sentry/react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import type { ErrorEvent, EventHint } from "@sentry/react";

/** Setting key for the crash-reporting consent toggle. Must match the Rust
 * `settings::KEY_CRASH_REPORTING_ENABLED` and the `useSettings.ts` union. */
const KEY_CRASH_REPORTING = "telemetry.crash_reporting";

/** Mirrors `settings::EVENT_SETTINGS_CHANGED` (Rust) — broadcast on every
 * settings write so a runtime opt-out can drop events without a restart. */
const SETTINGS_CHANGED_EVENT = "settings://changed";

/** Free-tier discipline: cap each distinct fingerprint per UTC day, mirroring
 * the Rust `throttle::MAX_PER_FINGERPRINT_PER_DAY`. In-memory only (a reload
 * resets it) — acceptable for a webview, same rationale as the Rust side. */
const MAX_PER_FINGERPRINT_PER_DAY = 5;

/** Re-entrancy guard: `initFrontendSentry()` is idempotent so a double-mount
 * (React StrictMode renders twice in dev) can't init Sentry twice. */
let initialised = false;

/** Live mirror of the crash-reporting toggle. Set true at init; flipped by the
 * settings-changed listener so a runtime opt-out makes `beforeSend` drop events
 * immediately (the Sentry client stays up — same approach as the Rust gate).
 * Turning it back ON when it was off at launch still needs a restart: Sentry was
 * never initialised, so there are no handlers to capture. */
let crashReportingLive = false;

// --- PII scrubbing ----------------------------------------------------------

/**
 * Replace any macOS home path (`/Users/<name>/…`) with `/Users/<redacted>/…`.
 *
 * Structure-based, not prefix-based: it matches `/Users/` followed by ANY single
 * path segment, so a username of any shape (a UUID, a hex string, an email
 * local-part) is redacted just as reliably as a known name. This is the exact
 * counterpart to the Rust `scrub_path` so both surfaces redact identically.
 */
export function scrubPath(input: string): string {
  return input.replace(/\/Users\/[^/]+/g, "/Users/<redacted>");
}

/** Walk every string in an arbitrary JSON-ish value, scrubbing home paths.
 * Bounded by Sentry's own event size; cycles aren't possible in a serialised
 * event payload. */
function scrubValue(value: unknown): unknown {
  if (typeof value === "string") return scrubPath(value);
  if (Array.isArray(value)) return value.map(scrubValue);
  if (value && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
      out[k] = scrubValue(v);
    }
    return out;
  }
  return value;
}

// --- Throttle ---------------------------------------------------------------

interface ThrottleState {
  day: number;
  counts: Map<string, number>;
}

const throttle: ThrottleState = { day: currentUtcDay(), counts: new Map() };

function currentUtcDay(): number {
  return Math.floor(Date.now() / 86_400_000);
}

/** Derive a stable key for an event: top exception `type:value`, else message,
 * else a constant. Mirrors the Rust `fingerprint_key` intent. */
function fingerprintKey(event: ErrorEvent): string {
  const exc = event.exception?.values?.[0];
  if (exc) return `${exc.type ?? "Error"}:${exc.value ?? ""}`;
  if (typeof event.message === "string") return event.message;
  return "<no-fingerprint>";
}

/** Returns `true` if the event is under the per-fingerprint daily cap (and
 * records it). Resets all counts when the UTC day rolls. */
function allow(key: string): boolean {
  const today = currentUtcDay();
  if (today !== throttle.day) {
    throttle.day = today;
    throttle.counts.clear();
  }
  const seen = throttle.counts.get(key) ?? 0;
  if (seen >= MAX_PER_FINGERPRINT_PER_DAY) return false;
  throttle.counts.set(key, seen + 1);
  return true;
}

// --- before_send ------------------------------------------------------------

/**
 * Mirror of the Rust `before_send`: PII scrub → per-fingerprint daily throttle.
 * Returns `null` to drop the event. Non-fatal sampling is intentionally NOT
 * applied here — uncaught webview errors are rare and high-signal (unlike the
 * chatty handled-error path the Rust 10% sample guards against), so we keep them
 * all, bounded only by the throttle.
 */
export function beforeSend(
  event: ErrorEvent,
  _hint?: EventHint,
): ErrorEvent | null {
  // Honour a runtime opt-out immediately.
  if (!crashReportingLive) return null;
  const scrubbed = scrubValue(event) as ErrorEvent;
  return allow(fingerprintKey(scrubbed)) ? scrubbed : null;
}

// --- Init -------------------------------------------------------------------

/** The crash toggle, read once at init. Defaults to ON (matching the Rust
 * fail-open default) if the settings read fails, so a fresh install still
 * captures — gating still ultimately requires a baked DSN, absent in dev. */
async function crashReportingEnabled(): Promise<boolean> {
  try {
    const value = await invoke<boolean | null>("settings_get", {
      key: KEY_CRASH_REPORTING,
    });
    return value !== false;
  } catch {
    return true;
  }
}

/**
 * Initialise webview crash capture. No-op unless a DSN is baked AND the user's
 * crash-reporting toggle is on. Idempotent. Resolves before any error boundary
 * is meaningful, so callers should `await` it before rendering for the tightest
 * capture window (a synchronous render that throws immediately still lands once
 * Sentry is up, since `init` installs the global handlers synchronously).
 */
export async function initFrontendSentry(): Promise<void> {
  if (initialised) return;

  const dsn = import.meta.env.VITE_SENTRY_DSN;
  if (!dsn) return; // dev/staging bake none → silent.

  if (!(await crashReportingEnabled())) return;

  initialised = true;
  Sentry.init({
    dsn,
    // Without this the JS SDK tags every event `production`, so a dev run with
    // a manual DSN override (the only way Sentry is live in dev) is mislabeled.
    // `import.meta.env.DEV` is true under the Vite dev server, false in a built
    // app — matching the Rust SDK's debug-vs-release `development`/`production`.
    environment: import.meta.env.DEV ? "development" : "production",
    tracesSampleRate: 0,
    // Our own path-scrub is the primary PII defence; this disables Sentry's
    // default PII collection (IP, etc.) as a backstop, matching the Rust side.
    sendDefaultPii: false,
    // No session tracking (errors only — operational analytics go through
    // PostHog). In SDK v10 the old `autoSessionTracking: false` flag is gone;
    // sessions are controlled by integration presence, so drop the default
    // `BrowserSession` integration instead.
    integrations: (defaults) =>
      defaults.filter((i) => i.name !== "BrowserSession"),
    beforeSend,
  });

  // Distinguish webview events from Rust events (which carry no `surface` tag).
  Sentry.setTag("surface", "webview");

  // Armed: reporting was on at init, so send until a runtime opt-out.
  crashReportingLive = true;

  // Honour a runtime opt-out without a restart: when the user flips crash
  // reporting off in Settings (possibly from another window), stop sending at
  // once. The client stays initialised; `beforeSend` simply drops.
  void listen<{ key: string; value: unknown }>(SETTINGS_CHANGED_EVENT, (e) => {
    if (e.payload.key === KEY_CRASH_REPORTING) {
      crashReportingLive = e.payload.value !== false;
    }
  }).catch(() => {
    // No live sync available; the init-time gate still applies.
  });
}

/** The Sentry error boundary. Wrap the React root in it. When Sentry isn't
 * initialised (dev/staging or toggle off) it still renders children and shows
 * the fallback on a render error — it just captures nothing. */
export const TelemetryErrorBoundary = Sentry.ErrorBoundary;

// --- Dev-only test affordance (task 13) -------------------------------------

/**
 * DEV-ONLY: fire the backend `telemetry_fire_test_event` Tauri command, which
 * emits exactly one tagged (`test:true`) Sentry message + one captured error
 * through the ACTIVE Rust DSN. Used post-release against prod to validate
 * capture + PII scrub + symbolication. Hidden from production builds via the
 * `import.meta.env.DEV` gate so users can't trigger it.
 */
export async function fireTelemetryTestEvent(): Promise<void> {
  if (!import.meta.env.DEV) return;
  await invoke("telemetry_fire_test_event");
}

// --- Test-only state access (plan 039 task 63) ------------------------------

/**
 * TEST-ONLY: drive `beforeSend`'s opt-out gate directly. `initFrontendSentry`
 * is the only other thing that flips `crashReportingLive`, and it no-ops in
 * this build (`VITE_SENTRY_DSN` is baked to `""` outside a prod build via
 * `vite.config.ts`'s `define`, so `initFrontendSentry` always returns before
 * reaching the line that would flip it) — this is the only way unit tests
 * can exercise the throttle logic in `beforeSend` without a real Sentry
 * client. Not imported anywhere outside `telemetry.test.ts`.
 */
export function __setCrashReportingLiveForTests(value: boolean): void {
  crashReportingLive = value;
}

/** TEST-ONLY: reset the per-fingerprint daily throttle so tests don't leak
 * counts across cases. */
export function __resetThrottleForTests(): void {
  throttle.day = currentUtcDay();
  throttle.counts.clear();
}
