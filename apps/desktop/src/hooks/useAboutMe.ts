/**
 * Feature 014 — "About Me": free-form vocabulary context prepended to
 * the Groq cleanup system prompt. Empty body is the canonical off
 * state (byte-identical cleanup prompt to pre-feature behaviour).
 *
 * Thin wrapper around {@link useExplicitSaveText} (plan 039 task 58) —
 * see that module for the save-clobber fix and shared implementation.
 */
import {
  useExplicitSaveText,
  type ExplicitSaveStatus,
  type UseExplicitSaveText,
} from "@/hooks/useExplicitSaveText";

/** Hard cap enforced by both the backend and the UI. */
export const ABOUT_ME_MAX_LEN = 1000;

export type AboutMeStatus = ExplicitSaveStatus;

export type UseAboutMe = UseExplicitSaveText;

export function useAboutMe(): UseAboutMe {
  return useExplicitSaveText({
    getCommand: "about_me_get",
    saveCommand: "about_me_save",
    maxLen: ABOUT_ME_MAX_LEN,
    label: "About Me",
  });
}
