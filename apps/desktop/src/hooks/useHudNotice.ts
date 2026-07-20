import { useCallback, useEffect, useRef, useState } from "react";
import { useTauriListen } from "@/hooks/useTauriListen";

const HUD_NOTICE_EVENT = "hud://notice";

/**
 * How long a HUD notice chip stays on screen before it auto-clears
 * (Feature 035). The single source of truth on the frontend — the hook
 * arms an auto-clear timer with it and the chip component reads it for its
 * fade timing. Kept in lockstep with the Rust `HUD_NOTICE_DWELL_MS`
 * (`src-tauri/src/hud.rs`) so the React fade-out and the OS-window hide
 * fire together.
 */
export const HUD_NOTICE_DWELL_MS = 2500;

/**
 * How long a notice must survive — WITHOUT a new press starting — before its
 * chip actually paints. A failed press surfaces its error a beat (~10-20ms)
 * before the *next* press's `Listening`, so when a user fires presses in quick
 * succession the previous press's chip would otherwise flash for a frame and
 * then be torn down by the next press. Debouncing the appearance means such a
 * false start never paints at all; only an error the user lets settle shows.
 * Small enough to be imperceptible for a single error (the chip is an FYI, not
 * on the paste path).
 */
export const HUD_NOTICE_SETTLE_MS = 150;

/** Tone variants understood by the chip. Mirrors Rust `HudTone` (camelCase serde). */
export type HudNoticeTone = "neutral" | "amber";

/** Wire shape of the `hud://notice` event payload. Mirrors Rust `HudNoticePayload`. */
export interface HudNotice {
  text: string;
  tone: HudNoticeTone;
}

const KNOWN_TONES: ReadonlySet<HudNoticeTone> = new Set(["neutral", "amber"]);

function isHudNotice(value: unknown): value is HudNotice {
  if (typeof value !== "object" || value === null) return false;
  const candidate = value as Record<string, unknown>;
  return (
    typeof candidate.text === "string" &&
    typeof candidate.tone === "string" &&
    KNOWN_TONES.has(candidate.tone as HudNoticeTone)
  );
}

/**
 * Subscribe to `hud://notice` and expose the current notice, or `null`
 * when none is showing.
 *
 * A notice does NOT paint the instant its event arrives — it must first
 * survive {@link HUD_NOTICE_SETTLE_MS} without a new press. `pressActive` is
 * whether a dictation pill is live; on its rising edge (a NEW press begins) a
 * still-pending notice is cancelled, so the previous press's error — which
 * lands just before the next press starts — never flashes on screen. A chip
 * that has already settled and painted is left alone: it rides out its
 * {@link HUD_NOTICE_DWELL_MS} dwell (and a later notice refreshes it), so there
 * is no exit-fade blink when the next press begins either.
 *
 * Each settled notice (re)arms a single dwell timer, so a notice arriving while
 * another is on screen extends the dwell rather than stacking. All timers and
 * the listener are torn down on unmount.
 *
 * The HUD window is click-through and `aria-hidden`; the chip rendered from
 * this state is purely a peripheral-vision FYI (see {@link HudNotice}).
 */
export function useHudNotice(pressActive = false): HudNotice | null {
  const [notice, setNotice] = useState<HudNotice | null>(null);
  const settleTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const dwellTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const clearSettle = useCallback(() => {
    if (settleTimerRef.current !== null) {
      clearTimeout(settleTimerRef.current);
      settleTimerRef.current = null;
    }
  }, []);
  const clearDwell = useCallback(() => {
    if (dwellTimerRef.current !== null) {
      clearTimeout(dwellTimerRef.current);
      dwellTimerRef.current = null;
    }
  }, []);

  // Timers outlive any single render but must never survive unmount.
  useEffect(() => () => {
    clearSettle();
    clearDwell();
  }, [clearSettle, clearDwell]);

  useTauriListen<unknown>(HUD_NOTICE_EVENT, (payload) => {
    if (!isHudNotice(payload)) return;
    // Hold the notice in the settle window before painting. A new press
    // arriving during this window cancels it (see the effect below).
    clearSettle();
    settleTimerRef.current = setTimeout(() => {
      settleTimerRef.current = null;
      setNotice(payload);
      clearDwell();
      dwellTimerRef.current = setTimeout(() => {
        setNotice(null);
        dwellTimerRef.current = null;
      }, HUD_NOTICE_DWELL_MS);
    }, HUD_NOTICE_SETTLE_MS);
  });

  // Cancel a still-pending (not-yet-painted) notice on the rising edge of a new
  // press, so a previous press's error never flashes as this hold begins. A
  // chip already on screen is intentionally left alone — it persists for its
  // dwell with no exit-fade blink.
  const prevPressRef = useRef(pressActive);
  useEffect(() => {
    if (
      pressActive &&
      !prevPressRef.current &&
      settleTimerRef.current !== null
    ) {
      clearTimeout(settleTimerRef.current);
      settleTimerRef.current = null;
    }
    prevPressRef.current = pressActive;
  }, [pressActive]);

  return notice;
}
