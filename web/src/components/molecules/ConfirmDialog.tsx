import type { ReactNode } from "react";
import { TriangleAlert } from "lucide-react";
import { Button } from "../atoms/Button";
import { Modal, ModalFooter } from "./Modal";

/**
 * App-styled replacement for `window.confirm`. Renders title + body in the
 * shared {@link Modal} (scrim, Escape-to-close, focus trap) with a
 * cancel/confirm button pair. `tone="danger"` flags a destructive action
 * with the alert glyph and a rose confirm button.
 */
export function ConfirmDialog({
  open,
  title,
  body,
  confirmLabel,
  cancelLabel,
  onConfirm,
  onCancel,
  tone = "danger",
  confirmBusy = false,
}: {
  open: boolean;
  title: ReactNode;
  body: ReactNode;
  confirmLabel: ReactNode;
  cancelLabel: ReactNode;
  onConfirm: () => void;
  onCancel: () => void;
  tone?: "danger" | "default";
  confirmBusy?: boolean;
}) {
  const isDanger = tone === "danger";
  return (
    <Modal open={open} onClose={onCancel} width={420} ariaLabel={String(title)}>
      <div className="flex items-start gap-3 px-5 pt-5 pb-4">
        {isDanger ? (
          <TriangleAlert
            className="mt-0.5 h-5 w-5 shrink-0 text-[var(--color-rose)]"
            strokeWidth={2}
            aria-hidden
          />
        ) : null}
        <div className="min-w-0 flex-1">
          <div className="font-[var(--font-display)] text-[18px] leading-tight font-semibold text-[var(--color-ink)]">
            {title}
          </div>
          <p className="mt-1.5 text-[13px] leading-snug text-[var(--color-muted-foreground)]">
            {body}
          </p>
        </div>
      </div>
      <ModalFooter>
        <Button variant="ghost" onClick={onCancel}>
          {cancelLabel}
        </Button>
        <Button
          variant={isDanger ? "moss" : "primary"}
          onClick={onConfirm}
          loading={confirmBusy}
          className={
            isDanger
              ? "!bg-[var(--color-rose)] !border-[var(--color-rose)] hover:!bg-[var(--color-rose-soft)] hover:!text-[var(--color-rose)]"
              : undefined
          }
        >
          {confirmLabel}
        </Button>
      </ModalFooter>
    </Modal>
  );
}
