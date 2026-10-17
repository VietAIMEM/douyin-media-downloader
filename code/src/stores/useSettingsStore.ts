/**
 * Settings state.
 *
 * The store owns loading, saving, and the error of the last operation; the
 * Settings page only renders what is here. Nothing is persisted in the browser
 * (no localStorage): the Rust settings service is the single source of truth,
 * so a stale browser cache can never disagree with the database.
 */

import { create } from "zustand";

import { api } from "@/services/api";
import { normalizeError, type BackendError } from "@/services/ipc";
import type { AppSettings, SettingsOptions } from "@/types";

interface SettingsState {
  settings: AppSettings | null;
  options: SettingsOptions | null;
  loading: boolean;
  saving: boolean;
  error: BackendError | null;
  /** Timestamp of the last successful save, for the "Saved" confirmation. */
  savedAt: number | null;

  load: () => Promise<void>;
  save: (settings: AppSettings) => Promise<boolean>;
  reset: () => Promise<boolean>;
  clearError: () => void;
}

export const useSettingsStore = create<SettingsState>()((set) => ({
  settings: null,
  options: null,
  loading: false,
  saving: false,
  error: null,
  savedAt: null,

  load: async () => {
    set({ loading: true, error: null });
    try {
      const [settings, options] = await Promise.all([
        api.getSettings(),
        api.getSettingsOptions(),
      ]);
      set({ settings, options, loading: false });
    } catch (raw) {
      set({ loading: false, error: normalizeError(raw) });
    }
  },

  save: async (settings) => {
    set({ saving: true, error: null });
    try {
      // The backend returns the validated, stored settings — use its answer
      // rather than the optimistic local copy.
      const stored = await api.saveSettings(settings);
      set({ settings: stored, saving: false, savedAt: Date.now() });
      return true;
    } catch (raw) {
      // Keep the user's edits in the form; only the error changes.
      set({ saving: false, error: normalizeError(raw) });
      return false;
    }
  },

  reset: async () => {
    set({ saving: true, error: null });
    try {
      const stored = await api.resetSettings();
      set({ settings: stored, saving: false, savedAt: Date.now() });
      return true;
    } catch (raw) {
      set({ saving: false, error: normalizeError(raw) });
      return false;
    }
  },

  clearError: () => set({ error: null }),
}));
