import { AnimatePresence, motion } from "framer-motion";
import { useRef } from "react";
import { HUD_NOTICE_SETTLE_MS, useHudNotice } from "@/hooks/useHudNotice";
import { useMinDurationTrue } from "@/hooks/useMinDurationTrue";
import { useTrailingTrue } from "@/hooks/useTrailingTrue";
import {
  useSessionState,
  type SessionStateWire,
} from "@/hooks/useSessionState";
import { useSpectrumHeights } from "@/hooks/useSpectrumHeights";
import { HudNotice } from "@/windows/hud/HudNotice";
import { MAX_BAR_PX } from "@/windows/hud/waveform";

/**
 * Minimum time the pill stays mounted after the underlying `Listening`
 * signal goes away. Guarantees the entrance animation has time to render
 * even when the session transitions away in microseconds (e.g. a press
 * with a missing API key fails `pool.take()` synchronously and goes
 * Listening → Error before the entrance has visibly painted).
 *
 * Kept slightly under {@link HUD_MIN_VISIBLE_MS} on the Rust side so the
 * OS window stays open through this component's exit animation.
 */
const PILL_MIN_MOUNT_MS = 320;

/**
 * How long to keep the pill up after a press settles to `idle` from an
 * `error`, so it bridges the window until the (debounced) notice chip paints
 * — covering {@link HUD_NOTICE_SETTLE_MS} plus a margin for the chip's React
 * state update. Without it the pill would blink out then back in on a failed
 * press. Only ever engaged by failures (a successful press never hits `error`).
 */
const PILL_ERROR_BRIDGE_MS = HUD_NOTICE_SETTLE_MS + 90;

/**
 * Session states that keep the pill mounted. Plan 012 extends this from
 * the old `listening`-only predicate so the pill stays visible through
 * post-release cleanup AND the Whisper-batch recovery branch. Plan 030
 * adds `listeningLocked` so tap-to-toggle sessions show the pill the
 * same way as a held PTT press. Hides on `idle` or `error`.
 */
const PILL_VISIBLE_STATES: ReadonlySet<SessionStateWire> = new Set([
  "listening",
  "listeningLocked",
  "cleaning",
  "recovering",
]);

/** Count of dots in the cleaning/recovering "processing" indicator. */
const PROCESSING_DOT_COUNT = 9;

/**
 * HUD overlay rendered in the borderless transparent `hud` window.
 *
 * Visibility is driven from two sides:
 *   - Rust toggles the OS window via `session://state-changed` (see
 *     `src-tauri/src/hud.rs`). The window is hidden until the first
 *     `Listening` transition; on non-listening transitions it stays
 *     visible for at least the Rust-side min-visible window so this
 *     React fade-out can play.
 *   - This component fades the pill in/out via `AnimatePresence`. The
 *     pill mounts on any of {@link PILL_VISIBLE_STATES}; the
 *     {@link useMinDurationTrue} hook keeps it mounted long enough to
 *     finish the entrance animation even on instant Listening → Error.
 *
 * Visual variants share the same outer pill so position doesn't jump as
 * the orchestrator transitions between phases:
 *   - `listening`       — spectrum bars driven by live amplitude.
 *   - `listeningLocked` — same visual as `listening`. Plan 030 first
 *                         shipped a distinct purple/amber/lock-glyph
 *                         affordance for peripheral-vision
 *                         discoverability, but dogfood feedback was
 *                         that the visual change is unwanted; the
 *                         locked-mode UX surface is now the gesture
 *                         itself (re-tap / Esc / tray Cancel / 60 s
 *                         timeout), not the pill chrome.
 *   - `cleaning`        — dimmed black pill, dots-and-spinner indicator.
 *   - `recovering`      — dimmed black pill + amber hairline ring, same
 *                         dots-and-spinner indicator.
 */
