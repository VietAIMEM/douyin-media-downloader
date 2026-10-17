/**
 * The sign-in section.
 *
 * The properties worth pinning are all about what it does *not* claim: it never
 * says "logged in" because a preference is on or a directory exists, it never
 * hides a failure behind a neutral state, and it never deletes a saved session
 * without asking. The status it shows came from the browser, and these check
 * that the screen passes that answer through rather than inventing one.
 */

import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { LoginPanel } from "@/features/profile/LoginPanel";
import { useLoginStore } from "@/services/login";
import { useSettingsStore } from "@/stores/useSettingsStore";
import type { AppSettings, LoginReport } from "@/types";

const beginLogin = vi.fn();
const getLoginStatus = vi.fn();
const clearLoginSession = vi.fn();
const saveSettings = vi.fn();

vi.mock("@/services/api", () => ({
  api: {
    beginLogin: () => beginLogin(),
    getLoginStatus: () => getLoginStatus(),
    clearLoginSession: () => clearLoginSession(),
    saveSettings: (settings: AppSettings) => saveSettings(settings),
  },
}));

function report(overrides: Partial<LoginReport> = {}): LoginReport {
  return {
    available: true,
    status: "signedOut",
    profileDirectory: "C:\\Users\\tester\\AppData\\Roaming\\DouyinDownloader\\data\\browser\\profile",
    ...overrides,
  };
}

function settings(profileLoginEnabled = false): AppSettings {
  return {
    downloadDirectory: "C:\\Downloads\\Douyin",
    maxConcurrentDownloads: 3,
    filenameTemplate: "{id}",
    folderTemplate: "{creator}/{type}",
    organizeByCreator: true,
    organizeByMediaType: true,
    browserAssistedResolution: true,
    profileLoginEnabled,
    conflictPolicy: "rename",
    theme: "system",
    logLevel: "info",
    logToFile: true,
    retryMaxAttempts: 3,
    requestTimeoutSeconds: 30,
    language: "en",
  } as AppSettings;
}

beforeEach(() => {
  beginLogin.mockReset().mockResolvedValue(report());
  getLoginStatus.mockReset().mockResolvedValue(report());
  clearLoginSession.mockReset().mockResolvedValue(report());
  saveSettings.mockReset().mockResolvedValue(settings());

  useLoginStore.setState({
    phase: "unknown",
    windowOpen: false,
    requiredBySource: false,
    profileDirectory: "",
    error: null,
  });
  useSettingsStore.setState({ settings: settings(), saving: false, error: null });
});

