import type { ReactNode } from "react";
import { Navigate, useLocation } from "react-router-dom";
import { useActiveOrg } from "../../hooks/useMe";

const ONBOARDING_PATH = "/onboarding";

/** Gate that lives just inside `Protected` (auth has resolved, `me` is
 *  in the store) and steers the user between `/onboarding` and the rest
 *  of the app based on `activeOrg.onboarded`.
 *
 *  - Org still needs onboarding (`onboarded === false`) and the user is
 *    NOT already on `/onboarding` → redirect them there.
 *  - Org is already onboarded (`onboarded === true`) and the user IS on
 *    `/onboarding` → redirect to `/` so they can't revisit the wizard.
 *
 *  When the active org can't be resolved yet (rare — usually means the
 *  JWT's `active_org_id` doesn't appear in `me.orgs`), do nothing and
 *  let the children render. That mirrors today's behavior. */
export function OnboardingGate({ children }: { children: ReactNode }) {
  const activeOrg = useActiveOrg();
  const { pathname } = useLocation();
  const onWizard = pathname === ONBOARDING_PATH;

  if (activeOrg) {
    if (!activeOrg.onboarded && !onWizard) {
      return <Navigate to={ONBOARDING_PATH} replace />;
    }
    if (activeOrg.onboarded && onWizard) {
      return <Navigate to="/" replace />;
    }
  }

  return <>{children}</>;
}
