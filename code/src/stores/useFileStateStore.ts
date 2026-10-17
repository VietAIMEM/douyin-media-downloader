/**
 * The application's one copy of what is on disk.
 *
 * Separate from the download store on purpose, because it is a different fact.
 * The download store knows what the *application* has done — queued, running,
 * finished. This knows what the *filesystem* holds, and it is the only thing
 * that can answer "is the file still there", which the task store cannot answer
 * and must not pretend to.
 *
 * Kept apart from the task state rather than folded into it. A download that
 * completed once is still a completed download after the user deletes the file;
 * rewriting the task as failed would be this application disagreeing with its
 * own history because of something it did not do.
 *
 * It is scanned deliberately — when a screen is opened and when the user asks —
 * never on a render, because every scan touches the disk once per recorded file.
 */

import { create } from "zustand";

import { api } from "@/services/api";
import { normalizeError, type BackendError } from "@/services/ipc";
import type { FileScan, FileScanEntry, FileState, Id } from "@/types";

interface FileStateStore {
  /** The last scan, or null until one has run. */
  scan: FileScan | null;
  scanning: boolean;
  error: BackendError | null;
  /** Bumped on every completed scan, so a screen can tell a fresh answer from a stale one. */
  revision: number;
  /** The scan, by task. Built once per scan rather than searched per row. */
  index: Map<Id, FileScanEntry>;
  /** Look one task up. Absent means it was not scanned — which is not "missing". */
  stateOf: (taskId: Id) => FileState | null;
  entryOf: (taskId: Id) => FileScanEntry | null;
  /** Read the disk again. */
  rescan: () => Promise<void>;
}

export const useFileStateStore = create<FileStateStore>((set, get) => ({
  scan: null,
  scanning: false,
  error: null,
  revision: 0,
  index: new Map(),

  stateOf: (taskId) => get().index.get(taskId)?.state ?? null,
  entryOf: (taskId) => get().index.get(taskId) ?? null,

  rescan: async () => {
    if (get().scanning) return;
    set({ scanning: true });
    try {
      const scan = await api.scanDownloadedFiles();
      set((current) => ({
        scan,
        index: new Map(scan.entries.map((entry) => [entry.taskId, entry])),
        error: null,
        revision: current.revision + 1,
      }));
    } catch (raw) {
      // The previous scan is kept. A refresh that failed says nothing about the
      // disk, and replacing a good answer with an error would lose information
      // for no reason.
      set({ error: normalizeError(raw) });
    } finally {
      set({ scanning: false });
    }
  },
}));

/**
 * How to describe a file state to someone reading it.
 *
 * `unavailable` is deliberately not "missing". It means the answer could not be
 * established — a drive that is not attached, a folder this process may not
 * read, a path recorded under a download folder that has since been changed —
 * and telling the user their file is gone in any of those cases would be
 * asserting something nobody checked.
 */
export function fileStateLabel(state: FileState | null): string | null {
  switch (state) {
    case "present":
      return null;
    case "missing":
      return "File missing";
    case "unavailable":
      return "Could not be checked";
    default:
      return null;
  }
}

/** What to say when a state could not be established, in the user's terms. */
export function fileStateHint(state: FileState | null, reason?: string): string | null {
  if (state !== "unavailable") {
    return state === "missing"
      ? "The file this download produced is no longer in your download folder. The record is kept — downloading it again is safe."
      : null;
  }
  switch (reason) {
    case "outside-configured-root":
      return "This was saved under a different download folder than the one configured now, so it cannot be checked.";
    case "permission-denied":
      return "This file could not be looked at — the folder or drive refused access. It has not been marked missing.";
    case "directory-unavailable":
      return "The folder this file lives in could not be read, so whether the file is there is unknown.";
    default:
      return "Whether this file is still there could not be established just now.";
  }
}
