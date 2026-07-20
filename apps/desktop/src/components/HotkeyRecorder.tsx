/**
 * Feature 037 — the shortcut capture widget for the Shortcuts settings route.
 *
 * A semantic, keyboard-operable `<button>` that records a hotkey when
 * activated. Two modes:
 *   - `"modifiers"` (dictation): captures a ≥2-modifier chord.
 *   - `"combo"` (re-paste): captures a modifier(s)+key combo, with a "Clear"
 *     affordance that disables the shortcut.
 *
 * All decision logic lives in the pure {@link captureReducer} (see
 * `hotkeyCapture.ts`); this component is the thin DOM/ARIA shell. While
 * recording it swallows every key (`onKeyDownCapture` + `preventDefault` +
 * `stopPropagation`) so a captured combo — e.g. ⌃⌘V — never leaks into the app.
 */
import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useReducer, useRef, useState } from "react";

import { Button } from "@/components/ui/button";
import type {
  DictationBinding,
  HotkeyModifier,
  RepasteBinding,
} from "@/hooks/useSettings";
import { cn } from "@/lib/utils";
import {
  captureReducer,
  dictationChips,
  initialCaptureState,
  isComposingKeydown,
  isDisplayableKey,
  keyDisplaySymbol,
  modifierFromCode,
  repasteChips,
} from "./hotkeyCapture";

type ModifiersRecorderProps = {
  mode: "modifiers";
  value: DictationBinding;
  onChange: (binding: DictationBinding) => void;
};

type ComboRecorderProps = {
  mode: "combo";
  value: RepasteBinding | null;
  onChange: (binding: RepasteBinding | null) => void;
};

export type HotkeyRecorderProps = (
  | ModifiersRecorderProps
  | ComboRecorderProps
) & {
  /**
   * False when the recorder's settings pane is hidden (another tab is active).
   * The pane stays mounted across tab switches (SettingsLayout toggles
   * `display: none`), so flipping this to false is what clears the transient
   * "Saved: …" confirmation — otherwise it would still be showing on return.
   * Defaults to true so standalone use (and existing tests) are unaffected.
   */
  visible?: boolean;
};

/** Backend command that suppresses/resumes the dictation trigger while recording. */
const SET_RECORDING_COMMAND = "hotkey_set_recording";

