// @vitest-environment jsdom
import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import type { HudNotice as HudNoticeData } from "@/hooks/useHudNotice";

import { HudNotice } from "./HudNotice";

afterEach(cleanup);

function renderNotice(notice: HudNoticeData | null) {
  const { container } = render(<HudNotice notice={notice} />);
  return container.querySelector('[data-testid="hud-notice"]');
}

describe("HudNotice", () => {
  it("keeps a hidden, empty chip mounted when there is no notice", () => {
    // The chip element is persistent (never unmounts) to avoid a remount flash;
    // with no notice it's present but marked hidden and carries no text.
    const chip = renderNotice(null);
    expect(chip).not.toBeNull();
    expect(chip?.getAttribute("data-visible")).toBe("false");
    expect(chip?.textContent).toBe("");
  });

  it("renders the notice text and marks the chip visible", () => {
    const chip = renderNotice({ text: "Connection dropped", tone: "neutral" });
    expect(chip).not.toBeNull();
    expect(chip?.textContent).toBe("Connection dropped");
    expect(chip?.getAttribute("data-visible")).toBe("true");
  });

  it("uses the translucent black chip with no leading dot for a neutral tone", () => {
    const chip = renderNotice({ text: "No speech detected", tone: "neutral" });
    expect(chip?.className).toContain("bg-black/55");
    expect(chip?.className).toContain("text-white/80");
    expect(chip?.className).not.toContain("amber");
    // No leading status dot — the chip is text-only.
    expect(chip?.querySelector("[data-testid='hud-notice-dot']")).toBeNull();
  });

  it("recolours and rings the chip for an amber tone (words-lost warning)", () => {
    const chip = renderNotice({ text: "Partial result — check text", tone: "amber" });
    expect(chip?.getAttribute("data-tone")).toBe("amber");
    expect(chip?.className).toContain("text-amber-300/95");
    expect(chip?.className).toContain("ring-amber-600/70");
    expect(chip?.className).not.toContain("text-white/80");
  });

  it("stays click-through and aria-hidden so it never steals input", () => {
    const chip = renderNotice({ text: "Pasted without cleanup", tone: "neutral" });
    expect(chip?.getAttribute("aria-hidden")).toBe("true");
  });
});
