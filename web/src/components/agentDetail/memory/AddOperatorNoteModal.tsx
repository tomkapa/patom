import { useState } from "react";
import { Brain, X } from "lucide-react";
import { Modal } from "../../molecules/Modal";
import { Select, type SelectOption } from "../../molecules/Select";
import { Switch } from "../../atoms/Switch";
import { Button } from "../../atoms/Button";
import { useCreateMemoryNote } from "../../../hooks/useAgentMemory";
import { useT } from "../../../i18n";
import { formatError } from "../../../lib/errors";
import type { MemoryKind, MemoryState } from "../../../types/api";

const CONTENT_MAX = 4096;

const KIND_VALUES: readonly MemoryKind[] = [
  "self",
  "other",
  "collaborator",
  "procedure",
  "open",
] as const;

const STATE_VALUES: readonly MemoryState[] = [
  "tentative",
  "held",
  "validated",
  "core",
] as const;

export function AddOperatorNoteModal({
  agentId,
  open,
  onClose,
}: {
  agentId: string;
  open: boolean;
  onClose: () => void;
}) {
  const { t } = useT();
  const create = useCreateMemoryNote();

  const [kind, setKind] = useState<MemoryKind>("self");
  const [state, setState] = useState<MemoryState>("held");
  const [content, setContent] = useState("");
  const [pinned, setPinned] = useState(false);
  const [submitError, setSubmitError] = useState<unknown>(null);

  const reset = () => {
    setKind("self");
    setState("held");
    setContent("");
    setPinned(false);
    setSubmitError(null);
    create.reset();
  };

  const close = () => {
    if (create.isPending) return;
    reset();
    onClose();
  };

  const len = content.length;
  const overLimit = len > CONTENT_MAX;
  const empty = content.trim().length === 0;
  const canSubmit = !empty && !overLimit && !create.isPending;

  const kindOptions: SelectOption<MemoryKind>[] = KIND_VALUES.map((v) => ({
    value: v,
    label: t(`agent.detail.memory.kind.${v}` as const),
  }));
  const stateOptions: SelectOption<MemoryState>[] = STATE_VALUES.map((v) => ({
    value: v,
    label: t(`agent.detail.memory.state.${v}` as const),
  }));

  const onSubmit = async () => {
    if (!canSubmit) return;
    setSubmitError(null);
    try {
      await create.mutateAsync({
        agentId,
        input: { kind, content, state, pinned },
      });
      reset();
      onClose();
    } catch (e) {
      setSubmitError(e);
    }
  };

  return (
    <Modal
      open={open}
      onClose={close}
      width={480}
      ariaLabel={t("agent.detail.memory.modal.title")}
    >
      <div className="flex items-start justify-between gap-3 border-b border-[var(--color-line)] px-6 py-5">
        <div className="flex flex-col gap-0.5">
          <h2 className="font-[var(--font-display)] text-[20px] font-bold text-[var(--color-ink)]">
            {t("agent.detail.memory.modal.title")}
          </h2>
          <p className="text-[13px] text-[var(--color-muted-foreground)]">
            {t("agent.detail.memory.modal.subtitle")}
          </p>
        </div>
        <button
          type="button"
          aria-label={t("agent.detail.memory.modal.cancel")}
          onClick={close}
          className="shrink-0 border border-[var(--color-line)] p-1.5 text-[var(--color-muted-foreground)] hover:text-[var(--color-ink)]"
        >
          <X className="h-3.5 w-3.5" strokeWidth={1.75} />
        </button>
      </div>

      <div className="flex flex-col gap-5 p-6">
        <div className="grid grid-cols-2 gap-4">
          <Field label={t("agent.detail.memory.modal.kindLabel")}>
            <Select<MemoryKind>
              value={kind}
              options={kindOptions}
              onChange={setKind}
              ariaLabel={t("agent.detail.memory.modal.kindLabel")}
            />
          </Field>
          <Field label={t("agent.detail.memory.modal.stateLabel")}>
            <Select<MemoryState>
              value={state}
              options={stateOptions}
              onChange={setState}
              ariaLabel={t("agent.detail.memory.modal.stateLabel")}
            />
          </Field>
        </div>

        <div className="flex flex-col gap-1.5">
          <div className="flex items-center gap-2">
            <span className="text-[13px] font-medium text-[var(--color-ink)]">
              {t("agent.detail.memory.modal.contentLabel")}
            </span>
            <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)]">
              {t("agent.detail.memory.modal.contentMax")}
            </span>
          </div>
          <textarea
            value={content}
            onChange={(e) => setContent(e.target.value)}
            placeholder={t("agent.detail.memory.modal.contentPlaceholder")}
            rows={6}
            className="min-h-[132px] resize-y border border-[var(--color-line-strong)] bg-[var(--color-card)] px-3 py-2.5 text-[13px] leading-[1.6] text-[var(--color-ink)] outline-none placeholder:text-[var(--color-muted-foreground)] focus:ring-1 focus:ring-[var(--color-ink)]"
          />
          <div className="flex justify-end">
            <span
              className={
                overLimit
                  ? "font-[var(--font-mono)] text-[11px] text-[var(--color-rose)]"
                  : "font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)]"
              }
            >
              {t("agent.detail.memory.modal.counter", {
                used: len,
                max: CONTENT_MAX,
              })}
            </span>
          </div>
        </div>

        <div className="flex items-center justify-between border border-[var(--color-line)] bg-[var(--color-paper-2)] px-4 py-3">
          <div className="flex flex-col gap-0.5">
            <span className="text-[13px] font-medium text-[var(--color-ink)]">
              {t("agent.detail.memory.modal.pinLabel")}
            </span>
            <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)]">
              {t("agent.detail.memory.modal.pinCaption")}
            </span>
          </div>
          <Switch
            checked={pinned}
            onChange={setPinned}
            ariaLabel={t("agent.detail.memory.modal.pinLabel")}
          />
        </div>

        {submitError ? (
          <div className="border border-[var(--color-rose)] bg-[var(--color-rose-soft)] px-3 py-2 text-[12.5px] text-[var(--color-rose)]">
            {formatError(submitError)}
          </div>
        ) : null}
      </div>

      <div className="flex items-center justify-end gap-2 border-t border-[var(--color-line)] bg-[var(--color-paper-2)] px-6 py-4">
        <Button variant="ghost" size="md" onClick={close}>
          {t("agent.detail.memory.modal.cancel")}
        </Button>
        <Button
          variant="primary"
          size="md"
          disabled={!canSubmit}
          loading={create.isPending}
          onClick={onSubmit}
        >
          <span className="inline-flex items-center gap-1.5">
            <Brain className="h-3.5 w-3.5" strokeWidth={1.75} />
            {create.isPending
              ? t("agent.detail.memory.modal.submitting")
              : t("agent.detail.memory.modal.submit")}
          </span>
        </Button>
      </div>
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
    <div className="flex flex-col gap-1.5">
      <span className="text-[13px] font-medium text-[var(--color-ink)]">
        {label}
      </span>
      {children}
    </div>
  );
}
