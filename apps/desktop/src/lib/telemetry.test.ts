/**
 * Plan 039 task 63 — unit tests for the pure exported functions in
 * `telemetry.ts`: `scrubPath` (PII redaction), `beforeSend`'s per-
 * fingerprint daily throttle, and the opt-out flip. Privacy-load-bearing,
 * trivially testable.
 *
 * `initFrontendSentry()` itself isn't exercised: `VITE_SENTRY_DSN` is baked
 * to `""` outside a real prod build (`vite.config.ts`'s `define`), so it
 * always no-ops here regardless of env stubbing — that's WHY the
 * `__setCrashReportingLiveForTests` / `__resetThrottleForTests` test-only
 * exports exist, to drive `beforeSend`'s state directly.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  __resetThrottleForTests,
  __setCrashReportingLiveForTests,
  beforeSend,
  scrubPath,
} from "./telemetry";

import type { ErrorEvent } from "@sentry/react";

function makeEvent(overrides: Partial<ErrorEvent> = {}): ErrorEvent {
  return {
    exception: {
      values: [{ type: "TypeError", value: "boom" }],
    },
    ...overrides,
  } as ErrorEvent;
}

describe("scrubPath", () => {
  it("redacts a macOS home directory path", () => {
    expect(scrubPath("/Users/alice/Library/Logs/muni.log")).toBe(
      "/Users/<redacted>/Library/Logs/muni.log",
    );
  });

  it("redacts by structure, not by a known-name allowlist", () => {
    // Any single path segment after /Users/ must be redacted — a UUID, a
    // hex string, an email local-part — not just recognizable names.
    for (const segment of [
      "550e8400-e29b-41d4-a716-446655440000",
      "a1b2c3d4e5f6",
      "someone.else",
    ]) {
      expect(scrubPath(`/Users/${segment}/repo/file.ts`)).toBe(
        "/Users/<redacted>/repo/file.ts",
      );
    }
  });

  it("redacts every occurrence in a multi-path string", () => {
    const input = "/Users/alice/a.ts imports /Users/bob/b.ts";
    expect(scrubPath(input)).toBe(
      "/Users/<redacted>/a.ts imports /Users/<redacted>/b.ts",
    );
  });

  it("leaves strings with no home path untouched", () => {
    expect(scrubPath("no paths here")).toBe("no paths here");
    expect(scrubPath("/var/log/system.log")).toBe("/var/log/system.log");
  });
});

describe("beforeSend", () => {
  beforeEach(() => {
    __resetThrottleForTests();
    __setCrashReportingLiveForTests(false);
  });

  afterEach(() => {
    __setCrashReportingLiveForTests(false);
    __resetThrottleForTests();
    vi.useRealTimers();
  });

  it("drops every event while reporting is off (fail-closed default)", () => {
    expect(beforeSend(makeEvent())).toBeNull();
  });

  it("passes events through once reporting is armed", () => {
    __setCrashReportingLiveForTests(true);
    const event = makeEvent();
    expect(beforeSend(event)).not.toBeNull();
  });

  it("flips to dropping everything the instant reporting is opted out", () => {
    __setCrashReportingLiveForTests(true);
    expect(beforeSend(makeEvent())).not.toBeNull();

    __setCrashReportingLiveForTests(false);
    expect(beforeSend(makeEvent())).toBeNull();
  });

  it("scrubs a macOS home path out of the exception value before send", () => {
    __setCrashReportingLiveForTests(true);
    const event = makeEvent({
      exception: {
        values: [
          { type: "Error", value: "at /Users/alice/project/index.ts:1:1" },
        ],
      },
    });

    const sent = beforeSend(event);

    expect(sent?.exception?.values?.[0]?.value).toBe(
      "at /Users/<redacted>/project/index.ts:1:1",
    );
  });

  it("throttles: drops an event once its fingerprint hits the daily cap", () => {
    __setCrashReportingLiveForTests(true);
    const sameFingerprint = () =>
      makeEvent({ exception: { values: [{ type: "Error", value: "same" }] } });

    // Cap is 5/day (MAX_PER_FINGERPRINT_PER_DAY) — the first 5 pass.
    for (let i = 0; i < 5; i++) {
      expect(beforeSend(sameFingerprint())).not.toBeNull();
    }
    // The 6th occurrence of the SAME fingerprint on the same day is dropped.
    expect(beforeSend(sameFingerprint())).toBeNull();
  });

  it("throttles independently per fingerprint", () => {
    __setCrashReportingLiveForTests(true);
    const fingerprintA = () =>
      makeEvent({ exception: { values: [{ type: "Error", value: "A" }] } });
    const fingerprintB = () =>
      makeEvent({ exception: { values: [{ type: "Error", value: "B" }] } });

    for (let i = 0; i < 5; i++) {
      expect(beforeSend(fingerprintA())).not.toBeNull();
    }
    expect(beforeSend(fingerprintA())).toBeNull();
    // A different fingerprint has its own, untouched budget.
    expect(beforeSend(fingerprintB())).not.toBeNull();
  });

  it("resets the throttle when the UTC day rolls over", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-01-01T23:59:00Z"));
    __setCrashReportingLiveForTests(true);
    const sameFingerprint = () =>
      makeEvent({ exception: { values: [{ type: "Error", value: "daily" }] } });

    for (let i = 0; i < 5; i++) {
      expect(beforeSend(sameFingerprint())).not.toBeNull();
    }
    expect(beforeSend(sameFingerprint())).toBeNull();

    // Cross midnight UTC into the next day.
    vi.setSystemTime(new Date("2026-01-02T00:00:01Z"));
    expect(beforeSend(sameFingerprint())).not.toBeNull();
  });

  it("falls back to the raw message when there's no exception", () => {
    __setCrashReportingLiveForTests(true);
    const event = makeEvent({ exception: undefined, message: "plain message" });
    expect(beforeSend(event)).not.toBeNull();
  });
});
