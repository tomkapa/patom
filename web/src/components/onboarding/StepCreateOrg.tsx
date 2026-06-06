import { useMemo, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { ArrowRight, Building2 } from "lucide-react";
import { api } from "../../lib/api";
import { ME_QUERY_KEY } from "../../hooks/useMe";
import { useAuthStore } from "../../stores/authStore";
import { TitleWithMossPill } from "./OnboardingTopBar";

const MIN = 1;
const MAX = 200;

/** Step 1 — pick the workspace name. The backend already auto-created
 *  a placeholder org on first sign-in, so submission is a PATCH /me/org
 *  with `{ name }`. The org's `onboarded` flag is NOT flipped here —
 *  that happens only at the final step. */
export function StepCreateOrg({ onContinue }: { onContinue: () => void }) {
  const seededName = useAuthStore(
    (s) =>
      s.me?.orgs.find((o) => o.id === s.me?.active_org_id)?.name ?? "",
  );
  const [name, setName] = useState(seededName);
  const trimmed = name.trim();
  const valid = trimmed.length >= MIN && trimmed.length <= MAX;
  const monogramLetter = useMemo(
    () => (trimmed[0] ?? "A").toUpperCase(),
    [trimmed],
  );

  const qc = useQueryClient();
  const m = useMutation({
    mutationFn: () => api.updateOrg({ name: trimmed }),
    onSuccess: async () => {
      // Update /me so the next step (and the gate) sees the new name.
      await qc.invalidateQueries({ queryKey: ME_QUERY_KEY });
      onContinue();
    },
  });

  return (
    <form
      className="flex w-[480px] flex-col bg-[var(--color-surface-primary)] shadow-[0_12px_40px_rgba(30,51,34,0.12)] ring-1 ring-[var(--color-moss-deep)]"
      onSubmit={(e) => {
        e.preventDefault();
        if (valid && !m.isPending) m.mutate();
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
        {m.isError && (
          <p
            className="mt-1 text-[12px] text-[var(--color-rose)]"
            role="alert"
          >
            Couldn't save the name. Try again.
          </p>
        )}
      </div>

      {/* Foot */}
      <div className="flex flex-col px-10 pt-6 pb-7">
        <button
          type="submit"
          disabled={!valid || m.isPending}
          className="inline-flex w-full cursor-pointer items-center justify-center gap-2 bg-[var(--color-moss)] px-5 py-3.5 text-[15px] font-semibold text-white transition-colors hover:bg-[var(--color-moss-deep)] disabled:cursor-not-allowed disabled:opacity-50"
          data-testid="onboarding-continue"
        >
          {m.isPending ? "Saving…" : "Continue"}
          <ArrowRight className="h-4 w-4" />
        </button>
      </div>
    </form>
  );
}
