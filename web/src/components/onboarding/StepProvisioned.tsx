import { ArrowRight, Check, MessageSquare, PartyPopper } from "lucide-react";
import { TitleWithMossPill } from "./OnboardingTopBar";
import { LucideByName } from "./LucideByName";
import type { TeamPreset } from "../../data/teamPresets";

/** The always-on default agent the BE seeded on OIDC callback. Pinned at
 *  the top of the success screen regardless of preset (see plan §1.5,
 *  user direction "Recruiter always shown first"). */
const RECRUITER = {
  name: "Recruiter",
  description: "Hires & configures agents",
  icon: "user-round-search",
} as const;

/** Step 4 — celebrate. The wizard's onboarded flag has already been
 *  flipped to true in the previous step (Skip-for-now or Send-invites);
 *  this screen just paints the success state and hands the user the
 *  "Open workspace" CTA. */
export function StepProvisioned({
  preset,
  onOpenWorkspace,
}: {
  preset: TeamPreset;
  onOpenWorkspace: () => void;
}) {
  // Recruiter first; preset agents follow. For "scratch" the roster is
  // just Recruiter.
  const roster = [RECRUITER, ...preset.agents];

  return (
    <div
      className="flex w-[560px] flex-col bg-[var(--color-surface-primary)] shadow-[0_12px_40px_rgba(30,51,34,0.12)] ring-1 ring-[var(--color-moss-deep)]"
      data-step="done"
    >
      <div className="h-1.5 w-full bg-[var(--color-moss)]" aria-hidden="true" />

      {/* Head */}
      <div className="flex flex-col items-center gap-3.5 border-b border-[var(--color-border-subtle)] px-8 pt-8 pb-6">
        <div className="flex h-14 w-14 items-center justify-center bg-[var(--color-moss)] text-white">
          <PartyPopper className="h-7 w-7" />
        </div>
        {(() => {
          const scratch = preset.id === "scratch";
          return (
            <TitleWithMossPill
              prefix={scratch ? "Your workspace is" : `Your ${preset.display_name} team is`}
              highlight={scratch ? "ready" : "hired"}
              size={22}
            />
          );
        })()}
        <p className="max-w-[420px] text-center text-[14px] leading-[1.5] text-[var(--color-fg-secondary)]">
          {roster.length === 1
            ? "Your Recruiter is set up and ready. Open the workspace and hire more agents from there."
            : `${roster.length} agents are set up and ready. Message your Lead to kick things off, or open the workspace.`}
        </p>
      </div>

      {/* Roster */}
      <ul className="flex flex-col" data-testid="provisioned-roster">
        {roster.map((a, i) => (
          <li
            key={`${a.name}-${i}`}
            className={
              "flex items-center gap-3 px-8 py-3 " +
              (i < roster.length - 1
                ? "border-b border-[var(--color-border-subtle)]"
                : "")
            }
          >
            <div
              className={
                "flex h-[34px] w-[34px] items-center justify-center " +
                (i === 0
                  ? "bg-[var(--color-moss)] text-white"
                  : "bg-[var(--color-moss-soft)] text-[var(--color-moss)]")
              }
            >
              <LucideByName name={a.icon} size={17} />
            </div>
            <div className="flex min-w-0 flex-1 flex-col gap-px">
              <div className="text-[14px] font-medium text-[var(--color-moss-deep)]">
                {a.name}
              </div>
              <div className="text-[12px] text-[var(--color-fg-muted)]">
                {a.description}
              </div>
            </div>
            <div className="flex items-center gap-1.5">
              <Check className="h-3 w-3 text-[var(--color-moss)]" strokeWidth={3} />
              <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-fg-muted)]">
                hired
              </span>
            </div>
          </li>
        ))}
      </ul>

      {/* Foot */}
      <div className="flex flex-col gap-2.5 px-8 pt-5 pb-6">
        <button
          type="button"
          disabled
          className="inline-flex w-full cursor-not-allowed items-center justify-center gap-2 bg-[var(--color-surface-primary)] px-4 py-3 text-[14px] font-medium text-[var(--color-moss-deep)] opacity-60 ring-1 ring-[var(--color-border-subtle)]"
          title="Open the workspace first, then start a chat from there"
        >
          <MessageSquare className="h-[15px] w-[15px]" />
          Message {roster[0]?.name ?? "Recruiter"}
        </button>
        <button
          type="button"
          onClick={onOpenWorkspace}
          className="inline-flex w-full cursor-pointer items-center justify-center gap-2 bg-[var(--color-moss)] px-4 py-3 text-[14px] font-semibold text-white transition-colors hover:bg-[var(--color-moss-deep)]"
          data-testid="onboarding-open-workspace"
        >
          Open workspace
          <ArrowRight className="h-[15px] w-[15px]" />
        </button>
      </div>
    </div>
  );
}
