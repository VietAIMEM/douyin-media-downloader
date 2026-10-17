/**
 * Backend/diagnostics state.
 *
 * Holds the two read-only snapshots the UI needs from the backend — application
 * info and database health — plus the loading/error state of fetching them.
 * Both are fetched together so the UI shows one consistent picture of the
 * backend rather than two that can disagree.
 */

import { create } from "zustand";

import { api } from "@/services/api";
import { normalizeError, type BackendError } from "@/services/ipc";
import type { AppInfo, DatabaseStatus } from "@/types";

interface BackendState {
  appInfo: AppInfo | null;
  databaseStatus: DatabaseStatus | null;
  loading: boolean;
  error: BackendError | null;
  lastRefreshed: number | null;

  refresh: () => Promise<void>;
}

export const useBackendStore = create<BackendState>()((set) => ({
  appInfo: null,
  databaseStatus: null,
  loading: false,
  error: null,
  lastRefreshed: null,

  refresh: async () => {
    set({ loading: true, error: null });
    try {
      const [appInfo, databaseStatus] = await Promise.all([
        api.getAppInfo(),
        api.getDatabaseStatus(),
      ]);
      set({
        appInfo,
        databaseStatus,
        loading: false,
        lastRefreshed: Date.now(),
      });
    } catch (raw) {
      // A failed refresh keeps the previous snapshot visible alongside the
      // error, which is more useful than blanking the screen.
      set({ loading: false, error: normalizeError(raw) });
    }
  },
}));
