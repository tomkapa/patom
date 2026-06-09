import { Check, X } from "lucide-react";
import { useAuthStore } from "../../stores/authStore";
import { Button } from "../atoms/Button";

export type StepKey = "name" | "team" | "invite";

const STEPS: { key: StepKey; n: number; label: string }[] = [
  { key: "name", n: 1, label: "Create org" },
  { key: "team", n: 2, label: "Choose a team" },
  { key: "invite", n: 3, label: "Invite" },
];

const STEP_ORDER: readonly StepKey[] = STEPS.map((s) => s.key);

type Status = "done" | "active" | "pending";

function stepStatus(step: StepKey, current: StepKey, allDone: boolean): Status {
  if (allDone) return "done";
  if (step === current) return "active";
  return STEP_ORDER.indexOf(step) < STEP_ORDER.indexOf(current)
    ? "done"
    : "pending";
}

/** Top bar shared across all four onboarding screens. Logo + a 3-step
 *  progress strip + the signed-in user's email. The stepper auto-advances
 *  based on the `current` prop; the final "Team Provisioned" screen
 *  passes `allDone` so every step renders as a green check.
 *
 *  Mirrors the design's `topBar` frame in `lY4bl`, `TbiZC`, `Ikd5b`, `G23zJ3`. */
export function OnboardingTopBar({
  current,
  allDone = false,
  onCancel,
  cancelling = false,
}: {
  current: StepKey;
  allDone?: boolean;
  /** When set, the left slot renders a Cancel button that abandons the
   *  workspace-creation flow (see `Onboarding`). Omitted on the final
   *  "done" screen, where there's nothing left to cancel. */
  onCancel?: () => void;
  cancelling?: boolean;
}) {
  const email = useAuthStore((s) => s.me?.user.email ?? "");

  return (
    <header
      className="flex h-[60px] w-full items-center justify-between border-b border-[var(--color-border-subtle)] bg-[var(--color-surface-primary)] px-8"
      data-onboarding-topbar
    >
      {/* Left: brand mark */}
      <div className="flex items-center gap-2.5">
        <span
          aria-hidden="true"
          className="inline-flex h-7 w-7 items-center justify-center bg-[var(--color-moss)] font-[var(--font-display)] text-[15px] font-bold text-white"
        >
          P
        </span>
        <span className="font-[var(--font-display)] text-[16px] font-bold text-[var(--color-moss-deep)]">
          Patom
        </span>
      </div>

      {/* Center: stepper */}
      <ol className="flex items-center gap-2.5" aria-label="Onboarding steps">
        {STEPS.map((s, i) => {
          const status = stepStatus(s.key, current, allDone);
          return (
            <li
              key={s.key}
              className="flex items-center gap-2.5"
              aria-current={status === "active" ? "step" : undefined}
            >
              <div className="flex items-center gap-[7px]">
                <StepDot n={s.n} status={status} />
                <span
                  className={
                    "text-[13px] " +
                    (status === "active"
                      ? "font-semibold text-[var(--color-moss-deep)]"
                      : status === "done"
                        ? "font-medium text-[var(--color-fg-muted)]"
                        : "font-normal text-[var(--color-fg-muted)]")
                  }
                >
                  {s.label}
                </span>
              </div>
              {i < STEPS.length - 1 && (
                <span
                  aria-hidden="true"
                  className={
                    "h-px w-6 " +
                    (status === "done" || allDone
                      ? "bg-[var(--color-moss)]"
                      : "bg-[var(--color-border-subtle)]")
                  }
                />
              )}
            </li>
          );
        })}
      </ol>

      {/* Right: Cancel button while a flow is in progress, else the
          signed-in user's email. */}
      {onCancel ? (
        <Button
          variant="danger"
          loading={cancelling}
          onClick={onCancel}
          data-testid="onboarding-cancel"
        >
          <X className="h-3.5 w-3.5" strokeWidth={2} />
          Cancel
        </Button>
      ) : (
        <div className="font-[var(--font-mono)] text-[12px] text-[var(--color-fg-muted)]">
          {email}
        </div>
      )}
    </header>
  );
}

function StepDot({ n, status }: { n: number; status: Status }) {
  if (status === "active") {
    return (
      <span className="inline-flex h-5 w-5 items-center justify-center rounded-full bg-[var(--color-moss)] font-[var(--font-mono)] text-[11px] font-semibold text-white">
        {n}
      </span>
    );
  }
  if (status === "done") {
    return (
      <span className="inline-flex h-5 w-5 items-center justify-center rounded-full border border-[var(--color-border-subtle)] bg-[var(--color-moss-soft)] text-[var(--color-moss)]">
        <Check className="h-[11px] w-[11px]" strokeWidth={3} />
      </span>
    );
  }
  return (
    <span className="inline-flex h-5 w-5 items-center justify-center rounded-full border border-[var(--color-border-subtle)] bg-[var(--color-surface-secondary)] font-[var(--font-mono)] text-[11px] font-semibold text-[var(--color-fg-muted)]">
      {n}
    </span>
  );
}

/** The "X your [highlight]" headline that wraps a noun in a moss pill —
 *  used on the Create-Org, Invite, and Team-Provisioned cards. */
export function TitleWithMossPill({
  prefix,
  highlight,
  suffix,
  size = 26,
}: {
  prefix: string;
  highlight: string;
  suffix?: string;
  size?: number;
}) {
  return (
    <h2 className="flex items-center gap-2 text-center font-[var(--font-display)] font-bold text-[var(--color-moss-deep)]">
      <span style={{ fontSize: size }}>{prefix}</span>
      <span
        className="inline-flex items-center justify-center bg-[var(--color-moss)] px-2.5 py-px text-white"
        style={{ fontSize: size }}
      >
        {highlight}
      </span>
      {suffix && <span style={{ fontSize: size }}>{suffix}</span>}
    </h2>
  );
}
