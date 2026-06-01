import { useRef, type ReactNode } from "react";
import { AnimatePresence, motion } from "motion/react";
import { X } from "lucide-react";
import { useT } from "../../i18n";
import { modalMotion, scrimMotion } from "../../lib/motion";
import { useOverlayA11y } from "../../hooks/useOverlayA11y";

// Scrim is `--color-rail` at 80% alpha (matches the design frames).
export function Modal({
  open,
  onClose,
  children,
  width = 460,
  ariaLabel,
  fill = false,
}: {
  open: boolean;
  onClose: () => void;
  children: ReactNode;
  width?: number;
  ariaLabel?: string;
  /** When true, the modal box fills the viewport height and lays out as
   *  a flex column without an outer scrollbar — children control their
   *  own scroll regions. Use for modals with a fixed header/footer and a
   *  single scrollable body (e.g. the prompt diff). */
  fill?: boolean;
}) {
  const dialogRef = useRef<HTMLDivElement | null>(null);
  useOverlayA11y(dialogRef, open, onClose);

  return (
    <AnimatePresence>
      {open ? (
        <motion.div
          role="dialog"
          aria-modal="true"
          aria-label={ariaLabel}
          {...scrimMotion}
          className="fixed inset-0 z-50 flex items-center justify-center bg-[var(--color-rail)]/80 p-8"
          onMouseDown={(e) => {
            // Close on backdrop click only (not when dragging from inside).
            if (e.target === e.currentTarget) onClose();
          }}
        >
          <motion.div
            ref={dialogRef}
            tabIndex={-1}
            {...modalMotion}
            className={
              fill
                ? "flex h-[calc(100vh-64px)] w-full flex-col border border-[var(--color-line)] bg-[var(--color-card)] shadow-xl focus:outline-none"
                : "max-h-[calc(100vh-64px)] w-full overflow-auto border border-[var(--color-line)] bg-[var(--color-card)] shadow-xl focus:outline-none"
            }
            style={{ maxWidth: width }}
          >
            {children}
          </motion.div>
        </motion.div>
      ) : null}
    </AnimatePresence>
  );
}

export function ModalHeader({
  eyebrow,
  title,
  icon,
  onClose,
}: {
  eyebrow: ReactNode;
  title: ReactNode;
  icon?: ReactNode;
  onClose: () => void;
}) {
  const { t } = useT();
  return (
    <div className="flex items-start gap-3 border-b border-[var(--color-line)] px-5 pt-5 pb-4">
      {icon ? <div className="shrink-0 pt-0.5">{icon}</div> : null}
      <div className="min-w-0 flex-1">
        <div className="font-[var(--font-mono)] text-[10px] tracking-[0.14em] text-[var(--color-muted-foreground)] uppercase">
          {eyebrow}
        </div>
        <div className="mt-1 font-[var(--font-display)] text-[18px] leading-tight font-semibold text-[var(--color-ink)]">
          {title}
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
  );
}

export function ModalFooter({
  left,
  children,
}: {
  left?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="flex shrink-0 items-center justify-between gap-3 border-t border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-3">
      <div className="flex min-w-0 items-center gap-2">{left}</div>
      <div className="flex shrink-0 items-center gap-2">{children}</div>
    </div>
  );
}
