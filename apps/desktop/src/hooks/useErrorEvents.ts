/**
 * Phase 10 — frontend half of the ErrorPresenter.
 *
 * The Rust side (`error_presenter.rs`) emits two events the Main
 * webview cares about:
 *   - `error://quiet`            — quiet errors that don't deserve a
 *                                   system notification. Surfaced as a
 *                                   muted toast so the user notices but
 *                                   isn't yanked away from their work.
 *   - `error://navigate-to-tab` — any error with an associated
 *                                   Settings tab. We auto-navigate the
 *                                   router so the user lands where they
 *                                   can fix the underlying issue (e.g.
 *                                   missing API key → API Keys tab).
 */
import { useNavigate } from "react-router-dom";
import { toast } from "sonner";

import { useTauriListen } from "@/hooks/useTauriListen";

const EVENT_ERROR_QUIET = "error://quiet";
const EVENT_NAVIGATE_TO_TAB = "error://navigate-to-tab";

interface ErrorPayload {
  kind: string;
  severity: "loud" | "quiet";
  userMessage: string;
  // Plan 039 task 61b — `"about"` was removed: no `MuniError` variant maps
  // to `SettingsTab::About` (grepped `error.rs`'s `settings_tab()` match
  // arms), so the FE had a union member Rust never emits and a route
  // `pathForTab` never handled. Re-add it only alongside a real Rust
  // emitter + a route mapping.
  settingsTab?: "general" | "hotkey" | "cleanup" | "history" | "apiKeys" | null;
}

export function useErrorEvents() {
  const navigate = useNavigate();

  useTauriListen<ErrorPayload>(EVENT_ERROR_QUIET, (payload) => {
    toast.warning(payload.userMessage);
  });

  useTauriListen<ErrorPayload>(EVENT_NAVIGATE_TO_TAB, (payload) => {
    const path = pathForTab(payload.settingsTab);
    if (path) navigate(path);
  });
}

function pathForTab(tab: ErrorPayload["settingsTab"]): string | null {
  switch (tab) {
    case "general":
      return "/settings/general";
    case "hotkey":
      return "/settings/hotkey";
    case "cleanup":
      return "/settings/cleanup";
    case "history":
      return "/settings/history";
    case "apiKeys":
      return "/settings/api-keys";
    default:
      return null;
  }
}
