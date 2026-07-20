// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SettingsLayout } from "./SettingsLayout";

afterEach(() => {
  cleanup();
});

// SettingsLayout now imports every route component up-front so it can
// keep them all mounted. The real components reach into Tauri IPC and
// platform-specific hooks; stub them so this test stays focused on the
// layout's nav + visibility behavior.
vi.mock("@/windows/main/routes/SettingsApiKeys", () => ({
  SettingsApiKeys: () => <p>api-keys body</p>,
}));
vi.mock("@/windows/main/routes/SettingsCleanup", () => ({
  SettingsCleanup: () => <p>cleanup body</p>,
}));
vi.mock("@/windows/main/routes/SettingsCostUsage", () => ({
  SettingsCostUsage: () => <p>cost-usage body</p>,
}));
vi.mock("@/windows/main/routes/SettingsGeneral", () => ({
  SettingsGeneral: () => <p>general body</p>,
}));
vi.mock("@/windows/main/routes/SettingsHistory", () => ({
  SettingsHistory: () => <p>history body</p>,
}));
vi.mock("@/windows/main/routes/SettingsVocabulary", () => ({
  SettingsVocabulary: () => <p>vocabulary body</p>,
}));
vi.mock("@/windows/main/routes/SettingsShortcuts", () => ({
  SettingsShortcuts: () => <p>shortcuts body</p>,
}));

function renderAt(path: string) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <SettingsLayout />
    </MemoryRouter>,
  );
}

describe("SettingsLayout", () => {
  it("renders all settings nav links", () => {
    renderAt("/settings/general");
    const nav = screen.getByRole("navigation", { name: /primary/i });
    for (const label of [
      "General",
      "Shortcuts",
      "Cleanup",
      "Vocabulary",
      "Substitutions",
      "History",
      "API Keys",
      "Cost & Usage",
    ]) {
      expect(nav.textContent).toContain(label);
    }
    expect(nav.textContent).not.toContain("Home");
    // Plan 040 — the app is free; there is no License tab any more.
    expect(nav.textContent).not.toContain("License");
  });

  it("shows the api-keys pane when navigated to it", () => {
    renderAt("/settings/api-keys");
    // Every pane is mounted; the active one is the only one not
    // hidden via the `hidden` attribute. (queryAllByText because
    // React 19 in dev mounts function components twice during
    // strict-mode-like behavior — both copies sit inside the same
    // Pane wrapper.)
    expect(anyVisible(screen.queryAllByText("api-keys body"))).toBe(true);
    expect(anyVisible(screen.queryAllByText("general body"))).toBe(false);
  });

  it("redirects the index route to General", () => {
    renderAt("/");
    expect(anyVisible(screen.queryAllByText("general body"))).toBe(true);
    expect(anyVisible(screen.queryAllByText("history body"))).toBe(false);
  });

  it("keeps every pane in the DOM regardless of active route", () => {
    // Phase 10 invariant: routes stay mounted so tab switches are pure
    // CSS toggles. If this regresses we'd revert to the per-mount
    // `useSetting` invoke storm that made tray-driven nav feel laggy.
    renderAt("/settings/general");
    expect(screen.queryAllByText("general body").length).toBeGreaterThan(0);
    expect(screen.queryAllByText("history body").length).toBeGreaterThan(0);
    expect(screen.queryAllByText("api-keys body").length).toBeGreaterThan(0);
  });
});

function anyVisible(els: HTMLElement[]): boolean {
  return els.some(isVisiblePane);
}

function isVisiblePane(el: HTMLElement): boolean {
  // The Pane wrapper sets `hidden` when inactive; walk up to find it.
  let node: HTMLElement | null = el;
  while (node) {
    if (node.hasAttribute("hidden")) return false;
    node = node.parentElement;
  }
  return true;
}
