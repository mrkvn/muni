// @vitest-environment jsdom
import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { HudNotice as HudNoticeData } from "@/hooks/useHudNotice";
import type { SessionStateWire } from "@/hooks/useSessionState";

// Mutable session state the mocked hook reports; each test sets it before
// rendering.
let mockState: SessionStateWire = "idle";

vi.mock("@/hooks/useSessionState", () => ({
  useSessionState: () => mockState,
}));

// Mutable HUD notice the mocked hook reports; defaults to none so existing
// pill assertions are unaffected.
let mockNotice: HudNoticeData | null = null;

vi.mock("@/hooks/useHudNotice", () => ({
  useHudNotice: () => mockNotice,
  HUD_NOTICE_SETTLE_MS: 150,
}));

// The pill must paint on first render for these assertions; mock the
// min-duration latch to pass through its `active` arg so visibility tracks
// the session state synchronously without timers. (The latch's own
// hold-open behaviour is covered by useMinDurationTrue.test.tsx.)
vi.mock("@/hooks/useMinDurationTrue", () => ({
  useMinDurationTrue: (active: boolean) => active,
}));

// Avoid the real spectrum interval + amplitude subscription in jsdom. The
// HUD now consumes amplitude only through useSpectrumHeights (which owns the
// subscription), so that single mock covers it.
vi.mock("@/hooks/useSpectrumHeights", () => ({
  useSpectrumHeights: () => [4, 8, 12, 16, 20, 18, 14, 10, 6, 4],
}));

import { HudWindow } from "./HudWindow";

function renderAt(state: SessionStateWire) {
  mockState = state;
  const { container } = render(<HudWindow />);
  const pill = container.querySelector("[data-variant]");
  if (!pill) throw new Error(`no pill rendered for state "${state}"`);
  return pill;
}

afterEach(() => {
  cleanup();
  mockNotice = null;
});

describe("HudWindow pill styling", () => {
  it("draws listening on the brighter black and the processing states dimmer", () => {
    expect(renderAt("listening").className).toContain("bg-black/70");
    cleanup();
    for (const state of ["cleaning", "recovering"] as const) {
      expect(renderAt(state).className).toContain("bg-black/55");
      cleanup();
    }
  });

  it("keeps a 1px border on every variant so the box model never shifts", () => {
    // Identical outer geometry across phases is load-bearing: the pill must
    // not jump 1px on a cleaning → recovering transition. Every variant
    // carries `border` + a fixed height; only the colour differs.
    for (const state of ["listening", "cleaning", "recovering"] as const) {
      const cls = renderAt(state).className;
      expect(cls).toMatch(/(^|\s)border(\s|$)/);
      expect(cls).toContain("h-[34px]");
      cleanup();
    }
  });

  it("collapses listeningLocked to the listening variant (transparent ring, bars)", () => {
    const pill = renderAt("listeningLocked");
    expect(pill.getAttribute("data-variant")).toBe("listening");
    expect(pill.className).toContain("border-transparent");
    expect(pill.querySelectorAll('[data-testid="hud-bar"]').length).toBe(10);
  });

  it("draws listening and cleaning with a transparent (no amber) ring", () => {
    for (const state of ["listening", "cleaning"] as const) {
      const cls = renderAt(state).className;
      expect(cls).toContain("border-transparent");
      expect(cls).not.toContain("amber");
      cleanup();
    }
  });

  it("flags recovering with an amber hairline ring, not a transparent one", () => {
    const pill = renderAt("recovering");
    expect(pill.getAttribute("data-variant")).toBe("recovering");
    expect(pill.className).toContain("border-amber-600/70");
    expect(pill.className).not.toContain("border-transparent");
    // Indicator stays the dots+spinner so width holds across cleaning → recovering.
    expect(pill.querySelector('[data-testid="hud-processing-dots"]')).not.toBeNull();
    expect(pill.querySelector('[data-testid="hud-processing-spinner"]')).not.toBeNull();
  });
});

describe("HudWindow notice mounting", () => {
  it("renders the notice chip above the pill while listening", () => {
    mockState = "listening";
    mockNotice = { text: "Connection dropped", tone: "neutral" };
    const { container } = render(<HudWindow />);

    const chip = container.querySelector('[data-testid="hud-notice"]');
    const pill = container.querySelector("[data-variant]");
    if (!chip || !pill) throw new Error("expected both chip and pill to render");

    // Chip must precede the pill in DOM order so it stacks above it in the
    // column flow.
    expect(
      chip.compareDocumentPosition(pill) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
  });

  it("keeps a single pill mounted under the chip when a notice outlives the dictation", () => {
    // A notice can fire after release (cleanup-skipped) or on a failed press
    // (key rejected), when the session is back to idle/error. Rather than spawn
    // a separate pill, the existing pill stays mounted under the chip for its
    // lifetime — so no extra pill has to animate in. Exactly one pill renders,
    // and it reuses a normal variant (never a bespoke "notice" pill).
    mockState = "idle";
    mockNotice = { text: "Deepgram key rejected — check Settings", tone: "neutral" };
    const { container } = render(<HudWindow />);

    const chip = container.querySelector('[data-testid="hud-notice"]');
    const pills = container.querySelectorAll("[data-variant]");
    if (!chip || pills.length !== 1) {
      throw new Error(`expected chip + exactly one pill, got ${pills.length} pills`);
    }
    expect(["listening", "cleaning", "recovering"]).toContain(
      pills[0]?.getAttribute("data-variant"),
    );
    // Chip stacks above the held pill.
    expect(
      chip.compareDocumentPosition(pills[0]!) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
  });

  it("never shows more than one pill when a notice fires while listening", () => {
    // Mid-dictation FYI (e.g. a connection blip while still listening): the
    // chip stacks on the existing listening pill — no second pill appears.
    mockState = "listening";
    mockNotice = { text: "Connection dropped", tone: "neutral" };
    const { container } = render(<HudWindow />);

    const pills = container.querySelectorAll("[data-variant]");
    expect(pills.length).toBe(1);
    expect(pills[0]?.getAttribute("data-variant")).toBe("listening");
  });

  it("keeps the pill mounted in the error state so it doesn't blink before the notice", () => {
    // Rust emits SessionState::Error (hides pill) one beat before hud://notice
    // (raises chip + holds pill). If `error` didn't keep the pill up, the same
    // pill element would exit on `error` and re-enter on the notice — a visible
    // blink. Here `error` with no notice yet must still render the pill.
    mockState = "error";
    mockNotice = null;
    const { container } = render(<HudWindow />);
    expect(container.querySelector("[data-variant]")).not.toBeNull();
  });

  it("keeps the chip hidden (mounted but not visible) when there is no notice", () => {
    // The chip element is persistent to avoid a remount flash; with no notice
    // it stays mounted but marked hidden and carries no text.
    mockState = "listening";
    mockNotice = null;
    const { container } = render(<HudWindow />);
    const chip = container.querySelector('[data-testid="hud-notice"]');
    expect(chip).not.toBeNull();
    expect(chip?.getAttribute("data-visible")).toBe("false");
    expect(chip?.textContent).toBe("");
  });
});
