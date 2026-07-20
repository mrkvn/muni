import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";

export type AppWindowLabel = "main" | "onboarding";

export function getAppWindowLabel(): AppWindowLabel {
  const label = getCurrentWebviewWindow().label;
  return label === "onboarding" ? "onboarding" : "main";
}
