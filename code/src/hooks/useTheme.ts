/**
 * Applies the stored theme to the document.
 *
 * The theme is a persisted setting, so this hook is the only place that writes
 * the `dark` class. `system` is resolved through `prefers-color-scheme` and
 * re-resolved when the OS preference changes, which is why the media query is
 * subscribed to rather than read once.
 */

import { useEffect } from "react";

import { useSettingsStore } from "@/stores/useSettingsStore";
import type { Theme } from "@/types";

const DARK_QUERY = "(prefers-color-scheme: dark)";

function prefersDark(): boolean {
  return window.matchMedia(DARK_QUERY).matches;
}

function applyTheme(theme: Theme): void {
  const dark = theme === "dark" || (theme === "system" && prefersDark());
  document.documentElement.classList.toggle("dark", dark);
}

export function useTheme(): Theme {
  // Before settings load this matches the bootstrap script in index.html.
  const theme: Theme = useSettingsStore((state) => state.settings?.theme ?? "system");

  useEffect(() => {
    applyTheme(theme);
    if (theme !== "system") return;

    const media = window.matchMedia(DARK_QUERY);
    const onChange = () => applyTheme("system");
    media.addEventListener("change", onChange);
    return () => media.removeEventListener("change", onChange);
  }, [theme]);

  return theme;
}
