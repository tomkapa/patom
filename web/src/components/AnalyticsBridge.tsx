import { useEffect, useRef } from "react";
import { useLocation } from "react-router-dom";
import { useAuthStore } from "../stores/authStore";
import { useActiveOrg } from "../hooks/useMe";
import * as analytics from "../lib/analytics";

/** Headless bridge between app state and the analytics seam. Mounted once at
 *  the `App` root (inside `BrowserRouter`, outside `Protected`) so route
 *  tracking works on every page — incl. `/sign-in` — while identity stays
 *  `null` until `useMe` populates the auth store.
 *
 *  Three responsibilities: identify the user + their active org when `me`
 *  loads or the org switches, emit `signed_in` once per identity, and send a
 *  manual `$pageview` on every route change. No-ops entirely without a key. */
export function AnalyticsBridge() {
  const me = useAuthStore((s) => s.me);
  const org = useActiveOrg();
  const location = useLocation();
  // Last identity we called `identify()` for, so an org switch re-identifies
  // but a re-render with the same identity does not.
  const lastIdentity = useRef<string | null>(null);

  useEffect(() => {
    if (!me) return;
    const identity = `${me.user.id}:${me.active_org_id ?? ""}`;
    if (lastIdentity.current === identity) return;
    const firstIdentify = lastIdentity.current === null;
    lastIdentity.current = identity;
    analytics.identify(me.user, org);
    // `signed_in` fires once, on the first identify of the session — not on
    // subsequent org switches (those are covered by `org_switched`).
    if (firstIdentify) analytics.track("signed_in");
  }, [me, org]);

  useEffect(() => {
    analytics.pageview(location.pathname);
  }, [location.pathname]);

  return null;
}
