/**
 * Application updates.
 *
 * The updater plugin does the parts that must not be reimplemented: it fetches
 * the release manifest for the configured channel, decides whether the
 * announced version is newer than the running one, downloads the package, and
 * **verifies its signature** against the public key compiled into this build. A
 * package that is not signed by the matching private key never reaches this
 * code — the download fails and the file is discarded.
 *
 * What is left is a state machine, and that is what this store is. The states
 * are the whole point: "checking", "up to date", "an update is available",
 * "downloading", "installing", and the two ways it can fail, are different
 * things to show a user, and collapsing them into a boolean is how an
 * application ends up installing something nobody agreed to.
 *
 * Nothing here runs on its own. A check happens when the user asks for one, and
 * an installation happens only after they confirm — the specification for this
 * phase is explicit that an update must not arrive while somebody is working.
 */

import { create } from "zustand";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

import { BackendError } from "@/services/ipc";

/** Where an update attempt has got to. */
export type UpdatePhase =
  | "idle"
  | "checking"
  | "up-to-date"
  | "available"
  | "downloading"
  | "installing"
  | "restart-required"
  | "error";

/** Which step failed, because the recovery differs. */
export type UpdateProblem = "check" | "download" | "install";

interface UpdateState {
  phase: UpdatePhase;
  /** The version the release channel announced, once one has been found. */
  version: string | null;
  /** The release notes the channel supplied, when it supplied any. */
  notes: string | null;
  /** Bytes received and the package's size, while downloading. */
  downloadedBytes: number;
  totalBytes: number | null;
  problem: UpdateProblem | null;
  error: BackendError | null;

  /** Ask the configured channel whether a newer release exists. */
  checkForUpdate: () => Promise<void>;
  /** Download and install the update the last check found. */
  installUpdate: () => Promise<void>;
  /** Restart into the freshly installed version. */
  restart: () => Promise<void>;
  /** Return to the resting state, dropping whatever was found or failed. */
  dismiss: () => void;
}

/**
 * The update the last check found.
 *
 * Deliberately outside the store: it is a live handle to a downloaded resource
 * on the Rust side, not state, and putting it where React can render it invites
 * somebody to compare it by value. It is closed whenever it is dropped, so the
 * Rust side is not left holding a download nobody will install.
 */
let pending: Update | null = null;

async function discardPending() {
  const update = pending;
  pending = null;
  if (update) {
    // Best effort: the handle is being dropped whatever happens here.
    await update.close().catch(() => {});
  }
}

/** Turn whatever the plugin threw into something safe to show. */
function updateError(problem: UpdateProblem, raw: unknown): BackendError {
  const message =
    problem === "check"
      ? "The update check failed. The release channel could not be reached, or is not configured for this build."
      : problem === "download"
        ? "The update could not be downloaded. Nothing has been installed; the version you are running is unchanged."
        : "The update could not be installed. Nothing has been changed; the version you are running is unchanged.";

  return new BackendError({
    kind: "network",
    code: `update_${problem}_failed`,
    message,
    // The plugin's own text, which names the cause: an unconfigured feed, a
    // TLS failure, a signature that did not verify. It never contains a token
    // or a private endpoint — the endpoints are compiled into the build.
    details: raw instanceof Error ? raw.message : String(raw),
  });
}

export const useUpdateStore = create<UpdateState>()((set, get) => ({
  phase: "idle",
  version: null,
  notes: null,
  downloadedBytes: 0,
  totalBytes: null,
  problem: null,
  error: null,

  checkForUpdate: async () => {
    // A second check while one is running would race the first.
    if (get().phase === "checking" || get().phase === "downloading") return;

    await discardPending();
    set({
      phase: "checking",
      version: null,
      notes: null,
      problem: null,
      error: null,
      downloadedBytes: 0,
      totalBytes: null,
    });

    try {
      const found = await check();
      if (!found) {
        set({ phase: "up-to-date" });
        return;
      }
      pending = found;
      set({
        phase: "available",
        version: found.version,
        notes: found.body?.trim() ? found.body.trim() : null,
      });
    } catch (raw) {
      set({ phase: "error", problem: "check", error: updateError("check", raw) });
    }
  },

  installUpdate: async () => {
    const update = pending;
    if (!update || get().phase !== "available") return;

    set({
      phase: "downloading",
      downloadedBytes: 0,
      totalBytes: null,
      problem: null,
      error: null,
    });

    try {
      await update.download((event) => {
        if (event.event === "Started") {
          set({ totalBytes: event.data.contentLength ?? null });
        } else if (event.event === "Progress") {
          set({ downloadedBytes: get().downloadedBytes + event.data.chunkLength });
        }
      });
    } catch (raw) {
      // The download failed, so the signature was never accepted and nothing
      // was installed. The handle is spent either way.
      await discardPending();
      set({ phase: "error", problem: "download", error: updateError("download", raw) });
      return;
    }

    set({ phase: "installing" });
    try {
      // On Windows this hands the installer the package and exits the
      // application, so the code after it does not run; the installer relaunches
      // it into the new version. Elsewhere the process survives and the restart
      // is asked for separately.
      await update.install();
      pending = null;
      set({ phase: "restart-required" });
    } catch (raw) {
      await discardPending();
      set({ phase: "error", problem: "install", error: updateError("install", raw) });
    }
  },

  restart: async () => {
    await relaunch();
  },

  dismiss: () => {
    void discardPending();
    set({
      phase: "idle",
      version: null,
      notes: null,
      downloadedBytes: 0,
      totalBytes: null,
      problem: null,
      error: null,
    });
  },
}));
