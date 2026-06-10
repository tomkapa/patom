import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { OnboardingTopBar, type StepKey } from "../components/onboarding/OnboardingTopBar";
import { StepCreateOrg } from "../components/onboarding/StepCreateOrg";
import { StepChooseTeam } from "../components/onboarding/StepChooseTeam";
import { StepInvite } from "../components/onboarding/StepInvite";
import { StepProvisioned } from "../components/onboarding/StepProvisioned";
import { ME_QUERY_KEY } from "../hooks/useMe";
import { useDeleteOrg } from "../hooks/useOrg";
import { useLogout } from "../hooks/useLogout";
import { useAuthStore } from "../stores/authStore";
import { useLightModeOnly } from "../lib/theme";
import { track } from "../lib/analytics";
import type { PresetId } from "../data/teamPresets";
import { findPreset } from "../data/teamPresets";

type WizardStep = "name" | "team" | "invite" | "done";

/** First-time-user wizard. Single route at `/onboarding`; step state is
 *  internal so the URL doesn't leak which step the user is on (the
 *  `OnboardingGate` is the source of truth for "should this page show
 *  at all"). The wizard is light-only — dark-mode CSS-var inversion
 *  (memory: dark-mode-legacy-token-inversion) breaks the moss-on-paper
 *  card; the sign-in screen forces light for the same reason. */
export function Onboarding() {
  const [step, setStep] = useState<WizardStep>("name");
  const [presetId, setPresetId] = useState<PresetId | null>(null);
  const navigate = useNavigate();
  const qc = useQueryClient();
  const me = useAuthStore((s) => s.me);
  const deleteOrg = useDeleteOrg();
  const logout = useLogout();
  const [cancelling, setCancelling] = useState(false);
  useLightModeOnly();

  // One funnel step per wizard screen. Keyed on `step` so each is recorded
  // once as the user advances; `workspace_created` / `onboarding_completed`
  // fire from the steps' own success handlers.
  useEffect(() => {
    track("onboarding_step_viewed", { step });
  }, [step]);

  // On the final ("done") screen the stepper paints every step as a
  // green check (`allDone={true}`); meanwhile we still need to hand
  // OnboardingTopBar a valid 3-step key for its prop type, so "done"
  // collapses to "invite".
  const stepperKey: StepKey = step === "done" ? "invite" : step;

  // Abandon the workspace-creation flow. Step 1 (`POST /me/orgs`) already
  // creates + switches into the new org, so the active org is the
  // in-progress workspace once we're past it; tear that down. Three cases:
  //   - in-progress org exists (active, un-onboarded) → delete it; land in
  //     a remaining workspace, or sign out if it was the user's only one;
  //   - "create another" abandoned before step 1 created anything (active
  //     org already onboarded) → just return to the existing workspace;
  //   - org-less first-timer, nothing created yet → sign out.
  const onCancel = async () => {
    if (cancelling) return;
    setCancelling(true);
    try {
      const active = me?.orgs.find((o) => o.id === me.active_org_id) ?? null;
      if (active && !active.onboarded) {
        const { active_org_id } = await deleteOrg.mutateAsync();
        if (active_org_id) {
          navigate("/", { replace: true });
        } else {
          logout.mutate();
        }
      } else if (me?.active_org_id) {
        navigate("/", { replace: true });
      } else {
        logout.mutate();
      }
    } catch {
      // Delete failed (e.g. network/server error). Swallow rather than
      // leave an unhandled rejection; `finally` re-enables the button so
      // the user stays in the wizard and can retry Cancel.
    } finally {
      setCancelling(false);
    }
  };

  return (
    <div className="grid h-screen w-screen grid-rows-[60px_1fr] bg-[var(--color-surface-secondary)]">
      <OnboardingTopBar
        current={stepperKey}
        allDone={step === "done"}
        // No cancel on the success screen — the workspace is already live.
        onCancel={step === "done" ? undefined : onCancel}
        cancelling={cancelling}
      />
      <main className="flex h-full w-full items-center justify-center py-10">
        {step === "name" && (
          <StepCreateOrg onContinue={() => setStep("team")} />
        )}
        {step === "team" && (
          <StepChooseTeam
            initialPresetId={presetId}
            onContinue={(picked) => {
              setPresetId(picked);
              setStep("invite");
            }}
          />
        )}
        {step === "invite" && (
          <StepInvite
            onFinished={() => {
              // Important: do NOT invalidate /me here. The backend has
              // already flipped onboarded → true; refetching now would
              // make the OnboardingGate see `onboarded === true` while
              // we're still on /onboarding and redirect to / before the
              // success screen renders. We invalidate on "Open
              // workspace" instead.
              setStep("done");
            }}
          />
        )}
        {step === "done" && (
          <StepProvisioned
            preset={presetId ? findPreset(presetId) : findPreset("scratch")}
            onOpenWorkspace={async () => {
              // Await the refetch before navigating — otherwise the gate
              // could observe the stale `onboarded:false` /me and bounce
              // us straight back to /onboarding.
              await qc.invalidateQueries({ queryKey: ME_QUERY_KEY });
              navigate("/", { replace: true });
            }}
          />
        )}
      </main>
    </div>
  );
}
