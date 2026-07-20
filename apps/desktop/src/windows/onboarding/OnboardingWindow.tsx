/**
 * Phase 9 — Onboarding window entry. The first-run wizard lives in
 * `OnboardingWizard.tsx`; this thin shell only mounts it so the
 * `entries/app.tsx` dispatch (which picks the component by window
 * label) keeps the same import name.
 */
import { OnboardingWizard } from "@/windows/onboarding/OnboardingWizard";

export function OnboardingWindow() {
  return <OnboardingWizard />;
}
