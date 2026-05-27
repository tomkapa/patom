import { useEffect, useRef, type ReactNode } from "react";
import { AnimatePresence, motion } from "motion/react";
import { X } from "lucide-react";
import { useT } from "../../i18n";
import { modalMotion, scrimMotion } from "../../lib/motion";

const FOCUSABLE_SELECTOR =
  'a[href], area[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

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

  // Trap focus inside the dialog and restore the caller's focus on
  // close. Without this, Tab can land on background controls — a
  // standard a11y blocker for keyboard/screen-reader users.
  useEffect(() => {
    if (!open) return;
    const previousFocus = document.activeElement as HTMLElement | null;
    const dialog = dialogRef.current;
    // Move focus into the dialog on next paint so any auto-focused
    // child (e.g. inputs) wins over our fallback.
    if (dialog) {
      const first = dialog.querySelector<HTMLElement>(FOCUSABLE_SELECTOR);
      (first ?? dialog).focus({ preventScroll: true });
    }

    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        onClose();
        return;
      }
      if (e.key !== "Tab" || !dialog) return;
      const items = Array.from(
        dialog.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR),
      ).filter((el) => !el.hasAttribute("aria-hidden"));
      if (items.length === 0) {
        e.preventDefault();
        dialog.focus({ preventScroll: true });
        return;
      }
      const first = items[0]!;
      const last = items[items.length - 1]!;
      const active = document.activeElement as HTMLElement | null;
      if (e.shiftKey && (active === first || !dialog.contains(active))) {
        e.preventDefault();
        last.focus({ preventScroll: true });
      } else if (!e.shiftKey && active === last) {
        e.preventDefault();
        first.focus({ preventScroll: true });
      }
    };
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("keydown", onKey);
      previousFocus?.focus({ preventScroll: true });
    };
  }, [open, onClose]);

  useEffect(() => {
    if (!open) return;
    const prevOverflow = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.body.style.overflow = prevOverflow;
    };
  }, [open]);

  return (
    <AnimatePresence>
      {open ? (
        <motion.div
          role="dialog"
          aria-modal="true"
          aria-label={ariaLabel}
          {...scrimMotion}
          className="fixed inset-0 z-50 flex items-center justify-center bg-[#1E3322CC] p-8"
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
