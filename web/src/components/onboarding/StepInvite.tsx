import { useState } from "react";
import { useMutation } from "@tanstack/react-query";
import {
  ArrowRight,
  Check,
  ChevronsUpDown,
  Copy,
  Link2,
  Mail,
  UserPlus,
  X,
} from "lucide-react";
import { TitleWithMossPill } from "./OnboardingTopBar";
import { api } from "../../lib/api";
import { useAuthStore } from "../../stores/authStore";
import type { Role } from "../../types/api";

type Pending = { email: string; role: Role };

/** Step 3 — invite teammates (optional). Skip-for-now and Send-N-invites
 *  share one mutation: send any pending invites (no-op when empty), then
 *  flip `onboarded` so the gate releases. */
export function StepInvite({ onFinished }: { onFinished: () => void }) {
  const orgSlug = useAuthStore(
    (s) =>
      s.me?.orgs.find((o) => o.id === s.me?.active_org_id)?.slug ?? "workspace",
  );

  const [pending, setPending] = useState<Pending[]>([]);
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<Role>("member");
  const [copied, setCopied] = useState(false);
  const [addError, setAddError] = useState<string | null>(null);

  const inviteLink = `patom.app/join/${orgSlug}`;

  function addPending() {
    const trimmed = email.trim().toLowerCase();
    if (!trimmed) {
      setAddError("Enter an email address first.");
      return;
    }
    if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(trimmed)) {
      setAddError("That doesn't look like a valid email.");
      return;
    }
    if (pending.some((p) => p.email === trimmed)) {
      setAddError(`${trimmed} is already on the invite list.`);
      setEmail("");
      return;
    }
    setPending((prev) => [...prev, { email: trimmed, role }]);
    setEmail("");
    setAddError(null);
  }

  function removePending(target: string) {
    setPending((prev) => prev.filter((p) => p.email !== target));
  }

  const finish = useMutation({
    mutationFn: async () => {
      // Send invites grouped by role (the BE endpoint takes one role per
      // call). Empty `pending` ⇒ no invite calls, this becomes a pure
      // "mark onboarded" — the path "Skip for now" takes.
      const byRole: Record<Role, string[]> = {
        owner: [],
        admin: [],
        member: [],
      };
      for (const p of pending) byRole[p.role].push(p.email);
      for (const r of ["owner", "admin", "member"] as const) {
        if (byRole[r].length > 0) {
          await api.inviteMembers(byRole[r], r);
        }
      }
      await api.updateOrg({ onboarded: true });
    },
    onSuccess: () => onFinished(),
  });

  return (
    <form
      className="flex w-[520px] flex-col bg-[var(--color-surface-primary)] shadow-[0_12px_40px_rgba(30,51,34,0.12)] ring-1 ring-[var(--color-moss-deep)]"
      onSubmit={(e) => {
        e.preventDefault();
        if (!finish.isPending) finish.mutate();
      }}
      data-step="invite"
    >
      <div className="h-1.5 w-full bg-[var(--color-moss)]" aria-hidden="true" />

      {/* Head */}
      <div className="flex flex-col items-center gap-3 px-10 pt-8 pb-5">
        <div className="flex h-[52px] w-[52px] items-center justify-center bg-[var(--color-moss-soft)]">
          <UserPlus className="h-[25px] w-[25px] text-[var(--color-moss)]" />
        </div>
        <TitleWithMossPill prefix="Invite your" highlight="teammates" />
        <p className="max-w-[400px] text-center text-[14px] leading-[1.5] text-[var(--color-fg-secondary)]">
          Add people to collaborate with your agents. This is optional — you can
          do it anytime.
        </p>
      </div>

      {/* Body */}
      <div className="flex flex-col gap-3.5 px-10 py-2">
        <label
          htmlFor="onboarding-invite-email"
          className="text-[14px] font-medium text-[var(--color-moss-deep)]"
        >
          Invite by email
        </label>

        <div className="flex items-center gap-2">
          <div className="flex flex-1 items-center gap-2 bg-[var(--color-surface-primary)] px-3 py-3 ring-1 ring-[var(--color-moss-deep)]">
            <Mail className="h-[15px] w-[15px] text-[var(--color-fg-muted)]" />
            <input
              id="onboarding-invite-email"
              type="email"
              value={email}
              onChange={(e) => {
                setEmail(e.target.value);
                if (addError) setAddError(null);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  addPending();
                }
              }}
              placeholder="name@company.com"
              className="flex-1 bg-transparent text-[14px] text-[var(--color-moss-deep)] outline-none placeholder:text-[var(--color-fg-muted)]"
              data-testid="onboarding-invite-email"
              aria-invalid={addError !== null}
              aria-describedby={addError ? "onboarding-invite-email-error" : undefined}
            />
          </div>

          <div className="relative">
            <select
              value={role}
              onChange={(e) => setRole(e.target.value as Role)}
              className="appearance-none bg-[var(--color-surface-secondary)] px-3 py-3 pr-8 text-[13px] text-[var(--color-moss-deep)] outline-none ring-1 ring-[var(--color-border-subtle)]"
              data-testid="onboarding-invite-role"
            >
              <option value="member">Member</option>
              <option value="admin">Admin</option>
              <option value="owner">Owner</option>
            </select>
            <ChevronsUpDown className="pointer-events-none absolute top-1/2 right-3 h-3 w-3 -translate-y-1/2 text-[var(--color-fg-muted)]" />
          </div>

          <button
            type="button"
            onClick={addPending}
            disabled={!email.trim()}
            className="inline-flex items-center justify-center bg-[var(--color-moss)] px-4 py-3 text-[14px] font-semibold text-white transition-colors hover:bg-[var(--color-moss-deep)] disabled:cursor-not-allowed disabled:opacity-50"
            data-testid="onboarding-invite-add"
          >
            Add
          </button>
        </div>

        {addError && (
          <p
            id="onboarding-invite-email-error"
            role="alert"
            className="text-[12px] text-[var(--color-rose)]"
          >
            {addError}
          </p>
        )}

        {pending.length > 0 && (
          <ul
            className="flex flex-col ring-1 ring-[var(--color-border-subtle)]"
            data-testid="onboarding-invite-pending"
          >
            {pending.map((p, i) => (
              <li
                key={p.email}
                className={
                  "flex items-center gap-3 px-3.5 py-2.5 " +
                  (i < pending.length - 1
                    ? "border-b border-[var(--color-border-subtle)]"
                    : "")
                }
              >
                <span
                  className="flex h-8 w-8 items-center justify-center rounded-full bg-[var(--color-moss-soft)] font-[var(--font-display)] text-[12px] font-bold text-[var(--color-moss)]"
                  aria-hidden="true"
                >
                  {p.email[0]?.toUpperCase() ?? "?"}
                </span>
                <div className="flex min-w-0 flex-1 flex-col gap-px">
                  <div className="truncate text-[13px] font-medium text-[var(--color-moss-deep)]">
                    {p.email}
                  </div>
                </div>
                <span className="text-[12px] capitalize text-[var(--color-fg-secondary)]">
                  {p.role}
                </span>
                <button
                  type="button"
                  aria-label={`Remove ${p.email}`}
                  onClick={() => removePending(p.email)}
                  className="cursor-pointer p-1 text-[var(--color-fg-muted)] hover:text-[var(--color-rose)]"
                >
                  <X className="h-[15px] w-[15px]" />
                </button>
              </li>
            ))}
          </ul>
        )}

        {/* Share-link row */}
        <div className="flex items-center gap-2 bg-[var(--color-surface-secondary)] px-3.5 py-3 ring-1 ring-[var(--color-border-subtle)]">
          <Link2 className="h-[15px] w-[15px] text-[var(--color-fg-muted)]" />
          <div className="flex min-w-0 flex-1 flex-col gap-px">
            <div className="text-[13px] font-medium text-[var(--color-moss-deep)]">
              Anyone with the link can join
            </div>
            <div className="truncate font-[var(--font-mono)] text-[11px] text-[var(--color-fg-muted)]">
              {inviteLink}
            </div>
          </div>
          <button
            type="button"
            onClick={() => {
              // navigator.clipboard rejects in non-secure contexts and
              // when the permission was denied. The "Copied" affirmation
              // must only show if the write actually succeeded; on
              // failure we leave the UI as-is so the user can try again
              // or copy the text manually.
              navigator.clipboard
                .writeText(`https://${inviteLink}`)
                .then(() => {
                  setCopied(true);
                  window.setTimeout(() => setCopied(false), 1500);
                })
                .catch(() => {});
            }}
            className="inline-flex cursor-pointer items-center justify-center gap-1.5 bg-[var(--color-surface-primary)] px-3 py-2 text-[13px] font-medium text-[var(--color-moss-deep)] ring-1 ring-[var(--color-border-subtle)]"
            data-testid="onboarding-invite-copy"
          >
            {copied ? (
              <Check className="h-3 w-3" />
            ) : (
              <Copy className="h-3 w-3" />
            )}
            {copied ? "Copied" : "Copy"}
          </button>
        </div>

        {finish.isError && (
          <p
            className="text-[12px] text-[var(--color-rose)]"
            role="alert"
          >
            Couldn't send invites. Try again, or skip — you can invite from
            workspace settings later.
          </p>
        )}
      </div>

      {/* Foot */}
      <div className="flex items-center justify-between gap-3 px-10 pt-5 pb-7">
        <button
          type="button"
          onClick={() => {
            // Skip → drop any pending chips, then run the same mutation
            // (sends zero invites and just flips `onboarded`).
            setPending([]);
            finish.mutate();
          }}
          disabled={finish.isPending}
          className="inline-flex cursor-pointer items-center justify-center gap-1.5 px-3.5 py-3 text-[14px] font-medium text-[var(--color-fg-secondary)] hover:text-[var(--color-moss-deep)] disabled:cursor-not-allowed disabled:opacity-50"
          data-testid="onboarding-invite-skip"
        >
          Skip for now
        </button>
        <button
          type="submit"
          disabled={finish.isPending || pending.length === 0}
          className="inline-flex cursor-pointer items-center justify-center gap-2 bg-[var(--color-moss)] px-5 py-3 text-[14px] font-semibold text-white transition-colors hover:bg-[var(--color-moss-deep)] disabled:cursor-not-allowed disabled:opacity-50"
          data-testid="onboarding-invite-send"
        >
          {finish.isPending
            ? "Sending…"
            : `Send ${pending.length} invite${pending.length === 1 ? "" : "s"} & finish`}
          <ArrowRight className="h-[15px] w-[15px]" />
        </button>
      </div>
    </form>
  );
}

