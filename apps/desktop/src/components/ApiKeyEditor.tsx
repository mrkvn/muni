/**
 * Single source of truth for the per-service API-key editor used both
 * by Settings → API Keys and the onboarding wizard's API Keys step.
 *
 * Visual + behavioural contract:
 * - "Saved" / "Not set" pill next to the field label.
 * - Reveal-eye toggle inside the input.
 * - Save button validates against the live endpoint before persisting;
 *   a rejected key clears the input so the button returns to disabled
 *   and the previously-saved keychain entry stays intact (the keychain
 *   write only runs on the success branch).
 * - Pulsing dots overlay during validation (`PulsingDots` keyframes).
 * - Remove button is enabled only when a key is currently saved.
 * - Only errors surface as inline text (red, `role="alert"`); success is
 *   conveyed by the Saved/Not set pill, so no "validated and saved." line.
 *
 * Both call sites used to maintain their own copies; they drifted on
 * error styling, badge presence, and the Remove affordance. Keeping
 * the editor here forces parity.
 */
import { useEffect, useId, useState } from "react";
import { Eye, EyeOff } from "lucide-react";

import { SettingsSection } from "@/components/SettingsSection";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { PulsingDots } from "@/components/PulsingDots";
import { useMinDurationTrue } from "@/hooks/useMinDurationTrue";
import {
  type SecretService,
  deleteSecret,
  saveSecret,
  useSecretProbe,
  validateApiKey,
} from "@/hooks/useSecrets";

type Status =
  | { kind: "idle" }
  | { kind: "working"; action: "save" | "remove" }
  | { kind: "ok"; message: string }
  | { kind: "error"; message: string };

export type ApiKeyEditorProps = {
  service: SecretService;
  label: string;
  helpUrl: string;
  /**
   * Presentation only — the behaviour (validate-before-save, Remove
   * gating, status messaging) is identical across layouts:
   * - "card"    inset-legend fieldset card (onboarding wizard).
   * - "section" chrome-free V9 block, uppercase label above (Settings → API Keys).
   */
  layout?: "card" | "section";
  /**
   * Whether the surface hosting this editor is currently shown. Settings
   * panes stay mounted-but-hidden across tab switches, so transient
   * validation feedback would otherwise linger when the user navigates
   * away and back. When this flips to `false` the status is reset.
   * Omit (defaults to always-visible) for surfaces that mount/unmount.
   */
  visible?: boolean;
};

