/**
 * Chrome-free settings section ("V9" layout): a bold proper-case heading
 * sits close above a stack of rows, with hairline dividers *between* rows
 * and no card border or fill. Sections are separated by vertical margin
 * alone. This replaced the inset-legend fieldset cards on the General
 * and API Keys tabs.
 *
 * Spacing follows a proximity scale so sections read as groups at a
 * glance: heading hugs its rows (~16px), sibling rows sit ~20px apart,
 * and sections are ~38px apart — each gap clearly larger than the one
 * nested inside it.
 *
 * Row dividers come from `divide-y` on the content wrapper, so the rule
 * lands between consecutive rows (never above the first or below the
 * last). Callers therefore pass plain row elements as *direct* children
 * — each row owns its own vertical padding (`py-2.5`), the divider is
 * automatic.
 *
 * Sections must be direct siblings inside one container so `last:mb-0`
 * drops the trailing gap on the final section.
 *
 * Rendered as `<section role="group">` + `<h2>` (rather than
 * `<fieldset>`/`<legend>`) for the same WebKit reason documented in the
 * old cards: fieldset intrinsic sizing reserved phantom space when a
 * hidden ancestor was revealed (tab switch), which made panels jump.
 */
import type { ReactNode } from "react";

import { cn } from "@/lib/utils";

export type SettingsSectionProps = {
  /** Section heading shown above the rows (bold, proper case). */
  title: string;
  /** Unique id wired to `aria-labelledby` so the group is named for AT. */
  titleId: string;
  children: ReactNode;
  /** Extra classes on the section root (e.g. a wider bottom margin). */
  className?: string;
  /**
   * Optional element rendered on the heading row, right-aligned next to the
   * title (e.g. a "Saved" / "Active" status pill). Keeps the at-a-glance
   * status on the same line as the thing it describes.
   */
  trailing?: ReactNode;
};

export function SettingsSection({
  title,
  titleId,
  children,
  className,
  trailing,
}: SettingsSectionProps) {
  return (
    <section
      role="group"
      aria-labelledby={titleId}
      className={cn("mb-7 last:mb-0", className)}
    >
      <div className="mb-1.5 flex items-center gap-2.5">
        <h2 id={titleId} className="text-[0.9375rem] font-semibold text-foreground">
          {title}
        </h2>
        {trailing}
      </div>
      <div className="divide-y divide-border">{children}</div>
    </section>
  );
}
