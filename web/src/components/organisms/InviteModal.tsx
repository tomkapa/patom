import { useEffect, useState } from "react";
import { Crown, Link2, Send, Shield, User as UserIcon } from "lucide-react";
import { Button } from "../atoms/Button";
import { Modal, ModalFooter, ModalHeader } from "../molecules/Modal";
import { EmailChipsInput, type EmailChip } from "../core/EmailChipsInput";
import { RadioCards } from "../core/RadioCards";
import { useInviteMembers } from "../../hooks/useOrg";
import { useT } from "../../i18n";
import type { Role } from "../../types/api";

export function InviteModal({
  open,
  onClose,
  orgName,
  orgSlug,
  callerRole,
}: {
  open: boolean;
  onClose: () => void;
  orgName: string;
  orgSlug: string;
  callerRole: Role;
}) {
  const { t } = useT();
  const invite = useInviteMembers();
  const [chips, setChips] = useState<EmailChip[]>([]);
  const [role, setRole] = useState<Role>("member");
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** Cleartext invite token, populated after the most recent send.
   *  Used to build the share link below. Until the first send the
   *  copy button is disabled — copying a placeholder would mint a
   *  dead link. */
  const [latestToken, setLatestToken] = useState<string | null>(null);

  // Reset on open so the modal is clean each invocation.
  useEffect(() => {
    if (!open) {
      setChips([]);
      setRole("member");
      setCopied(false);
      setError(null);
      setLatestToken(null);
    }
  }, [open]);

  const validChips = chips.filter((c) => !c.invalid);
  const canSend = validChips.length > 0 && !invite.isPending;

  const shareLink = latestToken
    ? `patom.app/i/${orgSlug}/${latestToken}`
    : null;

  const onSend = async () => {
    setError(null);
    try {
      const issued = await invite.mutateAsync({
        emails: validChips.map((c) => c.value),
        role,
      });
      // Stash the most recent issued token so the share-link button
      // resolves to a real, redeemable URL.
      const first = issued[0];
      if (first) setLatestToken(first.token);
      onClose();
    } catch (e) {
      setError((e as Error).message);
    }
  };

  const roleOptions = [
    {
      value: "member" as Role,
      label: t("settings.members.role.member"),
      description: t("settings.invite.role.member.desc"),
      icon: UserIcon,
    },
    {
      value: "admin" as Role,
      label: t("settings.members.role.admin"),
      description: t("settings.invite.role.admin.desc"),
      icon: Shield,
    },
    {
      value: "owner" as Role,
      label: t("settings.members.role.owner"),
      description: t("settings.invite.role.owner.desc"),
      icon: Crown,
    },
  ];

  // Non-owners can't grant owner.
  const allowedRoleOptions =
    callerRole === "owner" ? roleOptions : roleOptions.slice(0, 2);

  return (
    <Modal open={open} onClose={onClose} ariaLabel="Invite members" width={460}>
      <ModalHeader
        eyebrow={t("settings.invite.eyebrow")}
        title={t("settings.invite.title", { org: orgName })}
        onClose={onClose}
      />
      <div className="px-5 py-4">
        <p className="mb-4 text-[12.5px] text-[var(--color-muted-foreground)]">
          {t("settings.invite.subtitle")}
        </p>

        <div className="mb-4">
          <div className="mb-1.5 flex items-center justify-between font-[var(--font-mono)] text-[10.5px] tracking-[0.06em] text-[var(--color-muted-foreground)] uppercase">
            <span>{t("settings.invite.emails")}</span>
            <span>{t("settings.invite.emails.helper")}</span>
          </div>
          <EmailChipsInput
            chips={chips}
            onChange={setChips}
            placeholder="name@company.com"
          />
        </div>

        <div className="mb-4">
          <div className="mb-1.5 font-[var(--font-mono)] text-[10.5px] tracking-[0.06em] text-[var(--color-muted-foreground)] uppercase">
            {t("settings.invite.role")}
          </div>
          <RadioCards
            options={allowedRoleOptions}
            value={role}
            onChange={setRole}
            name="invite-role"
            ariaLabel="Role"
          />
        </div>

        <div className="border border-[var(--color-line)] bg-[var(--color-paper-2)] px-3 py-2">
          <div className="flex items-center justify-between gap-2">
            <div className="flex min-w-0 items-center gap-2">
              <Link2
                className="h-3.5 w-3.5 shrink-0 text-[var(--color-muted-foreground)]"
                strokeWidth={1.75}
              />
              <div className="min-w-0 truncate font-[var(--font-mono)] text-[12px] text-[var(--color-ink)]">
                {shareLink ?? `patom.app/i/${orgSlug}/…`}
              </div>
            </div>
            <button
              type="button"
              data-testid="invite-copy-link"
              disabled={!shareLink}
              onClick={async () => {
                if (!shareLink) return;
                // Both paths schedule the same 1.2s reset so the
                // button label can't get wedged on "Copied" if the
                // clipboard API is unavailable (some test envs).
                const finish = () => {
                  setCopied(true);
                  window.setTimeout(() => setCopied(false), 1200);
                };
                try {
                  await navigator.clipboard.writeText(shareLink);
                  finish();
                } catch {
                  finish();
                }
              }}
              className="inline-flex h-7 cursor-pointer items-center border border-[var(--color-line)] bg-[var(--color-card)] px-2.5 font-[var(--font-mono)] text-[10.5px] uppercase tracking-[0.06em] text-[var(--color-ink)] hover:bg-[var(--color-paper)] disabled:cursor-not-allowed disabled:opacity-50"
            >
              {copied
                ? t("settings.invite.link.copied")
                : t("settings.invite.link.copy")}
            </button>
          </div>
          <div className="mt-1 font-[var(--font-mono)] text-[10.5px] text-[var(--color-muted-foreground)]">
            {t("settings.invite.link.helper")}
          </div>
        </div>

        {error ? (
          <div className="mt-3 border border-[var(--color-rose)] bg-[var(--color-rose-soft)] px-3 py-1.5 text-[11.5px] text-[var(--color-rose)]">
            {error}
          </div>
        ) : null}
      </div>
      <ModalFooter
        left={
          <span
            className="font-[var(--font-mono)] text-[11px] tracking-[0.04em] text-[var(--color-muted-foreground)]"
            data-testid="invite-counter"
          >
            {validChips.length === 1
              ? t("settings.invite.count.one")
              : t("settings.invite.count.many", { n: validChips.length })}
          </span>
        }
      >
        <Button variant="secondary" onClick={onClose}>
          {t("settings.invite.cancel")}
        </Button>
        <Button
          variant="primary"
          disabled={!canSend}
          loading={invite.isPending}
          onClick={onSend}
          data-testid="invite-send"
        >
          <Send className="h-3.5 w-3.5" strokeWidth={2} />
          {t("settings.invite.send")}
        </Button>
      </ModalFooter>
    </Modal>
  );
}