export function ApiKeyEditor({
  service,
  label,
  helpUrl,
  layout = "card",
  visible = true,
}: ApiKeyEditorProps) {
  const probe = useSecretProbe(service);
  const [value, setValue] = useState<string>("");
  const [reveal, setReveal] = useState<boolean>(false);
  const [status, setStatus] = useState<Status>({ kind: "idle" });

  // `useId` avoids input id collisions when both windows mount this
  // component for the same service (the main window keeps all settings
  // routes mounted-but-hidden while onboarding is up).
  const inputId = useId();
  const helpId = useId();

  // Transient validation feedback is per-visit: drop it when the hosting
  // pane is hidden so navigating away and back shows a clean state rather
  // than a stale "Invalid key" / "saved." line.
  useEffect(() => {
    if (!visible) setStatus({ kind: "idle" });
  }, [visible]);

  const empty = value.trim().length === 0;
  const busy = status.kind === "working";
  const saving = status.kind === "working" && status.action === "save";
  // Two flicker sources to defeat:
  //   1. Sub-100 ms validation — floor the busy state at 400 ms so the
  //      pulsing dots are actually readable.
  //   2. Hook lag — `setHeld(true)` lands in a useEffect, so we OR the
  //      live `saving` flag in synchronously to flip the indicator on
  //      the same render that disables the button.
  const heldSaving = useMinDurationTrue(saving, 400);
  const showValidating = saving || heldSaving;

  async function handleSave() {
    setStatus({ kind: "working", action: "save" });
    try {
      const ok = await validateApiKey(service, value);
      if (!ok) {
        // Clear the input on rejection so Save returns to disabled.
        // The previously-saved keychain entry (if any) is untouched —
        // `saveSecret` only runs on the success branch below.
        setValue("");
        setStatus({ kind: "error", message: "Invalid key" });
        return;
      }
      await saveSecret(service, value);
      await probe.refresh();
      setValue("");
      setStatus({ kind: "ok", message: `${label} key validated and saved.` });
    } catch (e) {
      setStatus({
        kind: "error",
        message: `Couldn't validate ${label} key: ${String(e)}`,
      });
    }
  }

  async function handleDelete() {
    setStatus({ kind: "working", action: "remove" });
    try {
      await deleteSecret(service);
      await probe.refresh();
      setStatus({ kind: "ok", message: `${label} key removed.` });
    } catch (e) {
      setStatus({ kind: "error", message: `Remove failed: ${String(e)}` });
    }
  }

  // The pieces below are shared verbatim across both layouts so the two
  // surfaces can't drift on badge text, validation indicator, reveal
  // affordance, or the disabled/Remove gating — only their arrangement
  // changes between "card" and "section".
  const statusPill = (
    <span
      className={
        "rounded-md border px-2 py-0.5 text-xs " +
        (probe.isSet
          ? "border-emerald-500/30 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
          : "border-border bg-muted/40 text-muted-foreground")
      }
    >
      {probe.loading
        ? "…"
        : probe.error
          ? "Check failed"
          : probe.isSet
            ? "Saved"
            : "Not set"}
    </span>
  );

  const keyField = (wrapperClassName?: string, ariaLabel?: string) => (
    <div className={`relative ${wrapperClassName ?? ""}`}>
      <Input
        id={inputId}
        type={reveal ? "text" : "password"}
        value={value}
        onChange={(e) => setValue(e.target.value)}
        placeholder={
          probe.isSet ? "•••• (saved — type to replace)" : "Paste your key"
        }
        autoComplete="off"
        spellCheck={false}
        aria-describedby={helpId}
        aria-label={ariaLabel}
        className="pr-10"
      />
      <button
        type="button"
        onClick={() => setReveal((prev) => !prev)}
        aria-label={reveal ? "Hide key" : "Reveal key"}
        aria-pressed={reveal}
        className="absolute inset-y-0 right-2 flex items-center justify-center rounded-md p-1 text-muted-foreground hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        {reveal ? (
          <EyeOff className="h-4 w-4" aria-hidden="true" />
        ) : (
          <Eye className="h-4 w-4" aria-hidden="true" />
        )}
      </button>
    </div>
  );

  const saveButton = (
    <Button
      onClick={() => void handleSave()}
      disabled={empty || busy || showValidating}
      className="relative"
    >
      <span className={showValidating ? "invisible" : undefined}>Save</span>
      {showValidating ? (
        <span className="absolute inset-0 flex items-center justify-center">
          <PulsingDots />
        </span>
      ) : null}
    </Button>
  );

  const removeButton = (
    <Button
      variant="ghost"
      onClick={() => void handleDelete()}
      disabled={!probe.isSet || busy}
    >
      Remove saved key
    </Button>
  );

  // Only errors render as inline text. Success state ("saved"/"removed")
  // is already conveyed by the Saved/Not set pill, so an extra "validated
  // and saved." line is redundant and reads as out of place.
  const statusMessage =
    status.kind === "error" ? (
      <span id={helpId} role="alert" className="text-destructive text-xs">
        {status.message}
      </span>
    ) : null;

  const helpLink = (
    <a
      className="text-primary hover:underline text-xs"
      href={helpUrl}
      target="_blank"
      rel="noreferrer"
    >
      Get a key →
    </a>
  );

  const groupId = `${inputId}-group`;

  // V9 (Settings → API Keys): uppercase service label above a chrome-free
  // block — status pill, then the field with Save/Remove inline, then the
  // help link (with any validation message beside it).
  if (layout === "section") {
    return (
      <SettingsSection
        title={label}
        titleId={groupId}
        className="mb-8"
        trailing={statusPill}
      >
        {/* pt-1.5 adds a little breathing room between the service-name
            heading and the controls below it. */}
        <div className="flex flex-col gap-3 pt-1.5">
          {statusMessage ? (
            <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
              {statusMessage}
            </div>
          ) : null}
          <div className="flex items-center gap-2">
            {keyField("flex-1 min-w-0", `${label} API key`)}
            {saveButton}
            {removeButton}
          </div>
          <div>{helpLink}</div>
        </div>
      </SettingsSection>
    );
  }

  // "card" (onboarding wizard): inset-legend fieldset. Rendered as
  // <div role="group"> + <span> rather than <fieldset>/<legend> because
  // WebKit's fieldset intrinsic-sizing reserves ~19 px of phantom space
  // inside the content area on initial layout (and again every time a
  // hidden ancestor becomes visible), which produced a "panel jumps on
  // first input click" symptom. The label-in-the-border visual is
  // reproduced with absolute positioning + a background swatch.
  return (
    <div
      role="group"
      aria-labelledby={groupId}
      className="relative rounded-lg border border-border p-4"
    >
      <span
        id={groupId}
        className="absolute -top-2.5 left-3 bg-background px-1 text-sm font-medium"
      >
        {label}
      </span>
      <div className="flex flex-col gap-3">
        <div className="flex items-center justify-between gap-3">
          <div className="flex items-center gap-2">
            <Label htmlFor={inputId}>API key</Label>
            {statusPill}
          </div>
          {helpLink}
        </div>
        {keyField()}
        <div className="flex flex-wrap items-center gap-2">
          {saveButton}
          {removeButton}
          {statusMessage ? <span className="ml-auto">{statusMessage}</span> : null}
        </div>
      </div>
    </div>
  );
}
