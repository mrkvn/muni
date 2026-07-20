import { useEffect, useRef, useState } from "react";
import { AMPLITUDE_EVENT } from "@/hooks/useAudioAmplitude";
import { useTauriListen } from "@/hooks/useTauriListen";
import {
  BAR_COUNT,
  MIN_BAR_PX,
  computeSpectrumTargetHeights,
} from "@/windows/hud/waveform";

/**
 * How often the per-bar random targets are repicked. 80 ms is fast enough
 * that the bars feel reactive but slow enough that Framer Motion's per-bar
 * height tween (60 ms in {@link HudWindow}'s `Bar`) can settle between
 * picks — together they produce a "chattering EQ" look without strobing.
 */
const REPICK_INTERVAL_MS = 80;

const REST_HEIGHTS: readonly number[] = Object.freeze(
  Array.from({ length: BAR_COUNT }, () => MIN_BAR_PX),
);

/**
 * Drives the HUD's spectrum waveform: returns `BAR_COUNT` independently
 * randomised target heights that refresh every {@link REPICK_INTERVAL_MS}
 * based on the latest amplitude.
 *
 * ## Why this hook owns the amplitude subscription (instead of taking a prop)
 *
 * `audio://amplitude` fires at ~25 Hz while the hotkey is held, but the
 * waveform only *samples* amplitude at the {@link REPICK_INTERVAL_MS} cadence
 * (12.5 Hz). Routing amplitude through React state (via {@link
 * useAudioAmplitude} + a prop, as this once did) re-rendered the entire HUD
 * tree ~25×/s purely to thread a number the waveform reads at half that rate —
 * work that produced no visual change. Instead we land each event straight
 * into {@link ampRef} with NO `setState`, and let the repick interval be the
 * only thing that re-renders. Net: the HUD repaints at the repick cadence, not
 * the event cadence — identical picture, ~3× fewer renders while recording.
 *
 * Decoupling the repick cadence from the event rate also keeps the bars
 * chattering on a quasi-stable amplitude (events can arrive bursty) — the
 * original reason this is a timer rather than a per-event recompute.
 *
 * At silence (`amplitude` ≤ 0 or null), targets collapse to MIN — the row of
 * bars renders as a flat dotted line, matching the "held but quiet" look. The
 * reactive {@link useAudioAmplitude} hook stays available for the developer
 * overlay, which genuinely wants per-event state.
 */
export function useSpectrumHeights(): readonly number[] {
  const [heights, setHeights] = useState<readonly number[]>(REST_HEIGHTS);

  // Latest amplitude, refreshed on every `audio://amplitude` event WITHOUT a
  // re-render. The repick interval below samples this at its own cadence.
  const ampRef = useRef<number | null>(null);

  useTauriListen<number>(AMPLITUDE_EVENT, (payload) => {
    ampRef.current = typeof payload === "number" ? payload : 0;
  });

  useEffect(() => {
    const id = setInterval(() => {
      setHeights(computeSpectrumTargetHeights(ampRef.current));
    }, REPICK_INTERVAL_MS);
    return () => clearInterval(id);
  }, []);

  return heights;
}