export function HotkeyRecorder(props: HotkeyRecorderProps) {
  const { mode, onChange, visible = true } = props;
  const [state, dispatch] = useReducer(
    captureReducer,
    mode,
    initialCaptureState,
  );

  // Plan 039 task 55 — the live region announces the committed chord after a
  // successful save ("Saved: ⌃⌘V"), not just validation errors. Kept separate
  // from the reducer's `error` (which is cleared on every keydown/cancel) so
  // it persists until the next recording session starts.
  const [savedAnnouncement, setSavedAnnouncement] = useState<string | null>(
    null,
  );

  // Keep the latest onChange without re-running the commit effect on every
  // parent render (the recorder commits exactly once per captured chord).
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;

  // The recorder captures keys via `onKeyDownCapture` on the button, which only
  // fires while the button holds DOM focus. macOS WebKit (WKWebView — what Tauri
  // renders in) does NOT focus a <button> on click, unlike Windows/Linux. Without
  // an explicit focus, recording would start ("Press keys…") but no key event
  // would ever reach the handler — the recorder hangs. We focus on record start.
  const buttonRef = useRef<HTMLButtonElement>(null);

  // End a recording session (commit or cancel): stop listening and RESUME the
  // dictation trigger the backend suppressed on start. Plan 039 task 55 — no
  // longer blurs the button: focus stays put (on commit, so the user keeps
  // their place; on Escape/cancel, so a keyboard user isn't dropped out of the
  // settings list).
  const stopRecording = useCallback(() => {
    dispatch({ type: "cancel" });
    void invoke(SET_RECORDING_COMMAND, { recording: false });
  }, []);

  useEffect(() => {
    if (!state.result) return;
    if (state.result.kind === "modifiers" && mode === "modifiers") {
      // Build the binding without a `key` property when the capture is
      // modifier-only, so the shape matches `{ kind, mods }` and serializes
      // with no `key` member (mirrors Rust's skip-if-none). A keyed capture
      // (Feature 038) carries the key (and its task-54 `displayKey` hint)
      // through.
      const binding: DictationBinding =
        state.result.key === undefined
          ? { kind: "modifiers", mods: state.result.mods }
          : {
              kind: "modifiers",
              mods: state.result.mods,
              key: state.result.key,
              displayKey: state.result.displayKey,
            };
      (onChangeRef.current as ModifiersRecorderProps["onChange"])(binding);
      setSavedAnnouncement(`Saved: ${dictationChips(binding).join("")}`);
    } else if (state.result.kind === "combo" && mode === "combo") {
      const binding: RepasteBinding = {
        mods: state.result.mods,
        key: state.result.key,
        displayKey: state.result.displayKey,
      };
      (onChangeRef.current as ComboRecorderProps["onChange"])(binding);
      setSavedAnnouncement(`Saved: ${repasteChips(binding).join("")}`);
    }
    stopRecording();
  }, [state.result, mode, stopRecording]);

  const toggleRecording = useCallback(() => {
    if (state.recording) {
      stopRecording();
      return;
    }
    setSavedAnnouncement(null);
    dispatch({ type: "start" });
    // Focus explicitly: WKWebView won't focus the button on click, so without
    // this the key-capture handlers never fire (see buttonRef comment above).
    buttonRef.current?.focus();
    // Suppress the dictation trigger so holding the CURRENT chord to re-record
    // it doesn't also fire dictation (the native tap is below the webview).
    // Resumed by stopRecording on every end.
    void invoke(SET_RECORDING_COMMAND, { recording: true });
  }, [state.recording, stopRecording]);

  // App-switch safety: if the whole window loses focus while recording, cancel
  // (and resume the trigger). Tab switches are covered by the button's onBlur
  // — the inactive pane is display:none, which drops focus — but a window blur
  // (Cmd-Tab to another app) may not always fire the button's blur under WebKit.
  useEffect(() => {
    if (!state.recording) return;
    const onWindowBlur = () => stopRecording();
    window.addEventListener("blur", onWindowBlur);
    return () => window.removeEventListener("blur", onWindowBlur);
  }, [state.recording, stopRecording]);

  // The "Saved: …" confirmation is transient — a receipt for the interaction
  // that just happened, not persistent state. Clear it when the user leaves
  // the context that produced it, so it never lingers as stale UI:
  //   1. Switching settings tabs → this pane's `visible` flips false (the pane
  //      stays mounted, so nothing else resets the state).
  useEffect(() => {
    if (!visible) setSavedAnnouncement(null);
  }, [visible]);

  //   2. Focusing out of the app entirely (Cmd-Tab away, click another app) →
  //      a window `blur`. Distinct from the recording-cancel blur handler
  //      above (which only runs mid-recording); this one always runs and only
  //      touches the confirmation.
  useEffect(() => {
    const onWindowBlur = () => setSavedAnnouncement(null);
    window.addEventListener("blur", onWindowBlur);
    return () => window.removeEventListener("blur", onWindowBlur);
  }, []);

  // Unmount safety: never leave the trigger suppressed if this recorder is torn
  // down mid-recording.
  useEffect(() => {
    return () => {
      void invoke(SET_RECORDING_COMMAND, { recording: false });
    };
  }, []);

  const handleKeyDownCapture = useCallback(
    (event: React.KeyboardEvent<HTMLButtonElement>) => {
      if (!state.recording) return;
      event.preventDefault();
      event.stopPropagation();
      // Escape aborts recording without committing (both modes).
      if (event.code === "Escape") {
        stopRecording();
        return;
      }
      // Task 53b — an IME composition keydown (e.g. mid-composing an accented
      // or CJK character) is not a real physical key the user intends to
      // bind; ignore it rather than feeding a bogus/placeholder code to the
      // reducer.
      if (isComposingKeydown(event.nativeEvent)) return;
      // Space/Enter would otherwise re-activate the button; they're captured
      // here as ordinary keys (combo) or ignored (modifiers) by the reducer.
      if (event.repeat) return;
      dispatch({
        type: "keydown",
        modifier: modifierFromCode(event.code),
        code: event.code,
        // Task 54 — the raw, layout-aware key value (e.g. "a" on AZERTY for
        // physical code "KeyQ"), captured only for a display hint. Omitted
        // when untrustworthy (mid dead-key composition, or an Option-chord
        // glyph like ⌥R → "®") so the code-derived fallback renders instead.
        displayKey: isDisplayableKey(event.key, event.altKey)
          ? event.key
          : undefined,
      });
    },
    [state.recording, stopRecording],
  );

  const handleKeyUpCapture = useCallback(
    (event: React.KeyboardEvent<HTMLButtonElement>) => {
      if (!state.recording) return;
      event.preventDefault();
      event.stopPropagation();
      dispatch({ type: "keyup", modifier: modifierFromCode(event.code) });
    },
    [state.recording],
  );

  const handleBlur = useCallback(() => {
    if (state.recording) stopRecording();
  }, [state.recording, stopRecording]);

  const chips =
    props.mode === "modifiers"
      ? dictationChips(props.value)
      : props.value
        ? repasteChips(props.value)
        : [];

  const errorId = useIdRef();
  // Plan 039 task 55 — the live region shows a validation error when one is
  // active, otherwise the last successful-save announcement (if any). Only
  // the error half marks the control invalid/red — a successful save must
  // never look like a validation failure.
  const liveMessage = state.error ?? savedAnnouncement;

  return (
    <div className="flex w-full flex-col items-end gap-1.5">
      <div className="flex flex-wrap items-center justify-end gap-2">
        <button
          ref={buttonRef}
          type="button"
          onClick={toggleRecording}
          onKeyDownCapture={handleKeyDownCapture}
          onKeyUpCapture={handleKeyUpCapture}
          onBlur={handleBlur}
          aria-pressed={state.recording}
          aria-label={recorderAriaLabel(props, state.recording)}
          aria-describedby={liveMessage ? errorId : undefined}
          aria-invalid={state.error ? true : undefined}
          className={cn(
            "inline-flex min-h-9 min-w-[7rem] cursor-pointer items-center justify-center gap-1 rounded-md border border-input px-3 py-1.5 text-sm transition-colors",
            "focus-visible:outline-none",
            // Focus ring only while NOT recording. We programmatically focus the
            // button to capture keys, and the user doesn't want an outline during
            // recording — the accent background + "Press keys…" text is the cue.
            // A keyboard-tab focus while idle still rings for accessibility.
            !state.recording && "focus-visible:ring-2 focus-visible:ring-ring",
            state.recording
              ? "bg-accent text-accent-foreground"
              : "bg-background hover:bg-accent hover:text-accent-foreground",
            state.error && "border-destructive",
          )}
        >
          {state.recording ? (
            <span className="text-muted-foreground">Press keys…</span>
          ) : chips.length > 0 ? (
            <ChipRow chips={chips} />
          ) : (
            <span className="text-muted-foreground">Not set</span>
          )}
        </button>

        {/* Re-paste (combo) can be cleared to disable it. */}
        {props.mode === "combo" ? (
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={() => props.onChange(null)}
            disabled={!props.value}
          >
            Clear
          </Button>
        ) : null}
      </div>

      {/* Live region so screen readers announce validation errors AND a
          successful save (task 55) as they happen. */}
      <p
        id={errorId}
        role="status"
        aria-live="polite"
        className={cn(
          "min-h-0 text-right text-xs",
          state.error ? "text-destructive" : "text-muted-foreground",
          !liveMessage && "sr-only",
        )}
      >
        {liveMessage}
      </p>
    </div>
  );
}

