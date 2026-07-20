/**
 * Phase 8 — API keys settings tab. Mirrors Swift v1's `APIKeysTab`.
 *
 * The actual editor (per-service fieldset with reveal eye, validate-
 * before-save, Remove, and the pulsing-dots indicator) lives in
 * `components/ApiKeyEditor` and is reused by the onboarding wizard so
 * the two surfaces can't drift visually or behaviourally.
 */
import { ApiKeyEditor, type ApiKeyEditorProps } from "@/components/ApiKeyEditor";

const SERVICES: ApiKeyEditorProps[] = [
  {
    service: "deepgram",
    label: "Deepgram",
    helpUrl: "https://console.deepgram.com/",
  },
  {
    service: "groq",
    label: "Groq",
    helpUrl: "https://console.groq.com/keys",
  },
  {
    service: "gladia",
    label: "Gladia",
    helpUrl: "https://app.gladia.io/apikeys",
  },
];

export function SettingsApiKeys({ visible = true }: { visible?: boolean }) {
  return (
    <section aria-labelledby="api-keys-heading" className="flex flex-col">
      <header className="mb-5">
        <h1 id="api-keys-heading" className="text-xl font-semibold tracking-tight">
          API Keys
        </h1>
        <p className="text-muted-foreground text-sm">Stored in the macOS Keychain.</p>
      </header>

      {/*
        V9 layout: each service is a bold proper-case heading above its
        key controls, separated by vertical margin. Editors must stay
        direct siblings of this container so the SettingsSection
        `last:mb-0` rule drops the trailing gap on the final service.
      */}
      <div>
        {SERVICES.map((meta) => (
          <ApiKeyEditor key={meta.service} {...meta} layout="section" visible={visible} />
        ))}
      </div>
    </section>
  );
}
