/**
 * The saved Douyin sign-in, as the Profile Downloader sees it.
 *
 * Three things this deliberately does not do.
 *
 * It does not remember a sign-in. Every answer here came from asking the
 * browser's own profile a moment ago, because a remembered "signed in" outlives
 * the session it described and would have the screen promise something the
 * browser can no longer honour.
 *
 * It does not hold a credential. The status is two words; what the browser put
 * in its cookie jar stays in the browser.
 *
 * It does not open anything on its own. A window the user has to type into
 * appears when they ask for it, and not before.
 */

import { create } from "zustand";

import { api } from "@/services/api";
import { normalizeError, type BackendError } from "@/services/ipc";
import type { LoginReport } from "@/types";

/** Where the sign-in stands. */
export type LoginPhase =
  | "unknown"
  | "checking"
  | "signedIn"
  | "signedOut"
  /**
   * The browser answered, and the answer was "cannot tell".
   *
   * Kept apart from `signedOut`, which is a statement. This is the absence of
   * one — a page that had not finished, a redirect, a load that timed out — and
   * showing it as a sign-out would send the user to sign in again over nothing.
   */
  | "unverified"
  /** This build has no browser to sign in with. */
  | "unavailable"
  | "error";

interface LoginState {
  phase: LoginPhase;
  /** True from the moment the sign-in window opens until it finds a session. */
  windowOpen: boolean;
  /** The source itself asked for a signed-in visitor. */
  requiredBySource: boolean;
  /** The application's own profile directory, for display. */
  profileDirectory: string;
  error: BackendError | null;

  /** Ask the profile what it is holding. */
  refresh: () => Promise<void>;
  /** Open the window the user signs in with. */
  begin: () => Promise<void>;
  /** Close the browser and delete the saved profile. */
  clear: () => Promise<void>;
  /**
   * Note that a profile load was refused for want of a sign-in.
   *
   * Separate from the checked status because they are different evidence: the
   * browser may still hold a session cookie that Douyin has stopped accepting,
   * and only the source can say so.
   */
  markRequiredBySource: () => void;
  /** Forget what the last failure said, without forgetting the session. */
  dismiss: () => void;
}

/** The phase a report describes. */
function phaseOf(report: LoginReport): LoginPhase {
  if (!report.available) return "unavailable";
  if (report.status === "signedIn") return "signedIn";
  if (report.status === "signedOut") return "signedOut";
  // The browser was there and answered "cannot tell". Not a sign-out, and not
  // the silent assumption of one.
  if (report.status === "unknown") return "unverified";
  // Available but unanswered at all: the browser could not be asked.
  return "error";
}

export const useLoginStore = create<LoginState>()((set, get) => ({
  phase: "unknown",
  windowOpen: false,
  requiredBySource: false,
  profileDirectory: "",
  error: null,

  refresh: async () => {
    if (get().phase === "checking") return;
    set({ phase: "checking", error: null });
    try {
      const report = await api.getLoginStatus();
      const phase = phaseOf(report);
      set({
        phase,
        profileDirectory: report.profileDirectory,
        // A check that finds a session is what ends "signing in": the user has
        // done their part, and the window no longer needs to be kept open on
        // their behalf.
        windowOpen: phase === "signedIn" ? false : get().windowOpen,
        requiredBySource: phase === "signedIn" ? false : get().requiredBySource,
        error: phase === "error" ? failedCheck() : null,
      });
    } catch (raw) {
      set({ phase: "error", error: normalizeError(raw) });
    }
  },

  begin: async () => {
    set({ error: null, requiredBySource: false });
    try {
      const report = await api.beginLogin();
      set({
        phase: phaseOf(report),
        windowOpen: true,
        profileDirectory: report.profileDirectory,
      });
    } catch (raw) {
      set({ phase: "error", windowOpen: false, error: normalizeError(raw) });
    }
  },

  clear: async () => {
    set({ error: null });
    try {
      const report = await api.clearLoginSession();
      set({
        phase: phaseOf(report),
        windowOpen: false,
        requiredBySource: false,
        profileDirectory: report.profileDirectory,
      });
    } catch (raw) {
      // Left as it was: a sign-out that did not happen must not look like one.
      set({ error: normalizeError(raw) });
    }
  },

  markRequiredBySource: () => set({ requiredBySource: true }),

  dismiss: () => set({ requiredBySource: false, error: null }),
}));

/** The failure that is not a thrown error: the browser could not be asked. */
function failedCheck(): BackendError {
  return normalizeError({
    kind: "browser_unavailable",
    code: "login_status_unavailable",
    message:
      "The saved sign-in could not be checked, because the browser could not be started.",
  });
}
