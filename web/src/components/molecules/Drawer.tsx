import { useRef, type ReactNode } from "react";
import { AnimatePresence, motion } from "motion/react";
import { drawerLeftMotion, drawerRightMotion, scrimMotion } from "../../lib/motion";
import { useOverlayA11y } from "../../hooks/useOverlayA11y";

/**
 * Off-canvas panel for compact viewports. Shares `Modal`'s a11y
 * machinery — scrim, focus trap, Escape, body-scroll lock, backdrop
 * dismiss — but slides in from a side instead of scaling in centred.
 *
 * `closeOnInnerActivate` (used by the left nav drawer) dismisses the
 * drawer when a tap lands on any inner `<a>`/`<button>`, so picking a
 * channel, agent, or route closes the menu without each item wiring up
 * its own `onClose`.
 */
export function Drawer({
  open,
  onClose,
  side = "left",
  panelClassName = "",
  ariaLabel,
  closeOnInnerActivate = false,
  children,
}: {
  open: boolean;
  onClose: () => void;
  side?: "left" | "right";
  /** Sizing/positioning for the sliding panel (width, bg already set). */
  panelClassName?: string;
  ariaLabel?: string;
  closeOnInnerActivate?: boolean;
  children: ReactNode;
}) {
  const panelRef = useRef<HTMLDivElement | null>(null);
  useOverlayA11y(panelRef, open, onClose);

  const slide = side === "left" ? drawerLeftMotion : drawerRightMotion;

  return (
    <AnimatePresence>
      {open ? (
        <motion.div
          role="dialog"
          aria-modal="true"
          aria-label={ariaLabel}
          {...scrimMotion}
          className="fixed inset-0 z-50 flex bg-[var(--color-rail)]/80"
          style={{ justifyContent: side === "left" ? "flex-start" : "flex-end" }}
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) onClose();
          }}
        >
          <motion.div
            ref={panelRef}
            tabIndex={-1}
            {...slide}
            className={`flex h-full flex-col overflow-hidden focus:outline-none ${panelClassName}`}
            onClick={
              closeOnInnerActivate
                ? (e) => {
                    if ((e.target as HTMLElement).closest("a,button")) onClose();
                  }
                : undefined
            }
          >
            {children}
          </motion.div>
        </motion.div>
      ) : null}
    </AnimatePresence>
  );
}
