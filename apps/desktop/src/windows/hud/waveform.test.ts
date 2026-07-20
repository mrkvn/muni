import { describe, expect, it } from "vitest";
import {
  BAR_COUNT,
  MAX_BAR_PX,
  MIN_BAR_PX,
  SPECTRUM_MIN_GAIN,
  computeSpectrumTargetHeights,
  shapeAmplitude,
} from "./waveform";

describe("shapeAmplitude", () => {
  it("clamps null/NaN/negative input to 0", () => {
    expect(shapeAmplitude(null)).toBe(0);
    expect(shapeAmplitude(undefined)).toBe(0);
    expect(shapeAmplitude(Number.NaN)).toBe(0);
    expect(shapeAmplitude(-0.5)).toBe(0);
  });

  it("monotonically increases with input amplitude", () => {
    const samples = [0.01, 0.05, 0.1, 0.2, 0.33, 0.5];
    const shaped = samples.map(shapeAmplitude);
    for (let i = 1; i < shaped.length; i += 1) {
      expect(shaped[i]).toBeGreaterThan(shaped[i - 1]);
    }
  });

  it("saturates at 1 for amplitudes >= 1/3", () => {
    // Boost factor is 3, so amp >= 1/3 hits the upper clamp before sqrt.
    expect(shapeAmplitude(1 / 3)).toBeCloseTo(1, 5);
    expect(shapeAmplitude(0.5)).toBeCloseTo(1, 5);
    expect(shapeAmplitude(1)).toBeCloseTo(1, 5);
  });

  it("applies a perceptual gamma so quiet speech still moves the bars", () => {
    // Without sqrt, amp 0.05 → 0.15. With sqrt(0.15) ≈ 0.387 — well above
    // the ~10% threshold below which bars look frozen on the screen.
    expect(shapeAmplitude(0.05)).toBeGreaterThan(0.15);
  });
});

describe("computeSpectrumTargetHeights", () => {
  // A counter-based RNG so the test is deterministic and we can see how the
  // function maps `rng()` rolls to per-bar gains.
  function seq(values: readonly number[]): () => number {
    let i = 0;
    return () => values[i++ % values.length] ?? 0;
  }

  it("returns minimum heights at silence regardless of rng", () => {
    const heights = computeSpectrumTargetHeights(0, 6, seq([0, 0.5, 1]));
    expect(heights).toHaveLength(6);
    for (const h of heights) {
      expect(h).toBeCloseTo(MIN_BAR_PX, 6);
    }
  });

  it("returns minimum heights when amplitude is null", () => {
    const heights = computeSpectrumTargetHeights(null, BAR_COUNT, seq([0.9]));
    for (const h of heights) {
      expect(h).toBeCloseTo(MIN_BAR_PX, 6);
    }
  });

  it("emits the requested number of bars", () => {
    const heights = computeSpectrumTargetHeights(0.5, 4, seq([0.5]));
    expect(heights).toHaveLength(4);
  });

  it("respects [SPECTRUM_MIN_GAIN, 1] bounds at saturated amplitude", () => {
    // rng()=0 → smallest possible gain; rng()=1 → largest. Both within bounds.
    const lower = computeSpectrumTargetHeights(1, BAR_COUNT, seq([0]));
    const upper = computeSpectrumTargetHeights(1, BAR_COUNT, seq([1]));
    const span = MAX_BAR_PX - MIN_BAR_PX;
    for (const h of lower) {
      expect(h).toBeCloseTo(MIN_BAR_PX + span * SPECTRUM_MIN_GAIN, 5);
    }
    for (const h of upper) {
      expect(h).toBeCloseTo(MAX_BAR_PX, 5);
    }
  });

  it("never exceeds the maximum height even at saturated amplitude", () => {
    const heights = computeSpectrumTargetHeights(1, BAR_COUNT, Math.random);
    for (const h of heights) {
      expect(h).toBeGreaterThanOrEqual(MIN_BAR_PX - 1e-6);
      expect(h).toBeLessThanOrEqual(MAX_BAR_PX + 1e-6);
    }
  });

  it("gives independent heights when the rng varies per bar", () => {
    // Distinct rng rolls must produce distinct heights — proves bars are
    // not collapsed to a single computed value.
    const heights = computeSpectrumTargetHeights(
      0.6,
      4,
      seq([0.05, 0.4, 0.75, 0.99]),
    );
    const unique = new Set(heights.map((h) => h.toFixed(4)));
    expect(unique.size).toBe(4);
  });

  it("is deterministic for a deterministic rng", () => {
    const a = computeSpectrumTargetHeights(0.5, BAR_COUNT, seq([0.1, 0.7]));
    const b = computeSpectrumTargetHeights(0.5, BAR_COUNT, seq([0.1, 0.7]));
    expect(a).toEqual(b);
  });
});
