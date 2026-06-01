// Reactive viewport queries. Mirrors the `window.matchMedia` pattern in
// `lib/theme.ts` (which uses it for `prefers-color-scheme`), but exposed
// as a hook so layout components can branch which tree they mount —
// CSS-only `md:hidden` can't decide e.g. "render the sidebar inside a
// Drawer vs inline", which is a structural choice, not a style.
//
// Breakpoints match Tailwind v4 defaults: `md` = 768px, `lg` = 1024px.

import { useMemo, useSyncExternalStore } from "react";

/** `true` when `query` currently matches. SSR-safe (returns `false` on
 *  the server snapshot). Re-renders the caller on every match change. */
export function useMediaQuery(query: string): boolean {
  // Build one MediaQueryList per query and keep stable subscribe/getSnapshot
  // identities, so useSyncExternalStore subscribes once instead of churning
  // a fresh listener (and a fresh matchMedia call) on every render.
  const [subscribe, getSnapshot] = useMemo(() => {
    if (typeof window === "undefined") {
      return [() => () => {}, () => false] as const;
    }
    const mql = window.matchMedia(query);
    return [
      (cb: () => void) => {
        mql.addEventListener("change", cb);
        return () => mql.removeEventListener("change", cb);
      },
      () => mql.matches,
    ] as const;
  }, [query]);

  return useSyncExternalStore(subscribe, getSnapshot, () => false);
}

/** Compact = below Tailwind's `md` (768px): phones and small tablets.
 *  Drives the bottom-tab-bar + drawer chrome. */
export function useIsCompact(): boolean {
  return !useMediaQuery("(min-width: 768px)");
}

/** Wide = at/above Tailwind's `lg` (1024px): room for the chat thread
 *  panel as an inline fourth column rather than an overlay. */
export function useIsWide(): boolean {
  return useMediaQuery("(min-width: 1024px)");
}