function ChipRow({ chips }: { chips: string[] }) {
  return (
    <span className="flex items-center gap-1">
      {chips.map((chip, index) => (
        <kbd
          // Chips are positional glyphs; the index disambiguates repeats.
          key={`${chip}-${index}`}
          className="rounded border border-border bg-background px-1.5 py-0.5 text-xs font-medium"
        >
          {chip}
        </kbd>
      ))}
    </span>
  );
}

/** Stable per-instance id for the live-region <-> button `aria-describedby`. */
function useIdRef(): string {
  const ref = useRef<string>("");
  if (!ref.current) {
    ref.current = `hotkey-recorder-status-${Math.random().toString(36).slice(2)}`;
  }
  return ref.current;
}

function recorderAriaLabel(
  props: HotkeyRecorderProps,
  recording: boolean,
): string {
  if (recording) {
    return props.mode === "modifiers"
      ? "Recording dictation shortcut. Hold at least two modifier keys, or a modifier and a key, or press Escape to cancel."
      : "Recording re-paste shortcut. Press a modifier and a key, or press Escape to cancel.";
  }
  const noun =
    props.mode === "modifiers" ? "Dictation shortcut" : "Re-paste shortcut";
  const current =
    props.mode === "modifiers"
      ? describeDictation(props.value)
      : props.value
        ? describeRepaste(props.value)
        : "not set";
  return `${noun}, currently ${current}. Activate to record a new shortcut.`;
}

function describeDictation(binding: DictationBinding): string {
  const parts = binding.mods.map(modifierName);
  if (binding.key) parts.push(keyDisplaySymbol(binding.key, binding.displayKey));
  return parts.join(" ") || "not set";
}

function describeRepaste(binding: RepasteBinding): string {
  return [
    ...binding.mods.map(modifierName),
    keyDisplaySymbol(binding.key, binding.displayKey),
  ].join(" ");
}

function modifierName(modifier: HotkeyModifier): string {
  switch (modifier) {
    case "control":
      return "Control";
    case "option":
      return "Option";
    case "command":
      return "Command";
    case "shift":
      return "Shift";
    case "fn":
      return "Fn";
    default:
      return String(modifier);
  }
}
