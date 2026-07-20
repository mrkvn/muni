import { useEffect, useRef, useState } from "react";

/**
 * Hold a boolean `true` for at least `trailingMs` AFTER it flips to false.
 *
 * The mirror of {@link useMinDurationTrue} (which holds from when the value
 * becomes true): this one debounces the *falling* edge. While `value` is true
 * the hook returns true; when `value` goes false it keeps returning true for
 * `trailingMs`, then falls. If `value` goes true again within the window the
 * pending fall is cancelled.
 *
 * Used by the HUD to keep the pill up across the brief gap between a failed
 * press settling to `idle` and its (debounced) notice chip painting — see
 * `HudWindow.tsx`. Without it the pill would blink out and back in.
 */
export function useTrailingTrue(value: boolean, trailingMs: number): boolean {
  const [held, setHeld] = useState(value);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    if (value) {
      // Cancel any pending fall; latch true immediately.
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
      setHeld(true);
      return;
    }

    // value === false: hold true for the trailing window, then fall.
    timerRef.current = setTimeout(() => {
      setHeld(false);
      timerRef.current = null;
    }, trailingMs);

    return () => {
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
    };
  }, [value, trailingMs]);

  return held;
}
