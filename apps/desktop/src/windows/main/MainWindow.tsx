/**
 * Phase 8 — Main window shell. Hosts a HashRouter that drives
 * `SettingsLayout`'s pane visibility (per the plan's R6 revision:
 * Settings is mounted inside the Main window, not in a separate
 * window).
 *
 * `HashRouter` is the right fit for a Tauri webview: in production
 * the page is loaded from a custom protocol, and the hash variant
 * keeps deep links intact regardless of the host scheme.
 *
 * Phase 10 polish — `SettingsLayout` itself now renders every route
 * up-front and toggles visibility via `display: none`, so we no
 * longer need the `<Routes>/<Route>` matcher here. The router is
 * still needed for `NavLink` highlighting, `useNavigate`, and the
 * `useLocation` read inside `SettingsLayout`.
 */
import { HashRouter } from "react-router-dom";

import { Toaster } from "@/components/ui/sonner";
import { UpdateTitlebar } from "@/components/UpdateTitlebar";
import { useErrorEvents } from "@/hooks/useErrorEvents";
import { useTrayEvents } from "@/hooks/useTrayEvents";
import { SettingsLayout } from "@/windows/main/SettingsLayout";

export function MainWindow() {
  return (
    <HashRouter>
      <RouterChrome />
      {/* Overlay title bar: a thin draggable strip that reserves the band
          under the (transparent) native title bar and hosts the right-aligned
          "update ready" cue, with the app filling the rest. */}
      <div className="flex h-screen w-screen flex-col bg-background">
        <UpdateTitlebar />
        <div className="min-h-0 flex-1">
          <SettingsLayout />
        </div>
      </div>
      <Toaster position="top-right" />
    </HashRouter>
  );
}

/**
 * Side-effect-only wrapper. Subscribes to tray + error events and
 * dispatches them through the React Router context. Renders nothing.
 */
function RouterChrome() {
  useTrayEvents();
  useErrorEvents();
  return null;
}
