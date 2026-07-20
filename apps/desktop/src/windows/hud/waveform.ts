/**
 * Pure helpers for the HUD waveform. Split from {@link HudWindow} so the
 * geometry math can be exercised by Vitest without rendering React.
 */

/** Number of bars in the HUD waveform. */
export const BAR_COUNT = 10;

/** Minimum bar height (px) when the user is silent. */
export const MIN_BAR_PX = 3;

/** Maximum bar height (px) at peak amplitude. */
export const MAX_BAR_PX = 26;

/**
 * Soften and boost a raw RMS amplitude so quiet speech still moves the bars.
 *
 * Deepgram-grade RMS amplitudes for normal speech sit in the 0.05–0.25 range;
 * we boost ×3 to fill the 0..1 envelope, then apply sqrt() so very quiet
 * speech still produces noticeable movement (a perceptual gamma curve).
 */
export function shapeAmplitude(raw: number | null | undefined): number {
  if (raw == null || Number.isNaN(raw) || raw <= 0) return 0;
  const boosted = Math.min(raw * 3, 1);
  return Math.sqrt(boosted);
}

export interface BarGeometry {
  minPx: number;
  maxPx: number;
}

/**
 * Lower bound on the per-bar random scale used by the spectrum waveform.
 * At full amplitude every bar still reaches at least this fraction of the
 * span — keeps the bar row from collapsing to flat MIN heights on an
 * unlucky `rng()` roll.
 */
export const SPECTRUM_MIN_GAIN = 0.3;

/**
 * One tick of "spectrum" bar heights: each bar picks an independent
 * random gain in [SPECTRUM_MIN_GAIN, 1] and scales the amplitude span by
 * it. Reads as a music-visualiser chatter rather than a coherent
 * waveform.
 *
 * Pure: callers inject `rng` so tests can pin the output.
 *
 * @param amplitude Raw RMS amplitude (0..1+, may be null pre-audio).
 * @param count     Number of bars to emit. Defaults to {@link BAR_COUNT}.
 * @param rng       0..1 source; defaults to `Math.random`.
 * @param geometry  Min/max pixel heights. Defaults to ({@link MIN_BAR_PX},
 *                  {@link MAX_BAR_PX}).
 */
export function computeSpectrumTargetHeights(
  amplitude: number | null | undefined,
  count: number = BAR_COUNT,
  rng: () => number = Math.random,
  geometry: BarGeometry = { minPx: MIN_BAR_PX, maxPx: MAX_BAR_PX },
): number[] {
  const shaped = shapeAmplitude(amplitude);
  const span = geometry.maxPx - geometry.minPx;
  const variableGain = 1 - SPECTRUM_MIN_GAIN;
  return Array.from({ length: count }, () => {
    const gain = SPECTRUM_MIN_GAIN + variableGain * rng();
    return geometry.minPx + span * shaped * gain;
  });
}
