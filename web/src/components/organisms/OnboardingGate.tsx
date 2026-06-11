import type { ReactNode } from "react";
import { Navigate, useLocation } from "react-router-dom";
import { useMe } from "../../hooks/useMe";

const ONBOARDING_PATH = "/onboarding";

/** Gate that lives just inside `Protected` (auth has resolved, `me` is
 *  in the store) and steers the user between `/onboarding` and the rest
 *  of the app.
 *
 *  - **Org-less** (`me.active_org_id == null`, a cloud user who hasn't
 *    created a workspace) and NOT on `/onboarding` → send them to the
 *    wizard, where step 1 creates their first workspace.
 *  - Active org still needs onboarding (`onboarded === false`) and NOT on
 *    `/onboarding` → send them to the wizard.
 *  - Active org already onboarded and ON `/onboarding` → bounce to `/`,
 *    UNLESS the URL carries `?new=1` (an explicit "create another
 *    workspace" intent from the OrgSwitcher). Step 1 then switches the
 *    session into a fresh, not-yet-onboarded org and the normal rules
 *    keep the user in the wizard until they finish.
 *
 *  When the active org id is set but can't be resolved in `me.orgs`
 *  (rare), do nothing and let children render — mirrors prior behavior.
 *
 *  Reads `me` from the React Query cache (same data `Protected` already
 *  confirmed is loaded) rather than the authStore, so the redirect fires
 *  in the same render cycle — preventing children from mounting briefly
 *  and making org-scoped API calls that would 401 an org-less session. */
export function OnboardingGate({ children }: { children: ReactNode }) {
  const { data: me } = useMe();
  const { pathname, search } = useLocation();
  const onWizard = pathname === ONBOARDING_PATH;
  const wantsNew = new URLSearchParams(search).get("new") === "1";

  // Not loaded yet — `Protected` owns the auth gate; render through.
  if (!me) return <>{children}</>;

  // Org-less session: the user must create a workspace before anything
  // else is reachable.
  if (!me.active_org_id) {
    if (!onWizard) return <Navigate to={ONBOARDING_PATH} replace />;
    return <>{children}</>;
  }

  const activeOrg = me.orgs.find((o) => o.id === me.active_org_id) ?? null;

  if (activeOrg) {
    if (!activeOrg.onboarded && !onWizard) {
      return <Navigate to={ONBOARDING_PATH} replace />;
    }
    if (activeOrg.onboarded && onWizard && !wantsNew) {
      return <Navigate to="/" replace />;
    }
  }

  return <>{children}</>;
}
