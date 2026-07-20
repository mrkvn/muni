import "@/styles/globals.css";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { MainWindow } from "@/windows/main/MainWindow";
import { OnboardingWindow } from "@/windows/onboarding/OnboardingWindow";
import { getAppWindowLabel } from "@/lib/window-label";
import { initFrontendSentry, TelemetryErrorBoundary } from "@/lib/telemetry";

const root = document.getElementById("root");
if (!root) {
  throw new Error("Root element #root not found");
}

const label = getAppWindowLabel();
const Component = label === "onboarding" ? OnboardingWindow : MainWindow;

// Feature 033 — install webview crash capture before first render. No-op unless
// a DSN is baked (prod only) AND the user's crash-reporting toggle is on; safe
// to fire-and-forget since `init` installs Sentry's global handlers
// synchronously once it resolves, and the error boundary captures nothing until
// then. The privacy line (no transcript content) is enforced in `telemetry.ts`.
void initFrontendSentry();

createRoot(root).render(
  <StrictMode>
    <TelemetryErrorBoundary fallback={<RootCrashFallback />}>
      <Component />
    </TelemetryErrorBoundary>
  </StrictMode>,
);

/**
 * Minimal fallback shown when the webview throws an uncaught render error. Kept
 * deliberately static (no app components, no Tauri calls) so it can't itself
 * crash, and distinct from the live UI so the boundary doesn't re-render the
 * faulty tree into a loop. The error is already captured by Sentry (when
 * enabled) via the boundary's `onError` path.
 */
function RootCrashFallback() {
  return (
    <main
      role="alert"
      style={{
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        justifyContent: "center",
        height: "100vh",
        gap: "0.5rem",
        padding: "2rem",
        textAlign: "center",
        font: "14px system-ui, sans-serif",
      }}
    >
      <p>Something went wrong.</p>
      <p style={{ opacity: 0.7 }}>Please restart Muni.</p>
    </main>
  );
}
