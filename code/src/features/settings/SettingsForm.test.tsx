/**
 * The download destination control.
 *
 * The setting existed before the picker did: the path was a text field and
 * nothing else, so choosing a folder meant spelling it out — including one on a
 * drive the user cannot see from the form. These tests pin the control that
 * replaced it: the picker opens in directory mode, its answer becomes the value,
 * cancel changes nothing, and a failure is shown rather than swallowed.
 *
 * Component-level (`vitest` + `jsdom`) with the dialog plugin mocked, so what is
 * verified is the screen's behaviour, not the native dialog itself — the plugin
 * belongs to Tauri and the path it returns still has to survive `save_settings`
 * and the backend's own validation.
 */

import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { SettingsForm } from "@/features/settings/SettingsForm";
import { useSettingsStore } from "@/stores/useSettingsStore";
import type { AppSettings, SettingsOptions } from "@/types";

const open = vi.fn();
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (...args: unknown[]) => open(...args),
}));

const CURRENT: AppSettings = {
  downloadDirectory: "C:\\Users\\tester\\Downloads\\Douyin",
  maxConcurrentDownloads: 2,
  filenameTemplate: "{id}",
  folderTemplate: "{creator}/{type}",
  organizeByCreator: true,
  organizeByMediaType: true,
  browserAssistedResolution: true,
  profileLoginEnabled: false,
  conflictPolicy: "rename",
  theme: "system",
  logLevel: "info",
  logToFile: true,
  retryMaxAttempts: 3,
  requestTimeoutSeconds: 30,
  language: "en",
};

const OPTIONS: SettingsOptions = {
  conflictPolicies: [],
  themes: [],
  logLevels: [],
  filenamePlaceholders: ["id", "creator", "type"],
  maxConcurrentLimit: 8,
  maxRetryAttempts: 10,
  timeoutSecondsMin: 5,
  timeoutSecondsMax: 600,
  defaultDownloadDirectory: "C:\\Users\\tester\\Downloads\\Douyin",
};

function seed() {
  useSettingsStore.setState({
    settings: CURRENT,
    options: OPTIONS,
    loading: false,
    saving: false,
    error: null,
    savedAt: null,
  });
}

/** The download directory input, found by its label. */
function directoryInput(): HTMLInputElement {
  return screen.getByLabelText("Download directory") as HTMLInputElement;
}

beforeEach(() => {
  open.mockReset();
  seed();
});

describe("the download destination picker", () => {
  it("offers a Browse control beside the path", () => {
    render(<SettingsForm />);
    expect(screen.getByRole("button", { name: "Browse…" })).toBeInTheDocument();
  });

  it("opens a directory picker, not a file picker", async () => {
    open.mockResolvedValue("D:\\Videos\\Douyin");
    render(<SettingsForm />);

    await userEvent.click(screen.getByRole("button", { name: "Browse…" }));

    expect(open).toHaveBeenCalledTimes(1);
    const args = open.mock.calls[0][0] as Record<string, unknown>;
    expect(args.directory).toBe(true);
    expect(args.multiple).toBe(false);
    // It opens where the user already is, so the dialog is not a fresh start.
    expect(args.defaultPath).toBe(CURRENT.downloadDirectory);
  });

  it("puts the chosen folder, including one on another drive, in the field", async () => {
    open.mockResolvedValue("D:\\Videos\\Douyin");
    render(<SettingsForm />);

    await userEvent.click(screen.getByRole("button", { name: "Browse…" }));

    await waitFor(() => expect(directoryInput().value).toBe("D:\\Videos\\Douyin"));
  });

  it("changes nothing when the picker is cancelled", async () => {
    open.mockResolvedValue(null);
    render(<SettingsForm />);

    await userEvent.click(screen.getByRole("button", { name: "Browse…" }));

    await waitFor(() => expect(open).toHaveBeenCalled());
    expect(directoryInput().value).toBe(CURRENT.downloadDirectory);
  });

  it("announces a picker failure instead of swallowing it", async () => {
    open.mockRejectedValue(new Error("dialog unavailable"));
    render(<SettingsForm />);

    await userEvent.click(screen.getByRole("button", { name: "Browse…" }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("The folder picker could not be opened.");
    // And the path the user had is untouched.
    expect(directoryInput().value).toBe(CURRENT.downloadDirectory);
  });

  it("does not save on the user's behalf", async () => {
    // A picked path is a draft edit like any other: it still has to be saved,
    // and the backend still validates it. Saving here would bypass both.
    const save = vi.fn().mockResolvedValue(true);
    useSettingsStore.setState({ save });
    open.mockResolvedValue("D:\\Videos\\Douyin");
    render(<SettingsForm />);

    await userEvent.click(screen.getByRole("button", { name: "Browse…" }));
    await waitFor(() => expect(directoryInput().value).toBe("D:\\Videos\\Douyin"));

    expect(save).not.toHaveBeenCalled();
    expect(screen.getByText("unsaved changes")).toBeInTheDocument();
  });
});
