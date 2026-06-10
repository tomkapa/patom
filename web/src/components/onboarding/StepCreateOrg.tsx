import { useMemo, useState } from "react";
import { ArrowRight, Building2 } from "lucide-react";
import { ApiError } from "../../lib/errors";
import { useCreateOrg } from "../../hooks/useOrg";
import { useAuthStore } from "../../stores/authStore";
import { track } from "../../lib/analytics";
import { TitleWithMossPill } from "./OnboardingTopBar";

const MIN = 1;
const MAX = 200;

/** Step 1 — name and **create** the workspace. Submission is a
 *  `POST /me/orgs`, which creates the org (caller becomes Owner), seeds a
 *  default agent, and switches the session into the fresh, not-yet-
 *  onboarded org. Used for both first-time signup (org-less session) and
 *  "create another workspace" (`/onboarding?new=1`). The `onboarded` flag
 *  is flipped only at the final step. */
export function StepCreateOrg({ onContinue }: { onContinue: () => void }) {
  // Seed the field from the user's display name as a friendly default —
  // there is no pre-created org to read a name from anymore.
  const seededName = useAuthStore((s) => s.me?.user.display_name ?? "");
  const [name, setName] = useState(seededName);
  const trimmed = name.trim();
  const valid = trimmed.length >= MIN && trimmed.length <= MAX;
  const monogramLetter = useMemo(
    () => (trimmed[0] ?? "A").toUpperCase(),
    [trimmed],
  );

  const create = useCreateOrg();
  const capReached =
    create.error instanceof ApiError && create.error.status === 409;
  const submit = () =>
    // `useCreateOrg` already invalidates every query, so /me refetches
    // under the new session before we advance to the next step.
    create.mutate(trimmed, {
      onSuccess: () => {
        track("workspace_created");
        onContinue();
      },
    });

  return (
    <form
      className="flex w-[480px] flex-col bg-[var(--color-surface-primary)] shadow-[0_12px_40px_rgba(30,51,34,0.12)] ring-1 ring-[var(--color-moss-deep)]"
      onSubmit={(e) => {
        e.preventDefault();
        if (valid && !create.isPending) submit();
      }}
      data-step="create-org"
    >
      {/* Top moss accent bar */}
      <div className="h-1.5 w-full bg-[var(--color-moss)]" aria-hidden="true" />

      {/* Head */}
      <div className="flex flex-col items-center gap-3 px-10 pt-8 pb-6">
        <div className="flex h-[52px] w-[52px] items-center justify-center bg-[var(--color-moss-soft)]">
          <Building2 className="h-[26px] w-[26px] text-[var(--color-moss)]" />
        </div>
        <TitleWithMossPill prefix="Name your" highlight="workspace" />
        <p className="max-w-[380px] text-center text-[14px] leading-[1.5] text-[var(--color-fg-secondary)]">
          This is your company's home for agents and teammates. You can rename
          it anytime.
        </p>
      </div>

      {/* Body */}
      <div className="flex flex-col gap-2 px-10 py-2">
        <div className="flex items-center gap-2">
          <label
            htmlFor="onboarding-workspace-name"
            className="text-[14px] font-medium text-[var(--color-moss-deep)]"
          >
            Workspace name
          </label>
          <span className="font-[var(--font-mono)] text-[9px] tracking-[1px] text-[var(--color-moss)]">
            REQUIRED
          </span>
        </div>
        <div className="flex items-center gap-2.5 bg-[var(--color-surface-primary)] px-3.5 py-3 ring-[1.5px] ring-[var(--color-moss)] focus-within:ring-[var(--color-moss-deep)]">
          <span
            aria-hidden="true"
            className="flex h-[26px] w-[26px] items-center justify-center bg-[var(--color-moss)] font-[var(--font-display)] text-[13px] font-bold text-white"
          >
            {monogramLetter}
          </span>
          <input
            id="onboarding-workspace-name"
            autoFocus
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="Atlas Labs"
            maxLength={MAX}
            className="flex-1 bg-transparent text-[15px] font-medium text-[var(--color-moss-deep)] caret-[var(--color-moss)] outline-none placeholder:text-[var(--color-fg-muted)]"
            data-testid="onboarding-workspace-name"
          />
        </div>
        {create.isError && (
          <p
            className="mt-1 text-[12px] text-[var(--color-rose)]"
            role="alert"
          >
            {capReached
              ? "You've reached the maximum number of workspaces."
              : "Couldn't create the workspace. Try again."}
          </p>
        )}
      </div>

      {/* Foot */}
      <div className="flex flex-col px-10 pt-6 pb-7">
        <button
          type="submit"
          disabled={!valid || create.isPending}
          className="inline-flex w-full cursor-pointer items-center justify-center gap-2 bg-[var(--color-moss)] px-5 py-3.5 text-[15px] font-semibold text-white transition-colors hover:bg-[var(--color-moss-deep)] disabled:cursor-not-allowed disabled:opacity-50"
          data-testid="onboarding-continue"
        >
          {create.isPending ? "Saving…" : "Continue"}
          <ArrowRight className="h-4 w-4" />
        </button>
      </div>
    </form>
  );
}
