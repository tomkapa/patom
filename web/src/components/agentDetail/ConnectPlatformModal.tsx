import { useEffect, useState } from "react";
import { Eye, EyeOff, Hash, Info, Lock, PlugZap, X } from "lucide-react";
import { Modal, ModalFooter } from "../molecules/Modal";
import { Monogram } from "../atoms/Monogram";
import { Banner } from "../molecules/Banner";
import { useT } from "../../i18n";
import type { Agent } from "../../types/api";

/** An optional masked field rendered below the secret — e.g. Lark's card-action
 *  Encrypt Key + Verification Token. Empty values are dropped, so they never
 *  reach the request body. */
export type ExtraField = {
  id: string;
  label: string;
  placeholder?: string;
  helper?: string;
};

/** Connect-a-bot modal shared by Lark and Discord. The two required fields (id +
 *  secret) carry platform-specific labels; `extraFields` adds optional masked
 *  inputs (Lark-only today). The agent is fixed to the page it opened from,
 *  shown read-only. The page maps `(idValue, secretValue, extra)` onto the right
 *  request body. */
export function ConnectPlatformModal({
  open,
  onClose,
  name,
  logo,
  agent,
  idLabel,
  secretLabel,
  idPlaceholder,
  hint,
  submitting,
  error,
  extraFields,
  onSubmit,
}: {
  open: boolean;
  onClose: () => void;
  name: string;
  logo: string;
  agent: Agent;
  idLabel: string;
  secretLabel: string;
  idPlaceholder: string;
  hint: string;
  submitting: boolean;
  error?: string | null;
  extraFields?: ExtraField[];
  onSubmit: (
    idValue: string,
    secretValue: string,
    extra: Record<string, string>,
  ) => void;
}) {
  const { t } = useT();
  const [idValue, setIdValue] = useState("");
  const [secretValue, setSecretValue] = useState("");
  const [reveal, setReveal] = useState(false);
  const [extra, setExtra] = useState<Record<string, string>>({});
  const [revealExtra, setRevealExtra] = useState<Record<string, boolean>>({});

  // Reset the form each time the modal opens so a prior platform's input
  // never bleeds into the next.
  useEffect(() => {
    if (open) {
      setIdValue("");
      setSecretValue("");
      setReveal(false);
      setExtra({});
      setRevealExtra({});
    }
  }, [open]);

  const canSubmit =
    idValue.trim().length > 0 && secretValue.trim().length > 0 && !submitting;

  const submit = () => {
    if (!canSubmit) return;
    // Drop blank optional fields so an empty string never reaches the body
    // (the backend treats absent ≠ "" — the latter fails validation).
    const cleaned: Record<string, string> = {};
    for (const [key, value] of Object.entries(extra)) {
      const trimmed = value.trim();
      if (trimmed) cleaned[key] = trimmed;
    }
    onSubmit(idValue.trim(), secretValue.trim(), cleaned);
  };

  return (
    <Modal
      open={open}
      onClose={onClose}
      width={460}
      ariaLabel={t("agent.detail.integrations.modal.title", { name })}
    >
      {/* Header */}
      <div className="flex items-center gap-3 border-b border-[var(--color-line)] px-6 pt-5 pb-4">
        <span
          aria-hidden
          className="flex h-10 w-10 shrink-0 items-center justify-center border border-[var(--color-line-2)] bg-[var(--color-paper-2)]"
        >
          <img src={logo} alt={name} className="h-5 w-5" />
        </span>
        <div className="min-w-0 flex-1">
          <div className="font-[var(--font-display)] text-[17px] leading-tight font-bold text-[var(--color-ink-2)]">
            {t("agent.detail.integrations.modal.title", { name })}
          </div>
          <div className="font-[var(--font-mono)] text-[11px] text-[var(--color-fg-muted)]">
            {t("agent.detail.integrations.modal.sub", { agent: agent.name })}
          </div>
        </div>
        <button
          type="button"
          aria-label={t("connections.modal.close")}
          onClick={onClose}
          className="-mt-1 -mr-1 shrink-0 cursor-pointer p-1 text-[var(--color-muted-foreground)] transition-colors duration-150 ease-out hover:text-[var(--color-ink)]"
        >
          <X className="h-4 w-4" />
        </button>
      </div>

      {/* Body */}
      <form
        id="connect-platform-form"
        onSubmit={(e) => {
          e.preventDefault();
          submit();
        }}
        className="flex flex-col gap-4 px-6 py-6"
      >
        <div className="flex items-start gap-2.5">
          <Info
            className="mt-0.5 h-4 w-4 shrink-0 text-[var(--color-moss)]"
            strokeWidth={1.75}
            aria-hidden
          />
          <p className="text-[13px] leading-[1.45] text-[var(--color-muted-foreground)]">
            {hint}
          </p>
        </div>

        {error ? <Banner variant="rose">{error}</Banner> : null}

        {/* App / Application ID */}
        <Field label={idLabel}>
          <FieldShell>
            <input
              value={idValue}
              onChange={(e) => setIdValue(e.target.value)}
              placeholder={idPlaceholder}
              autoFocus
              data-testid="connect-id"
              className="min-w-0 flex-1 bg-transparent font-[var(--font-mono)] text-[13px] text-[var(--color-ink)] outline-none placeholder:text-[var(--color-fg-muted)]"
            />
            <Hash
              className="h-3.5 w-3.5 shrink-0 text-[var(--color-fg-muted)]"
              strokeWidth={1.75}
            />
          </FieldShell>
        </Field>

        {/* Bot Token / App Secret */}
        <Field label={secretLabel}>
          <FieldShell>
            <input
              type={reveal ? "text" : "password"}
              value={secretValue}
              onChange={(e) => setSecretValue(e.target.value)}
              placeholder="••••••••••••••••••••••••••••••••"
              data-testid="connect-secret"
              className="min-w-0 flex-1 bg-transparent font-[var(--font-mono)] text-[13px] text-[var(--color-ink)] outline-none placeholder:text-[var(--color-fg-muted)]"
            />
            <button
              type="button"
              aria-label={t("agent.detail.integrations.modal.secretToggle")}
              onClick={() => setReveal((r) => !r)}
              className="shrink-0 cursor-pointer text-[var(--color-fg-muted)] transition-colors duration-150 ease-out hover:text-[var(--color-ink)]"
            >
              {reveal ? (
                <Eye className="h-3.5 w-3.5" strokeWidth={1.75} />
              ) : (
                <EyeOff className="h-3.5 w-3.5" strokeWidth={1.75} />
              )}
            </button>
          </FieldShell>
          <p className="font-[var(--font-mono)] text-[11px] text-[var(--color-fg-muted)]">
            {t("agent.detail.integrations.modal.secretHelper")}
          </p>
        </Field>

        {/* Optional extra secrets (Lark card-action credentials) */}
        {extraFields?.map((field) => (
          <Field key={field.id} label={field.label}>
            <FieldShell>
              <input
                type={revealExtra[field.id] ? "text" : "password"}
                value={extra[field.id] ?? ""}
                onChange={(e) =>
                  setExtra((prev) => ({ ...prev, [field.id]: e.target.value }))
                }
                placeholder={field.placeholder ?? "••••••••••••••••"}
                data-testid={`connect-extra-${field.id}`}
                className="min-w-0 flex-1 bg-transparent font-[var(--font-mono)] text-[13px] text-[var(--color-ink)] outline-none placeholder:text-[var(--color-fg-muted)]"
              />
              <button
                type="button"
                aria-label={t("agent.detail.integrations.modal.secretToggle")}
                onClick={() =>
                  setRevealExtra((prev) => ({
                    ...prev,
                    [field.id]: !prev[field.id],
                  }))
                }
                className="shrink-0 cursor-pointer text-[var(--color-fg-muted)] transition-colors duration-150 ease-out hover:text-[var(--color-ink)]"
              >
                {revealExtra[field.id] ? (
                  <Eye className="h-3.5 w-3.5" strokeWidth={1.75} />
                ) : (
                  <EyeOff className="h-3.5 w-3.5" strokeWidth={1.75} />
                )}
              </button>
            </FieldShell>
            {field.helper ? (
              <p className="font-[var(--font-mono)] text-[11px] text-[var(--color-fg-muted)]">
                {field.helper}
              </p>
            ) : null}
          </Field>
        ))}

        {/* Agent — locked to the current page */}
        <Field label={t("agent.detail.integrations.modal.agentLabel")}>
          <div className="flex items-center justify-between gap-2 border border-[var(--color-line-2)] bg-[var(--color-paper-2)] px-3 py-2.5">
            <span className="flex min-w-0 items-center gap-2">
              <Monogram
                name={agent.name}
                id={agent.id}
                avatarUrl={agent.avatar_url ?? undefined}
                size={20}
                tone="moss"
              />
              <span className="truncate text-[13px] font-medium text-[var(--color-ink-2)]">
                {agent.name}
              </span>
            </span>
            <Lock
              className="h-3.5 w-3.5 shrink-0 text-[var(--color-fg-muted)]"
              strokeWidth={1.75}
            />
          </div>
        </Field>
      </form>

      {/* Footer */}
      <ModalFooter>
        <button
          type="button"
          onClick={onClose}
          className="inline-flex cursor-pointer items-center px-4 py-2.5 text-[13px] font-medium text-[var(--color-muted-foreground)] transition-colors duration-150 ease-out hover:text-[var(--color-ink)]"
        >
          {t("agent.detail.integrations.modal.cancel")}
        </button>
        <button
          type="button"
          onClick={submit}
          disabled={!canSubmit}
          aria-busy={submitting || undefined}
          data-testid="connect-submit"
          className="inline-flex cursor-pointer items-center gap-2 bg-[var(--color-moss)] px-4 py-2.5 text-[13px] font-semibold text-white transition-colors duration-150 ease-out hover:bg-[var(--color-moss-deep)] disabled:cursor-not-allowed disabled:opacity-50"
        >
          <PlugZap className="h-3.5 w-3.5" strokeWidth={1.75} />
          {t("agent.detail.integrations.modal.connect")}
        </button>
      </ModalFooter>
    </Modal>
  );
}

function Field({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <label className="flex flex-col gap-1.5">
      <span className="text-[13px] font-medium text-[var(--color-ink-2)]">
        {label}
      </span>
      {children}
    </label>
  );
}

function FieldShell({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex items-center gap-2 border border-[var(--color-line-strong)] bg-[var(--color-card)] px-3 py-[11px] focus-within:ring-1 focus-within:ring-[var(--color-moss)]">
      {children}
    </div>
  );
}
