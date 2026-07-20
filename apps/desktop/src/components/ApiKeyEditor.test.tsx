// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiKeyEditor } from "./ApiKeyEditor";

// The editor's behaviour (validate-before-save, status messaging,
// reset-on-hide) is what we want to pin; the keychain/IPC layer is
// swapped for in-memory stubs so the test stays deterministic.
const validateApiKey = vi.fn<(service: string, value: string) => Promise<boolean>>();
const saveSecret = vi.fn<() => Promise<void>>();
const deleteSecret = vi.fn<() => Promise<void>>();
const refresh = vi.fn<() => Promise<void>>();

let probeIsSet = false;

vi.mock("@/hooks/useSecrets", () => ({
  useSecretProbe: () => ({ isSet: probeIsSet, loading: false, refresh }),
  saveSecret: (...args: unknown[]) => saveSecret(...(args as [])),
  deleteSecret: (...args: unknown[]) => deleteSecret(...(args as [])),
  validateApiKey: (service: string, value: string) => validateApiKey(service, value),
}));

beforeEach(() => {
  probeIsSet = false;
  validateApiKey.mockReset();
  saveSecret.mockReset().mockResolvedValue(undefined);
  deleteSecret.mockReset().mockResolvedValue(undefined);
  refresh.mockReset().mockResolvedValue(undefined);
});

afterEach(() => {
  cleanup();
});

function ApiKeyEditorAt({ visible }: { visible: boolean }) {
  return (
    <ApiKeyEditor
      service="deepgram"
      label="Deepgram"
      helpUrl="https://example.com"
      layout="section"
      visible={visible}
    />
  );
}

// Always render through the same wrapper component so `rerender` updates
// props on the existing tree instead of unmounting/remounting (a remount
// would reset state on its own and hide the bug we're pinning).
function renderEditor(visible = true) {
  return render(<ApiKeyEditorAt visible={visible} />);
}

describe("ApiKeyEditor", () => {
  it("shows 'Invalid key' when validation rejects, without persisting", async () => {
    const user = userEvent.setup();
    validateApiKey.mockResolvedValue(false);
    renderEditor();

    await user.type(screen.getByLabelText("Deepgram API key"), "bad-key");
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(await screen.findByText("Invalid key")).toBeTruthy();
    // Rejection must not touch the keychain.
    expect(saveSecret).not.toHaveBeenCalled();
  });

  it("does not render a success message on save — the pill carries that state", async () => {
    const user = userEvent.setup();
    validateApiKey.mockResolvedValue(true);
    renderEditor();

    await user.type(screen.getByLabelText("Deepgram API key"), "good-key");
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(saveSecret).toHaveBeenCalledTimes(1));
    expect(screen.queryByText(/validated and saved/i)).toBeNull();
  });

  it("clears a stale error when the pane is hidden and shown again", async () => {
    const user = userEvent.setup();
    validateApiKey.mockResolvedValue(false);
    const { rerender } = renderEditor(true);

    await user.type(screen.getByLabelText("Deepgram API key"), "bad-key");
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByText("Invalid key")).toBeTruthy();

    // Navigate away (pane hidden), then back (pane shown). Panes stay
    // mounted, so the only thing that resets the status is the visibility
    // signal — the error must be gone on return.
    rerender(<ApiKeyEditorAt visible={false} />);
    rerender(<ApiKeyEditorAt visible={true} />);

    expect(screen.queryByText("Invalid key")).toBeNull();
  });
});
