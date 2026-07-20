/**
 * Phase 8 — Sidebar layout for the Main window.
 *
 * Translates Swift v1's `NSTabViewController` (six top-aligned tabs)
 * into a vertical `<nav>` of `<NavLink>`s on the left + a scrollable
 * content area on the right. The Swift `tabItem` icons map onto
 * `lucide-react` equivalents.
 *
 * Phase 10 polish — every route is rendered up-front and hidden via
 * `display: none` rather than swapped through `<Outlet />`. Reason:
 * routes like `SettingsGeneral` mount several `useSetting` hooks that
 * each fire an `invoke('settings_get')`; remounting on every tray
 * click added a perceptible "previous tab visible for a frame, then
 * jumps to the new tab" lag when the tray brought the window forward.
 * Keeping all panes mounted means tab switches are pure CSS toggles,
 * and the Rust→JS event → React render race that flushSync couldn't
 * fully eliminate disappears entirely.
 */
import {
  BookA,
  DollarSign,
  History,
  Key,
  Keyboard,
  Lightbulb,
  Settings,
  Sparkles,
} from "lucide-react";
import type { ReactNode } from "react";
import { Navigate, NavLink, useLocation } from "react-router-dom";

import { cn } from "@/lib/utils";
import { MuniLogo } from "@/components/MuniLogo";
import { SettingsApiKeys } from "@/windows/main/routes/SettingsApiKeys";
import { SettingsCleanup } from "@/windows/main/routes/SettingsCleanup";
import { SettingsCostUsage } from "@/windows/main/routes/SettingsCostUsage";
import { SettingsGeneral } from "@/windows/main/routes/SettingsGeneral";
import { SettingsHistory } from "@/windows/main/routes/SettingsHistory";
import { SettingsMyWords } from "@/windows/main/routes/SettingsMyWords";
import { SettingsShortcuts } from "@/windows/main/routes/SettingsShortcuts";
import { SettingsVocabulary } from "@/windows/main/routes/SettingsVocabulary";

type NavItem = {
  to: string;
  label: string;
  icon: React.ComponentType<{ className?: string }>;
  end?: boolean;
};

const NAV: NavItem[] = [
  { to: "/settings/general", label: "General", icon: Settings },
  { to: "/settings/hotkey", label: "Shortcuts", icon: Keyboard },
  { to: "/settings/cleanup", label: "Cleanup", icon: Sparkles },
  { to: "/settings/vocabulary", label: "Vocabulary", icon: Lightbulb },
  { to: "/settings/my-words", label: "Substitutions", icon: BookA },
  { to: "/settings/history", label: "History", icon: History },
  { to: "/settings/api-keys", label: "API Keys", icon: Key },
  { to: "/settings/cost-usage", label: "Cost & Usage", icon: DollarSign },
];

export function SettingsLayout() {
  const { pathname } = useLocation();

  if (pathname === "/") {
    return <Navigate to="/settings/general" replace />;
  }

  return (
    <div className="flex h-full w-full overflow-hidden bg-background text-foreground">
      <aside
        aria-label="Settings navigation"
        className="flex w-56 shrink-0 flex-col gap-1 border-r border-border bg-card/40 p-3"
      >
        <div className="flex items-center gap-2 px-2 pt-1 pb-4">
          <MuniLogo className="h-5 w-5" />
          <h2 className="text-base font-semibold tracking-tight">Muni</h2>
        </div>
        <nav aria-label="Primary">
          <ul className="flex flex-col gap-0.5">
            {NAV.map((item) => (
              <li key={item.to}>
                <NavLink
                  to={item.to}
                  end={item.end}
                  className={({ isActive }) =>
                    cn(
                      "flex items-center gap-2 rounded-md px-2 py-1.5 text-sm transition-colors",
                      "hover:bg-accent hover:text-accent-foreground",
                      "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
                      isActive
                        ? "bg-accent text-accent-foreground"
                        : "text-muted-foreground",
                    )
                  }
                >
                  <item.icon className="h-4 w-4" aria-hidden="true" />
                  {item.label}
                </NavLink>
              </li>
            ))}
          </ul>
        </nav>
      </aside>
      <main className="flex-1 overflow-y-auto">
        <div className="mx-auto flex max-w-3xl flex-col gap-6 px-8 py-8">
          <Pane visible={pathname === "/settings/general"}>
            <SettingsGeneral />
          </Pane>
          <Pane visible={pathname === "/settings/hotkey"}>
            <SettingsShortcuts visible={pathname === "/settings/hotkey"} />
          </Pane>
          <Pane visible={pathname === "/settings/cleanup"}>
            <SettingsCleanup />
          </Pane>
          <Pane visible={pathname === "/settings/vocabulary"}>
            <SettingsVocabulary />
          </Pane>
          <Pane visible={pathname === "/settings/my-words"}>
            <SettingsMyWords />
          </Pane>
          <Pane visible={pathname === "/settings/history"}>
            <SettingsHistory />
          </Pane>
          <Pane visible={pathname === "/settings/api-keys"}>
            <SettingsApiKeys visible={pathname === "/settings/api-keys"} />
          </Pane>
          <Pane visible={pathname === "/settings/cost-usage"}>
            <SettingsCostUsage />
          </Pane>
        </div>
      </main>
    </div>
  );
}

function Pane({
  visible,
  children,
}: {
  visible: boolean;
  children: ReactNode;
}) {
  // `hidden` plus `display: none` keeps the subtree mounted (so its
  // hooks stay live + state persists) while pulling it out of the
  // accessibility tree and the layout flow when inactive.
  return <div hidden={!visible}>{children}</div>;
}
