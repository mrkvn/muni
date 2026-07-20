/**
 * Cleanup settings tab — user-facing tuning for how Muni cleans up
 * dictation. Two independent panes:
 *   1. **About me** — free-form vocabulary context that helps Muni
 *      recover words it mishears (proper nouns, brand names, jargon).
 *   2. **Your preferences** — free-form instructions for how the user
 *      wants their dictation cleaned up. Appended after Muni's own
 *      rules with explicit override precedence on conflict.
 *
 * Vocabulary hints live in their own sidebar tab (`SettingsVocabulary`).
 *
 * Each pane has its own dirty/save state; the panes never share status.
 */
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { ABOUT_ME_MAX_LEN, useAboutMe } from "@/hooks/useAboutMe";
import { USER_PROMPT_MAX_LEN, useUserPrompt } from "@/hooks/useUserPrompt";

export function SettingsCleanup() {
  return (
    <section aria-labelledby="cleanup-heading" className="flex flex-col gap-5">
      <header>
        <h1 id="cleanup-heading" className="text-xl font-semibold tracking-tight">
          Cleanup
        </h1>
      </header>

      <AboutMeSection />

      <hr className="border-border" />

      <UserPromptSection />
    </section>
  );
}

function AboutMeSection() {
  const { text, setText, save, status, loadError, dirty, tooLong } = useAboutMe();
  const charCount = [...text].length;

  return (
    <div className="flex flex-col gap-3">
      <header>
        <h2 className="text-[0.9375rem] font-semibold">About me</h2>
        <p className="text-muted-foreground text-sm">
          A few sentences about your world — used to catch mishears.
        </p>
      </header>

      {loadError ? (
        <p role="alert" className="text-destructive text-sm">
          {loadError}
        </p>
      ) : null}

      <Textarea
        aria-label="About me"
        className="min-h-[120px] text-sm"
        placeholder="e.g. I wear sando and build software/apps/automations all day."
        value={text}
        onChange={(event) => setText(event.target.value)}
      />

      <div className="flex items-center justify-between gap-3">
        <span
          className={tooLong ? "text-destructive text-xs" : "text-muted-foreground text-xs"}
          aria-live="polite"
        >
          {charCount} / {ABOUT_ME_MAX_LEN}
        </span>
        <div className="flex items-center gap-3">
          {status.kind === "saved" ? (
            <span className="text-muted-foreground text-xs">Saved.</span>
          ) : null}
          {status.kind === "error" ? (
            <span role="alert" className="text-destructive text-xs">
              {status.message}
            </span>
          ) : null}
          <Button
            onClick={() => void save()}
            disabled={!dirty || tooLong || status.kind === "saving"}
          >
            Save
          </Button>
        </div>
      </div>
    </div>
  );
}

function UserPromptSection() {
  const { text, setText, save, status, loadError, dirty, tooLong } = useUserPrompt();
  const charCount = [...text].length;

  return (
    <div className="flex flex-col gap-3">
      <header>
        <h2 className="text-[0.9375rem] font-semibold">Your preferences</h2>
        <p className="text-muted-foreground text-sm">
          How you want your dictation cleaned up.
        </p>
      </header>

      {loadError ? (
        <p role="alert" className="text-destructive text-sm">
          {loadError}
        </p>
      ) : null}

      <Textarea
        aria-label="Your preferences"
        className="min-h-[120px] text-sm"
        placeholder="Never use em-dashes; use commas or periods instead."
        value={text}
        onChange={(event) => setText(event.target.value)}
      />

      <div className="flex items-center justify-between gap-3">
        <span
          className={tooLong ? "text-destructive text-xs" : "text-muted-foreground text-xs"}
          aria-live="polite"
        >
          {charCount} / {USER_PROMPT_MAX_LEN}
        </span>
        <div className="flex items-center gap-3">
          {status.kind === "saved" ? (
            <span className="text-muted-foreground text-xs">Saved.</span>
          ) : null}
          {status.kind === "error" ? (
            <span role="alert" className="text-destructive text-xs">
              {status.message}
            </span>
          ) : null}
          <Button
            onClick={() => void save()}
            disabled={!dirty || tooLong || status.kind === "saving"}
          >
            Save
          </Button>
        </div>
      </div>
    </div>
  );
}
