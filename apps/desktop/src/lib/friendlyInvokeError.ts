/**
 * Plan 039 task 38 — turn a rejected Tauri `invoke` error into user-facing copy.
 *
 * The load-bearing case is a *degraded boot*: when Muni comes up in safe mode
 * (or a managed state otherwise failed to initialise), commands that reach for
 * a `State<'_, T>` reject with Tauri's internal string
 * `"state not managed for field ..."`. Surfacing that raw string to the user is
 * meaningless, so we map it to a plain-language restart nudge. Error shapes we
 * can't reduce to a meaningful message (opaque objects that would otherwise
 * stringify to `"[object Object]"`) are treated the same way — an unreadable
 * failure is, from the user's seat, exactly a "something's wrong, restart" case.
 *
 * Anything with a readable message (a plain string, or a `MuniError`
 * `userMessage`) is passed through unchanged.
 */

/** Copy shown when a command fails because boot left the app in a degraded state. */
export const DEGRADED_BOOT_MESSAGE =
  "Muni started in a degraded state — restart the app; if this persists, report it.";

/**
 * Extract a human-readable message from an unknown thrown/rejected value, or
 * `null` when the value is opaque (e.g. a bare object with no `userMessage` /
 * `message`, which `String()` would render as `"[object Object]"`).
 */
function extractMessage(e: unknown): string | null {
  if (typeof e === "string") return e.length > 0 ? e : null;
  if (e === null || e === undefined) return null;
  if (typeof e === "object") {
    const userMessage = (e as { userMessage?: unknown }).userMessage;
    if (typeof userMessage === "string" && userMessage.length > 0) {
      return userMessage;
    }
    const message = (e as { message?: unknown }).message;
    if (typeof message === "string" && message.length > 0) {
      return message;
    }
    // Opaque object shape — no meaningful message to surface.
    return null;
  }
  // Primitives (number, boolean, bigint, symbol) — stringify for display.
  return String(e);
}

/**
 * Map a rejected `invoke` error to friendly copy. The Tauri "state not managed"
 * signature (degraded / safe-mode boot) and any unreadable shape become
 * {@link DEGRADED_BOOT_MESSAGE}; anything with a readable message is returned
 * as-is.
 */
export function friendlyInvokeError(e: unknown): string {
  const message = extractMessage(e);
  if (message === null || /state not managed/i.test(message)) {
    return DEGRADED_BOOT_MESSAGE;
  }
  return message;
}
