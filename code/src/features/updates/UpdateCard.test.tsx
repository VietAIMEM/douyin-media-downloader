/**
 * The update control.
 *
 * These are the properties that matter for an in-app updater, and they are all
 * about what it does *not* do on its own: it does not install anything without
 * the user confirming, it does not claim a failed download succeeded, and it
 * does not present a state it is not in. Whether the downloaded package is
 * genuinely signed is not a question this level can answer — the plugin verifies
 * against a key compiled into the build — so what is checked here is that the
 * application hands the decision to the user and reports the result honestly.
 *
 * Component-level, with the Tauri plugins mocked.
 */

import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { UpdateCard } from "@/features/updates/UpdateCard";
import { useUpdateStore } from "@/services/updater";
import { useBackendStore } from "@/stores/useBackendStore";
import type { AppInfo } from "@/types";

const check = vi.fn();
vi.mock("@tauri-apps/plugin-updater", () => ({
  check: (...args: unknown[]) => check(...args),
}));

const relaunch = vi.fn();
vi.mock("@tauri-apps/plugin-process", () => ({
  relaunch: (...args: unknown[]) => relaunch(...args),
}));

/** A stand-in for the handle `check` returns when an update exists. */
function fakeUpdate(version: string, notes = "") {
  return {
    version,
    body: notes,
    date: "2026-10-12T00:00:00Z",
    download: vi.fn(async (onEvent?: (event: unknown) => void) => {
      onEvent?.({ event: "Started", data: { contentLength: 2048 } });
      onEvent?.({ event: "Progress", data: { chunkLength: 2048 } });
      onEvent?.({ event: "Finished" });
    }),
    install: vi.fn(async () => {}),
    close: vi.fn(async () => {}),
  };
}

/**
 * A build that carries an update channel, which is what the update flow needs.
 * `appInfo(version, false)` is a build without one.
 */
function appInfo(version: string, updateChannelConfigured = true): AppInfo {
  return {
    name: "Douyin Downloader",
    version,
    identifier: "com.douyindownloader.desktop",
    os: "windows",
    architecture: "x86_64",
    dataDirectory: "C:\\data",
    databasePath: "C:\\data\\douyin-downloader.db",
    logDirectory: "C:\\data\\logs",
    schemaVersion: 5,
    logLevel: "info",
    demoMode: false,
    updateChannelConfigured,
    supportedPlatforms: ["douyin"],
    resolvers: [],
    startedAt: "2026-10-12T00:00:00Z",
  };
}

beforeEach(() => {
  check.mockReset();
  relaunch.mockReset();
  useUpdateStore.getState().dismiss();
  useBackendStore.setState({ appInfo: appInfo("1.0.0"), databaseStatus: null });
});

