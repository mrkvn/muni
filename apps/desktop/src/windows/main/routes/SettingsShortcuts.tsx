/**
 * Feature 037 — the Shortcuts settings route (`/settings/hotkey`).
 *
 * Two rebindable hotkeys: the main **dictation** trigger (a ≥2-modifier chord)
 * and the **re-paste** hotkey (a modifier+key combo, clearable to disable).
 * Both persist through the typed Rust commands
 * (`set_dictation_hotkey` / `set_repaste_hotkey`) — Rust is the validation
 * source of truth, so a rejected binding surfaces its message as a toast and
 * the optimistic value reverts. Current values are read via `useSetting`, which
 * live-updates from the `settings://changed` the commands emit.
 *
 * This is also the deep-link target for hotkey-install errors
 * (`useErrorEvents` routes `SettingsTab::Hotkey` here).
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";

import { HotkeyRecorder } from "@/components/HotkeyRecorder";
import { SettingsSection } from "@/components/SettingsSection";
import {
  type DictationBinding,
  type RepasteBinding,
  useSetting,
} from "@/hooks/useSettings";

/**
 * Plan 039 task 51(a) — the backend emits this when a keyed shortcut fails to
 * register with the OS (combo already held by the system or another app).
 *
 * The backend also *heals* the persisted binding to a registrable fallback
 * (dictation → its modifier-only default; re-paste → disabled) and emits
 * `settings://changed`, so the displayed value updates itself to match reality
 * via `useSetting` — this hook's only job is to tell the user *why* it changed
 * so they can pick a different combo. Payload mirrors
 * `EVENT_HOTKEY_REGISTRATION_FAILED`.
 */
const EVENT_REGISTRATION_FAILED = "hotkey://registration-failed";

interface RegistrationFailedPayload {
  target: "dictation" | "repaste";
  accel: string;
}

/**
 * Toast a failed OS shortcut registration so it reaches the UI (the mechanism
 * conflict errors already use). The registration happens asynchronously after
 * the persisting command returned, so this is the only surface for it.
 */
export function useRegistrationFailureToast() {
  useEffect(() => {
    let cancelled = false;
    let off: UnlistenFn | undefined;
    void (async () => {
      const unlisten = await listen<RegistrationFailedPayload>(
        EVENT_REGISTRATION_FAILED,
        (event) => {
          const which =
            event.payload.target === "dictation" ? "dictation" : "re-paste";
          toast.error(
            `Couldn't register the ${which} shortcut — it may be in use by the system or another app. Try a different combo.`,
          );
        },
      );
      if (cancelled) {
        unlisten();
        return;
      }
      off = unlisten;
    })();
    return () => {
      cancelled = true;
      off?.();
    };
  }, []);
}

export function SettingsShortcuts({ visible = true }: { visible?: boolean } = {}) {
  const dictation = useDictationHotkey();
  const repaste = useRepasteHotkey();
  useRegistrationFailureToast();

  return (
    <section aria-labelledby="shortcuts-heading" className="flex flex-col">
      <header className="mb-5">
        <h1
          id="shortcuts-heading"
          className="text-xl font-semibold tracking-tight"
        >
          Shortcuts
        </h1>
      </header>

      <div>
        <SettingsSection title="Dictation" titleId="dictation-shortcut-group">
          <div className="flex items-start justify-between gap-3 py-2.5">
            <div className="flex flex-1 flex-col gap-0.5">
              <span className="text-sm">Dictation hotkey</span>
              <span className="text-xs text-muted-foreground">
                Hold to talk or quick-tap to toggle. Use two modifiers, or a
                modifier and a key. The fn/globe key can&rsquo;t be recorded
                here.
              </span>
            </div>
            <div className="flex min-w-0 flex-1 flex-col">
              <HotkeyRecorder
                mode="modifiers"
                visible={visible}
                value={dictation.value}
                onChange={(binding) => void dictation.save(binding)}
              />
            </div>
          </div>
        </SettingsSection>

        <SettingsSection
          title="Re-paste last dictation"
          titleId="repaste-shortcut-group"
        >
          <div className="flex items-start justify-between gap-3 py-2.5">
            <div className="flex flex-1 flex-col gap-0.5">
              <span className="text-sm">Re-paste hotkey</span>
              <span className="text-xs text-muted-foreground">
                Inserts your most recent dictation wherever the cursor is. Clear
                it to disable. While set, it takes over this combo system-wide.
              </span>
            </div>
            <div className="flex min-w-0 flex-1 flex-col">
              <HotkeyRecorder
                mode="combo"
                visible={visible}
                value={repaste.value}
                onChange={(binding) => void repaste.save(binding)}
              />
            </div>
          </div>
        </SettingsSection>
      </div>
    </section>
  );
}

/**
 * Reads the dictation binding via `useSetting` and persists changes through
 * `set_dictation_hotkey`. Optimistic: the pending value shows immediately; on
 * a Rust rejection it reverts (the command never persisted) and toasts the
 * validation message. Mirrors the `useLaunchAtLogin` pattern.
 */
function useDictationHotkey(): {
  value: DictationBinding;
  save: (binding: DictationBinding) => Promise<void>;
} {
  const setting = useSetting("hotkey.dictation_binding");
  const [pending, setPending] = useState<DictationBinding | null>(null);

  const save = useCallback(async (binding: DictationBinding) => {
    setPending(binding);
    try {
      await invoke("set_dictation_hotkey", { binding });
    } catch (error) {
      toast.error(String(error));
    } finally {
      setPending(null);
    }
  }, []);

  return { value: pending ?? setting.value, save };
}

/**
 * Reads the re-paste binding (or `null` when disabled) and persists via
 * `set_repaste_hotkey`. Same optimistic-with-revert contract as
 * `useDictationHotkey`; passing `null` clears (disables) the shortcut.
 */
function useRepasteHotkey(): {
  value: RepasteBinding | null;
  save: (binding: RepasteBinding | null) => Promise<void>;
} {
  const setting = useSetting("hotkey.repaste_binding");
  const [pending, setPending] = useState<{
    binding: RepasteBinding | null;
  } | null>(null);

  const save = useCallback(async (binding: RepasteBinding | null) => {
    setPending({ binding });
    try {
      await invoke("set_repaste_hotkey", { binding });
    } catch (error) {
      toast.error(String(error));
    } finally {
      setPending(null);
    }
  }, []);

  // `pending` is wrapped so an optimistic `null` (cleared) is distinguishable
  // from "no pending write" — both are falsy otherwise.
  return { value: pending ? pending.binding : setting.value, save };
}