export function HudWindow() {
  const sessionState = useSessionState();
  const liveActive = PILL_VISIBLE_STATES.has(sessionState);
  // Pass `liveActive` so a new press cancels a still-pending notice from the
  // previous (failed) press — its error lands a beat before this hold begins,
  // and debouncing keeps that false start from ever flashing on screen.
  const notice = useHudNotice(liveActive);
  const isVisible = useMinDurationTrue(liveActive, PILL_MIN_MOUNT_MS);

  // Plan 030 — `listeningLocked` collapses to the `listening` variant
  // so the pill chrome is identical to a held PTT press.
  const variant: "listening" | "cleaning" | "recovering" =
    sessionState === "recovering"
      ? "recovering"
      : sessionState === "cleaning"
        ? "cleaning"
        : "listening";

  // Keep the pill that's already on screen mounted for the chip's lifetime and
  // stack the notice chip on top of it — rather than spawning a separate pill
  // for the notice. This means no extra pill has to animate in/out under the
  // chip. When a notice outlives the dictation (an error / post-paste FYI), the
  // session has left the live states and `variant` would snap back to the
  // `listening` default, so freeze the last live variant: the held pill stays
  // visually identical (no dots → bars morph) until the chip clears.
  const lastLiveVariantRef = useRef<"listening" | "cleaning" | "recovering">(
    "listening",
  );
  if (liveActive) lastLiveVariantRef.current = variant;
  const shownVariant = liveActive ? variant : lastLiveVariantRef.current;

  // Keep the pill up across a failed press's handoff to the notice chip. On an
  // error, Rust emits `SessionState::Error` (hides pill), the press settles to
  // `idle`, and only then — after the debounce settle — does the chip paint and
  // re-hold the pill. `recentlyErrored` trails the `error` state by
  // `PILL_ERROR_BRIDGE_MS` (> the chip settle window) so the pill rides
  // continuously from the live pill, through the failure, to the chip+pill
  // state — no blink. A successful dictation never enters `error`, so the
  // normal path is untouched; the notice then holds the pill through its dwell.
  const recentlyErrored = useTrailingTrue(
    sessionState === "error",
    PILL_ERROR_BRIDGE_MS,
  );
  const showPill = isVisible || recentlyErrored || notice !== null;

  // Every variant carries a 1px border so the box model is identical
  // across phases — only the border *colour* changes, which keeps the
  // pill from shifting 1px on a cleaning → recovering transition.
  // `recovering` reuses the dimmed cleaning black but swaps the
  // transparent hairline for an amber one to flag the fallback-ASR branch.
  const pillBase =
    "flex h-[34px] items-center gap-[3px] rounded-full border px-3 backdrop-blur-md";
  const pillClass =
    shownVariant === "recovering"
      ? `${pillBase} bg-black/55 border-amber-600/70`
      : shownVariant === "cleaning"
        ? `${pillBase} bg-black/55 border-transparent`
        : `${pillBase} bg-black/70 border-transparent`;

  return (
    <main
      aria-hidden="true"
      className="pointer-events-none flex h-screen w-screen items-end justify-center bg-transparent"
    >
      {/* Bottom-anchored column holding the pill. The notice chip is rendered
          by `HudNotice` as an ABSOLUTELY-positioned element above the pill
          (hence `relative` here) — it stays mounted and never reserves layout
          space, so the pill keeps its position whether or not a chip shows.
          The chip is NOT gated on PILL_VISIBLE_STATES — a post-paste FYI can
          surface when no pill is up. */}
      <div className="relative mb-2 flex flex-col items-center justify-end">
        <HudNotice notice={notice} />
        <AnimatePresence>
          {showPill ? (
            <motion.div
              key="hud-pill"
              data-variant={shownVariant}
              initial={{ opacity: 0, scale: 0.92, y: 4 }}
              animate={{ opacity: 1, scale: 1, y: 0 }}
              exit={{ opacity: 0, scale: 0.94, y: 2 }}
              transition={{ duration: 0.12, ease: "easeOut" }}
              className={pillClass}
              style={{ minWidth: 84 }}
            >
              {shownVariant === "listening" ? (
                <SpectrumBars />
              ) : (
                <ProcessingIndicator />
              )}
            </motion.div>
          ) : null}
        </AnimatePresence>
      </div>
    </main>
  );
}

/**
 * Live waveform for the `listening` state. Renders `BAR_COUNT` bars whose
 * heights re-target on a fixed cadence from {@link useSpectrumHeights};
 * Framer Motion eases between target changes so the bar row chatters
 * without strobing.
 */
function SpectrumBars() {
  const heights = useSpectrumHeights();
  return (
    <>
      {heights.map((height, index) => (
        <Bar key={index} heightPx={height} />
      ))}
    </>
  );
}

interface BarProps {
  heightPx: number;
}

function Bar({ heightPx }: BarProps) {
  return (
    <motion.span
      aria-hidden="true"
      className="block w-[3px] rounded-full bg-white/95"
      animate={{ height: heightPx }}
      transition={{
        // Spectrum targets re-pick every ~80 ms; a 60 ms tween bridges
        // them without the discrete jumps showing through.
        duration: 0.06,
        ease: "easeOut",
      }}
      // Initial height is set so the very first paint shows resting bars
      // instead of jumping up from 0px.
      style={{ height: heightPx, maxHeight: MAX_BAR_PX }}
      data-testid="hud-bar"
    />
  );
}

/**
 * Cleaning + recovering indicator: a row of tiny pulsing dots followed by
 * a small spinner. Used while the pipeline is doing post-release work
 * (cleanup pass) or while the fallback ASR is in flight. Both states
 * share the dimmed black pill; `recovering` adds an amber hairline ring
 * to flag the fallback branch. The indicator itself is the same so the
 * pill width stays steady through a cleaning → recovering transition.
 */
function ProcessingIndicator() {
  return (
    <>
      <span
        aria-hidden="true"
        className="flex items-center gap-[2px]"
        data-testid="hud-processing-dots"
      >
        {Array.from({ length: PROCESSING_DOT_COUNT }).map((_, i) => (
          <span
            key={i}
            aria-hidden="true"
            className="block h-[2px] w-[2px] rounded-full bg-white/85"
            style={{
              animation: "hudDotPulse 1.4s ease-in-out infinite",
              animationDelay: `${(i * 0.08).toFixed(2)}s`,
            }}
          />
        ))}
      </span>
      <span
        aria-hidden="true"
        className="ml-1 block h-[14px] w-[14px] rounded-full"
        data-testid="hud-processing-spinner"
        style={{
          border: "1.4px solid rgba(255,255,255,0.18)",
          borderTopColor: "rgba(255,255,255,0.9)",
          animation: "hudSpin 0.85s linear infinite",
        }}
      />
    </>
  );
}
