// Local-only theme preference. Not synced to the server — each
// browser/device picks its own. `"system"` follows OS preference and
// reacts live to `prefers-color-scheme` changes.

export type ThemePref = "system" | "light" | "dark";

const STORAGE_KEY = "relay.theme";
const VALID: readonly ThemePref[] = ["system", "light", "dark"] as const;

function isThemePref(v: unknown): v is ThemePref {
  return typeof v === "string" && (VALID as readonly string[]).includes(v);
}

export function getStoredTheme(): ThemePref {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    return isThemePref(raw) ? raw : "system";
  } catch {
    return "system";
  }
}

function resolveEffective(pref: ThemePref): "light" | "dark" {
  if (pref === "light" || pref === "dark") return pref;
  return window.matchMedia("(prefers-color-scheme: dark)").matches
    ? "dark"
    : "light";
}

function applyClass(effective: "light" | "dark") {
  const root = document.documentElement;
  if (effective === "dark") root.classList.add("dark");
  else root.classList.remove("dark");
  root.style.colorScheme = effective;
}

let mql: MediaQueryList | null = null;
let mqlListener: ((e: MediaQueryListEvent) => void) | null = null;

export function applyTheme(pref: ThemePref): void {
  applyClass(resolveEffective(pref));

  if (mql && mqlListener) {
    mql.removeEventListener("change", mqlListener);
    mql = null;
    mqlListener = null;
  }

  if (pref === "system") {
    mql = window.matchMedia("(prefers-color-scheme: dark)");
    mqlListener = (e) => applyClass(e.matches ? "dark" : "light");
    mql.addEventListener("change", mqlListener);
  }
}

export function setTheme(pref: ThemePref): void {
  try {
    localStorage.setItem(STORAGE_KEY, pref);
  } catch {
    // Storage may be unavailable (private mode, quota); apply anyway
    // for the current tab.
  }
  applyTheme(pref);
}
