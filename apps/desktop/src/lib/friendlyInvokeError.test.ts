import { describe, expect, it } from "vitest";

import {
  DEGRADED_BOOT_MESSAGE,
  friendlyInvokeError,
} from "./friendlyInvokeError";

describe("friendlyInvokeError", () => {
  it("maps the Tauri 'state not managed' string to the degraded-boot copy", () => {
    // The exact shape Tauri rejects with when a command reaches for a managed
    // state that a degraded/safe-mode boot never set up.
    const raw = "state not managed for field `history` on command `history_list`";
    expect(friendlyInvokeError(raw)).toBe(DEGRADED_BOOT_MESSAGE);
  });

  it("matches the degraded signature case-insensitively and inside objects", () => {
    expect(friendlyInvokeError("State Not Managed for field `x`")).toBe(
      DEGRADED_BOOT_MESSAGE,
    );
    expect(
      friendlyInvokeError({ message: "state not managed for field `y`" }),
    ).toBe(DEGRADED_BOOT_MESSAGE);
  });

  it("passes other errors through as a readable string", () => {
    expect(friendlyInvokeError("disk full")).toBe("disk full");
  });

  it("prefers a MuniError userMessage over a bare stringification", () => {
    expect(
      friendlyInvokeError({ kind: "groqServerError", userMessage: "Groq is down." }),
    ).toBe("Groq is down.");
  });

  it("never leaks [object Object] for unknown shapes", () => {
    expect(friendlyInvokeError({ weird: true })).not.toContain("[object Object]");
  });
});