describe("the sign-in section", () => {
  it("asks the browser rather than assuming, on mount", async () => {
    getLoginStatus.mockResolvedValue(report({ status: "signedIn" }));
    render(<LoginPanel />);

    expect(await screen.findByText("Logged in")).toBeInTheDocument();
    expect(getLoginStatus).toHaveBeenCalled();
  });

  it("says so when there is no session, rather than staying silent", async () => {
    render(<LoginPanel />);

    expect(await screen.findByText("Not logged in")).toBeInTheDocument();
  });

  it("says the status is unavailable when there is no browser to ask", async () => {
    getLoginStatus.mockResolvedValue(report({ available: false, status: null }));
    render(<LoginPanel />);

    expect(await screen.findByText("Login status unavailable")).toBeInTheDocument();
  });

  it("does not read an unclear answer as a sign-out", async () => {
    // The third state, and the reason it exists. Douyin's page answering
    // "cannot tell" — a slow load, a redirect, a page that rendered without
    // naming anyone — must not be shown as "Not logged in", which would send
    // the user to sign in again over nothing.
    getLoginStatus.mockResolvedValue(report({ status: "unknown" }));
    render(<LoginPanel />);

    expect(await screen.findByText("Login status unverified")).toBeInTheDocument();
    expect(screen.queryByText("Not logged in")).not.toBeInTheDocument();
    expect(screen.queryByText("Logged in")).not.toBeInTheDocument();
  });

  it("shows a sign-in in progress while the window is open", async () => {
    const user = userEvent.setup();
    render(<LoginPanel />);
    await screen.findByText("Not logged in");

    await user.click(screen.getByRole("button", { name: "Login" }));

    expect(await screen.findByText("Login in progress")).toBeInTheDocument();
    expect(beginLogin).toHaveBeenCalled();
  });

  it("stops calling it in progress once a check finds the session", async () => {
    const user = userEvent.setup();
    render(<LoginPanel />);
    await screen.findByText("Not logged in");

    await user.click(screen.getByRole("button", { name: "Login" }));
    await screen.findByText("Login in progress");

    getLoginStatus.mockResolvedValue(report({ status: "signedIn" }));
    await user.click(screen.getByRole("button", { name: "Check status" }));

    expect(await screen.findByText("Logged in")).toBeInTheDocument();
    expect(screen.queryByText("Login in progress")).not.toBeInTheDocument();
  });

  it("says when the source itself asked for a signed-in visitor", async () => {
    // Different evidence from the checked status: the browser may still hold a
    // cookie that Douyin has stopped accepting, and only the source can say so.
    getLoginStatus.mockResolvedValue(report({ status: "signedIn" }));
    render(<LoginPanel />);

    await waitFor(() => expect(screen.getByText("Logged in")).toBeInTheDocument());
    useLoginStore.getState().markRequiredBySource();

    expect(
      await screen.findByText(/Douyin wanted a signed-in visitor/i),
    ).toBeInTheDocument();
  });

  it("saves the preference as soon as it is changed", async () => {
    const user = userEvent.setup();
    render(<LoginPanel />);
    await screen.findByText("Not logged in");

    await user.click(screen.getByLabelText(/Use login session when loading profiles/i));

    await waitFor(() => expect(saveSettings).toHaveBeenCalled());
    expect(saveSettings.mock.calls[0][0].profileLoginEnabled).toBe(true);
  });

  it("shows the stored preference rather than a default", async () => {
    useSettingsStore.setState({ settings: settings(true) });
    render(<LoginPanel />);

    await waitFor(() =>
      expect(screen.getByLabelText(/Use login session when loading profiles/i)).toBeChecked(),
    );
  });

  it("asks before deleting a saved session", async () => {
    const user = userEvent.setup();
    getLoginStatus.mockResolvedValue(report({ status: "signedIn" }));
    render(<LoginPanel />);
    await screen.findByText("Logged in");

    await user.click(screen.getByRole("button", { name: "Clear saved session" }));

    // The question is asked, and nothing has been deleted while it is on screen.
    expect(screen.getByText(/Delete the saved sign-in\?/i)).toBeInTheDocument();
    expect(clearLoginSession).not.toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "Delete saved sign-in" }));
    await waitFor(() => expect(clearLoginSession).toHaveBeenCalledTimes(1));
  });

  it("leaves the session alone when the confirmation is refused", async () => {
    const user = userEvent.setup();
    getLoginStatus.mockResolvedValue(report({ status: "signedIn" }));
    render(<LoginPanel />);
    await screen.findByText("Logged in");

    await user.click(screen.getByRole("button", { name: "Clear saved session" }));
    await user.click(screen.getByRole("button", { name: "Cancel" }));

    expect(clearLoginSession).not.toHaveBeenCalled();
    expect(screen.queryByText(/Delete the saved sign-in\?/i)).not.toBeInTheDocument();
  });

  it("reports a sign-in that could not be started, and keeps the previous state visible", async () => {
    const user = userEvent.setup();
    beginLogin.mockRejectedValue(new Error("no browser"));
    render(<LoginPanel />);
    await screen.findByText("Not logged in");

    await user.click(screen.getByRole("button", { name: "Login" }));

    expect(await screen.findByRole("alert")).toBeInTheDocument();
    expect(screen.queryByText("Logged in")).not.toBeInTheDocument();
  });

  it("names the profile directory so it is not a mystery", async () => {
    render(<LoginPanel />);
    await screen.findByText("Not logged in");

    expect(screen.getByText(/browser\\profile/)).toBeInTheDocument();
  });
});
