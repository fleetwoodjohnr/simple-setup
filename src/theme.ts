import type { ThemePref } from "./types";

/** The theme is kept in localStorage as well as in the settings file, so it applies before the
 * first paint (the settings arrive a moment later over IPC). */
const KEY = "simple-setup-theme";
const DARK = "(prefers-color-scheme: dark)";

export function resolveTheme(pref: ThemePref, systemDark: boolean): "light" | "dark" {
  return pref === "system" ? (systemDark ? "dark" : "light") : pref;
}

export function storedTheme(): ThemePref {
  const v = localStorage.getItem(KEY);
  return v === "light" || v === "dark" ? v : "system";
}

let current: ThemePref = "system";

export function applyTheme(pref: ThemePref) {
  current = pref;
  localStorage.setItem(KEY, pref);
  document.documentElement.dataset.theme = resolveTheme(pref, matchMedia(DARK).matches);
}

/** Applies the stored preference and follows the system while it is "system". */
export function startTheme() {
  applyTheme(storedTheme());
  matchMedia(DARK).addEventListener("change", () => {
    if (current === "system") applyTheme("system");
  });
}
