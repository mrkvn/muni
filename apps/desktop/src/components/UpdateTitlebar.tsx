/**
 * Overlay title-bar strip for the main window.
 *
 * The main window uses `titleBarStyle: "Overlay"`, so the webview extends under
 * the native title bar (the traffic lights float at top-left). This thin strip
 * reserves that band and is the window's drag region. When the updater reports
 * a pending update it shows a lean, **right-aligned** cue:
 *
 *   - right-aligned so it never collides with the traffic lights,
 *   - in the title bar so it never covers page content (the original banner
 *     covered the "General" heading),
 *   - text-only — the actionable "Restart to apply" lives in the tray menu.
 *
 * Driven by the Rust updater's `updater://state` event.
 */
import { useState } from "react";
import { useTauriListen } from "@/hooks/useTauriListen";

const EVENT_UPDATER_STATE = "updater://state";

type UpdaterState =
  | { phase: "idle" }
  | { phase: "downloading" }
  | { phase: "ready"; version?: string };

export function UpdateTitlebar() {
  const [ready, setReady] = useState(false);

  useTauriListen<UpdaterState>(EVENT_UPDATER_STATE, (payload) => {
    setReady(payload.phase === "ready");
  });

  return (
    <div
      data-tauri-drag-region
      className="flex h-8 shrink-0 select-none items-center justify-end px-3"
    >
      {ready && (
        <span
          data-tauri-drag-region
          className="flex items-center gap-1.5 text-xs text-muted-foreground"
        >
          <span aria-hidden className="size-1.5 rounded-full bg-primary" />
          Update ready — restart to apply
        </span>
      )}
    </div>
  );
}
