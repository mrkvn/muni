/**
 * Free-form "preferences" textarea on the Cleanup tab — the user's own
 * instructions that get appended after the bundled cleanup body and
 * take precedence over the system prompt when the two conflict.
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
export const USER_PROMPT_MAX_LEN = 1000;

export type UserPromptStatus = ExplicitSaveStatus;

export type UseUserPrompt = UseExplicitSaveText;

export function useUserPrompt(): UseUserPrompt {
  return useExplicitSaveText({
    getCommand: "user_prompt_get",
    saveCommand: "user_prompt_save",
    maxLen: USER_PROMPT_MAX_LEN,
    label: "preferences",
  });
}
