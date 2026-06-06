import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { OnboardingTopBar, type StepKey } from "../components/onboarding/OnboardingTopBar";
import { StepCreateOrg } from "../components/onboarding/StepCreateOrg";
import { StepChooseTeam } from "../components/onboarding/StepChooseTeam";
import { StepInvite } from "../components/onboarding/StepInvite";
import { StepProvisioned } from "../components/onboarding/StepProvisioned";
import { ME_QUERY_KEY } from "../hooks/useMe";
import { useLightModeOnly } from "../lib/theme";
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
  useLightModeOnly();

  // On the final ("done") screen the stepper paints every step as a
  // green check (`allDone={true}`); meanwhile we still need to hand
  // OnboardingTopBar a valid 3-step key for its prop type, so "done"
  // collapses to "invite".
  const stepperKey: StepKey = step === "done" ? "invite" : step;

  return (
    <div className="grid h-screen w-screen grid-rows-[60px_1fr] bg-[var(--color-surface-secondary)]">
      <OnboardingTopBar current={stepperKey} allDone={step === "done"} />
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
            onOpenWorkspace={() => {
              // Now refetch /me so the gate sees onboarded:true on the
              // next render at /, then navigate.
              void qc.invalidateQueries({ queryKey: ME_QUERY_KEY });
              navigate("/", { replace: true });
            }}
          />
        )}
      </main>
    </div>
  );
}
