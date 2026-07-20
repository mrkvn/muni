/**
 * Feature 033 — first-run telemetry consent (brainstorm 010 Q2).
 *
 * Both toggles default ON and are presented pre-checked: this is the
 * honest informed opt-out, not a silent default. The two settings are
 * separable so a user can keep crash reporting while declining usage
 * analytics, or vice versa. Flipping either writes through `useSetting`
 * immediately (optimistic Tauri Store round-trip), so the choice is
 * persisted whether or not the user touches the toggle.
 *
 * Each toggle's own sub-label states concretely what it sends; the
 * privacy line (operational metadata only, never transcripts or audio)
 * is carried there rather than in a long preamble.
 */
import { ShieldCheck } from "lucide-react";

import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { useSetting } from "@/hooks/useSettings";

export function TelemetryConsentStep() {
  const crashReporting = useSetting("telemetry.crash_reporting");
  const analytics = useSetting("telemetry.analytics");

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center gap-3">
        <span className="flex size-9 items-center justify-center rounded-md bg-muted">
          <ShieldCheck className="size-4" aria-hidden="true" />
        </span>
        <p className="text-base font-medium">Help improve Muni</p>
      </div>

      <p className="text-muted-foreground text-sm leading-relaxed">
        You can change these any time in Settings → General.
      </p>

      <div className="flex flex-col gap-1 rounded-md border border-border">
        <div className="flex items-center justify-between gap-4 px-4 py-3">
          <div className="flex flex-col">
            <Label
              htmlFor="onboarding-telemetry-crash-reporting"
              className="text-sm font-medium"
            >
              Send crash reports
            </Label>
            <p className="text-muted-foreground text-xs">
              Scrubbed crash details so we can catch breakage.
            </p>
          </div>
          <Switch
            id="onboarding-telemetry-crash-reporting"
            checked={crashReporting.value}
            onCheckedChange={(next) => void crashReporting.set(next)}
            disabled={crashReporting.loading}
            aria-label="Send crash reports"
          />
        </div>
        <div className="border-t border-border" />
        <div className="flex items-center justify-between gap-4 px-4 py-3">
          <div className="flex flex-col">
            <Label
              htmlFor="onboarding-telemetry-analytics"
              className="text-sm font-medium"
            >
              Send usage analytics
            </Label>
            <p className="text-muted-foreground text-xs">
              Anonymous performance &amp; feature-usage counts.
            </p>
          </div>
          <Switch
            id="onboarding-telemetry-analytics"
            checked={analytics.value}
            onCheckedChange={(next) => void analytics.set(next)}
            disabled={analytics.loading}
            aria-label="Send usage analytics"
          />
        </div>
      </div>
    </div>
  );
}
