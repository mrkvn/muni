import { motion, type Variants } from "framer-motion";
import { useEffect, useState } from "react";

import type { HudNotice as HudNoticeData } from "@/hooks/useHudNotice";

interface HudNoticeProps {
  notice: HudNoticeData | null;
}

const REVEALED = "inset(0 0 0 0 round 9999px)";
const CLIPPED = "inset(0 100% 0 0 round 9999px)";

/**
 * Persistent-element animation. Entrance is a left→right wipe; exit is a plain
 * fade (NOT a reverse wipe) — so the clip is snapped back to the clipped start
 * only AFTER the fade completes, leaving the next entrance free to wipe again.
 */
const CHIP_VARIANTS: Variants = {
  shown: {
    opacity: 1,
    clipPath: REVEALED,
    transition: { duration: 0.22, ease: "easeOut" },
  },
  hidden: {
    opacity: 0,
    clipPath: CLIPPED,
    transition: {
      opacity: { duration: 0.18, ease: "easeOut" },
      // Hold the clip revealed through the fade, then reset it instantly once
      // the chip is invisible — a whole-chip fade-out, no reverse wipe.
      clipPath: { delay: 0.18, duration: 0 },
    },
  },
};

/**
 * A short FYI chip stacked ABOVE the HUD pill (Feature 035).
 *
 * Surfaces non-actionable, in-the-moment errors a heads-down user would
 * otherwise miss — "Connection dropped", "Pasted without cleanup",
 * "No speech detected". No leading dot, translucent black blur, `white/80`
 * text; the `amber` tone recolours the text + adds an amber hairline ring to
 * flag a "text may be lost / double-check" warning.
 *
 * The chip element is **always mounted** and merely animates between
 * {@link HIDDEN} and {@link SHOWN}; it is never unmounted/remounted. Mounting a
 * fresh Framer element makes WKWebView paint it fully-formed for a frame before
 * `initial` is applied — a flash visible from the *second* appearance onward
 * (the first mount has no prior compositor state). Keeping one persistent
 * element sidesteps the mount path entirely, so every appearance wipes in
 * cleanly. The last text/tone is retained so the chip keeps its content while
 * it clips out instead of blanking mid-exit.
 *
 * It is absolutely positioned above the pill so its persistent (hidden) box
 * never reserves layout space — the pill keeps its position whether or not a
 * chip is showing.
 *
 * The HUD window is click-through and `aria-hidden`; the chip inherits both
 * (`pointer-events-none` + `aria-hidden`) so it never steals focus or input.
 */
export function HudNotice({ notice }: HudNoticeProps) {
  const [last, setLast] = useState<HudNoticeData | null>(null);
  useEffect(() => {
    if (notice) setLast(notice);
  }, [notice]);

  const visible = notice !== null;

  // Hard, React-controlled paint gate. The chip paints while visible AND for a
  // short grace after it hides (so the fade-out can play), then becomes
  // `visibility:hidden` — a paint-skip WKWebView honours even across an
  // unrelated repaint (the next press). Without this, Framer's `opacity:0`/
  // `clip-path` hidden state gets painted-through for a frame and flashes the
  // retained text. Deterministic here (not via Framer) so it can never stick.
  const [painted, setPainted] = useState(false);
  useEffect(() => {
    if (visible) {
      setPainted(true);
      return;
    }
    const t = setTimeout(() => setPainted(false), 220);
    return () => clearTimeout(t);
  }, [visible]);

  const tone = (notice ?? last)?.tone ?? "neutral";
  const text = (notice ?? last)?.text ?? "";
  const isAmber = tone === "amber";

  const chipClass = [
    "inline-flex items-center whitespace-nowrap rounded-full px-[9px] py-[3px]",
    "bg-black/55 backdrop-blur-md text-[12px] font-medium",
    isAmber ? "text-amber-300/95 ring-1 ring-amber-600/70" : "text-white/80",
  ].join(" ");

  return (
    <div className="pointer-events-none absolute bottom-[calc(100%+7px)] left-1/2 -translate-x-1/2">
      <motion.div
        aria-hidden="true"
        data-testid="hud-notice"
        data-tone={tone}
        data-visible={visible}
        className={chipClass}
        // `initial={false}` + a persistent element: Framer jumps to the current
        // `animate` variant with no enter animation on mount (the chip starts
        // hidden), then animates hidden<->shown as `notice` toggles. No mount =
        // no fully-formed flash. See CHIP_VARIANTS for the wipe-in / fade-out.
        initial={false}
        animate={visible ? "shown" : "hidden"}
        variants={CHIP_VARIANTS}
        // First-paint guard + the React-controlled paint gate. `visibility` is
        // driven by `painted` (not Framer) so the hidden chip is a hard
        // paint-skip and can never get stuck visible.
        style={{
          opacity: 0,
          clipPath: CLIPPED,
          visibility: painted ? "visible" : "hidden",
        }}
      >
        {text}
      </motion.div>
    </div>
  );
}
