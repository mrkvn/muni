/**
 * Phase 10 — listen for tray menu clicks and dispatch them inside the
 * Main webview. The tray itself lives in Rust (`tray.rs`) and emits
 * `tray://*` events; this hook routes them into navigations + toasts.
 *
 * Tray nav uses `flushSync(navigate)` followed by an explicit
 * `focus_main_window` IPC call so heavy routes (Settings → General
 * with multiple `useSetting` invokes on mount) commit BEFORE macOS
 * surfaces the window. Doing the focus from Rust earlier showed the
 * previous route for a frame after the window appeared on these
 * routes — flushSync forces a synchronous React commit so by the
 * time we ask Rust to bring the window forward, the new route is
 * already painted.
 */
import { invoke } from "@tauri-apps/api/core";
import { flushSync } from "react-dom";
import { useNavigate } from "react-router-dom";
import { toast } from "sonner";

import { useTauriListen } from "@/hooks/useTauriListen";

const EVENT_OPEN_HISTORY = "tray://open-history";
const EVENT_OPEN_SETTINGS = "tray://open-settings";
const EVENT_CHECK_UPDATES = "tray://check-updates";

interface UpdaterCheckResult {
  kind: "upToDate" | "updateAvailable" | "notConfigured" | "error";
  version?: string;
  message?: string;
}

export function useTrayEvents() {
  const navigate = useNavigate();

  const navigateThenFocus = (path: string) => {
    flushSync(() => {
      navigate(path);
    });
    void invoke("focus_main_window");
  };

  useTauriListen(EVENT_OPEN_HISTORY, () => {
    navigateThenFocus("/settings/history");
  });

  useTauriListen(EVENT_OPEN_SETTINGS, () => {
    // §16.5: tray "Settings" should reopen Main at whichever
    // /settings/* sub-route the user was on before ⌘W. Hard-coding
    // /settings/general here clobbers that — if the user was
    // configuring API Keys, ⌘W'd, then clicked tray Settings,
    // they'd land on General with their thought interrupted.
    // Only navigate to General when the current route is NOT
    // already inside Settings (e.g., Home, or first launch).
    const path = window.location.pathname;
    if (path.startsWith("/settings/")) {
      void invoke("focus_main_window");
      return;
    }
    navigateThenFocus("/settings/general");
  });

  useTauriListen(EVENT_CHECK_UPDATES, () => {
    void (async () => {
      // Updates check doesn't navigate, but the toast still needs a
      // visible Main window to land in.
      void invoke("focus_main_window");
      const id = toast.loading("Checking for updates…");
      try {
        const result = await invoke<UpdaterCheckResult>("check_for_updates");
        renderUpdaterResult(id, result);
      } catch (e) {
        toast.error(`Update check failed: ${String(e)}`, { id });
      }
    })();
  });
}

function renderUpdaterResult(id: string | number, result: UpdaterCheckResult) {
  switch (result.kind) {
    case "upToDate":
      toast.success("You're up to date.", { id });
      return;
    case "updateAvailable":
      // The download/install now proceeds silently in the background; the
      // "ready to restart" moment is announced by the window title + the
      // tray badge/menu, not here.
      toast.success(
        result.version
          ? `Update ${result.version} found — downloading in the background.`
          : "Update found — downloading in the background.",
        { id },
      );
      return;
    case "notConfigured":
      toast.info("Updates aren't configured for this build.", { id });
      return;
    case "error":
      toast.error(result.message ?? "Update check failed.", { id });
      return;
  }
}