describe("the update control", () => {
  it("shows the running version and asks before doing anything", () => {
    render(<UpdateCard />);

    expect(screen.getByText("1.0.0")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Check for updates" })).toBeInTheDocument();
    // Nothing has been checked, so nothing is claimed.
    expect(check).not.toHaveBeenCalled();
  });

  it("says a build with no update channel never checks, and offers no check", () => {
    // The personal-use build: no feed is compiled in, so there is nothing to
    // ask. Offering the button would make a failure the only reachable
    // outcome, which reads as a broken updater rather than as a build that was
    // never meant to update itself.
    useBackendStore.setState({ appInfo: appInfo("1.0.0", false), databaseStatus: null });
    render(<UpdateCard />);

    expect(screen.getByText("1.0.0")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Check for updates" })).not.toBeInTheDocument();
    expect(screen.getByText(/no update channel/i)).toBeInTheDocument();
    expect(screen.getByText(/installer over this one/i)).toBeInTheDocument();
    expect(check).not.toHaveBeenCalled();
  });

  it("reports being up to date when the channel announces nothing", async () => {
    check.mockResolvedValue(null);
    render(<UpdateCard />);

    await userEvent.click(screen.getByRole("button", { name: "Check for updates" }));

    expect(await screen.findByText(/is the newest available/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Install update" })).not.toBeInTheDocument();
  });

  it("offers the new version and its release notes", async () => {
    check.mockResolvedValue(fakeUpdate("1.1.0", "Adds profile pagination."));
    render(<UpdateCard />);

    await userEvent.click(screen.getByRole("button", { name: "Check for updates" }));

    expect(await screen.findByText(/1\.1\.0/)).toBeInTheDocument();
    expect(screen.getByText("Adds profile pagination.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Install update" })).toBeInTheDocument();
  });

  it("does not install until the user confirms", async () => {
    const update = fakeUpdate("1.1.0");
    check.mockResolvedValue(update);
    render(<UpdateCard />);

    await userEvent.click(screen.getByRole("button", { name: "Check for updates" }));
    await screen.findByRole("button", { name: "Install update" });

    // The first click only asks the question.
    await userEvent.click(screen.getByRole("button", { name: "Install update" }));
    expect(screen.getByText(/Install version 1\.1\.0 and restart/)).toBeInTheDocument();
    expect(update.download).not.toHaveBeenCalled();
    expect(update.install).not.toHaveBeenCalled();

    // Escape backs out, and still nothing has happened.
    await userEvent.keyboard("{Escape}");
    await waitFor(() =>
      expect(screen.queryByText(/Install version 1\.1\.0 and restart/)).not.toBeInTheDocument(),
    );
    expect(update.download).not.toHaveBeenCalled();
  });

  it("downloads and installs only after the confirmation is answered", async () => {
    const update = fakeUpdate("1.1.0");
    check.mockResolvedValue(update);
    render(<UpdateCard />);

    await userEvent.click(screen.getByRole("button", { name: "Check for updates" }));
    await screen.findByRole("button", { name: "Install update" });
    await userEvent.click(screen.getByRole("button", { name: "Install update" }));
    await userEvent.click(screen.getByRole("button", { name: "Install and restart" }));

    await waitFor(() => expect(update.install).toHaveBeenCalledTimes(1));
    expect(update.download).toHaveBeenCalledTimes(1);
    // Windows hands the installer the package and exits, so the application asks
    // for the restart itself rather than assuming one.
    expect(await screen.findByRole("button", { name: "Restart now" })).toBeInTheDocument();
  });

  it("reports a failed check as a failure, not as up to date", async () => {
    check.mockRejectedValue(new Error("updates are not configured"));
    render(<UpdateCard />);

    await userEvent.click(screen.getByRole("button", { name: "Check for updates" }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("The update check failed");
    // The plugin's own words, so the cause is diagnosable.
    expect(alert).toHaveTextContent("updates are not configured");
    expect(screen.queryByText(/is the newest available/)).not.toBeInTheDocument();
  });

  it("does not claim an installation that failed to download", async () => {
    const update = fakeUpdate("1.1.0");
    update.download.mockRejectedValue(new Error("the signature did not verify"));
    check.mockResolvedValue(update);
    render(<UpdateCard />);

    await userEvent.click(screen.getByRole("button", { name: "Check for updates" }));
    await screen.findByRole("button", { name: "Install update" });
    await userEvent.click(screen.getByRole("button", { name: "Install update" }));
    await userEvent.click(screen.getByRole("button", { name: "Install and restart" }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("The update could not be downloaded");
    expect(alert).toHaveTextContent("the signature did not verify");
    expect(update.install).not.toHaveBeenCalled();
    expect(screen.queryByText("Installed")).not.toBeInTheDocument();
  });

  it("leaves everything alone when the user declines", async () => {
    const update = fakeUpdate("1.1.0");
    check.mockResolvedValue(update);
    render(<UpdateCard />);

    await userEvent.click(screen.getByRole("button", { name: "Check for updates" }));
    await screen.findByRole("button", { name: "Install update" });
    await userEvent.click(screen.getByRole("button", { name: "Not now" }));

    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Check for updates" })).toBeInTheDocument(),
    );
    expect(update.download).not.toHaveBeenCalled();
    expect(update.install).not.toHaveBeenCalled();
  });
});
