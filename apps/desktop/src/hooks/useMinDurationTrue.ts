import { useEffect, useRef, useState } from "react";

/**
 * Hold a boolean `true` for at least `minMs` after it first becomes true.
 *
 * When `value` flips to true, the hook latches `true` and records the start
 * time. When `value` flips to false:
 *   - If at least `minMs` has elapsed since the latch, the hook returns
 *     `false` immediately.
 *   - Otherwise, it stays `true` until the remaining time elapses, then
 *     returns `false`.
 *
 * If `value` flips back to true before the floor is reached, the latch
 * resets and the timer is cleared — no flicker.
 *
 * Used by the HUD to keep the waveform pill mounted long enough for its
 * Framer-Motion entrance animation to actually paint, even when the
 * underlying session state transitions away in microseconds (e.g. a press
 * with a missing API key fails `pool.take()` synchronously and goes
 * Listening → Error within ~1 ms).
 */
export function useMinDurationTrue(value: boolean, minMs: number): boolean {
  const [held, setHeld] = useState(value);
  const startedAtRef = useRef<number | null>(value ? Date.now() : null);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    if (value) {
      // Cancel any pending fall-to-false; latch and (re)start the floor timer.
      if (timerRef.current) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
      startedAtRef.current = Date.now();
      setHeld(true);
      return;
    }

    // value === false
    const startedAt = startedAtRef.current;
    if (startedAt === null) {
      // Was already false; nothing to do.
      setHeld(false);
      return;
    }

    const elapsed = Date.now() - startedAt;
    if (elapsed >= minMs) {
      setHeld(false);
      startedAtRef.current = null;
      return;
    }

    const remaining = minMs - elapsed;
    timerRef.current = setTimeout(() => {
      setHeld(false);
      startedAtRef.current = null;
      timerRef.current = null;
    }, remaining);

    return () => {
      if (timerRef.current) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
    };
  }, [value, minMs]);

  return held;
}
