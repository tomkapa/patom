import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import { AnimatePresence, motion } from "motion/react";
import { useDismissable } from "../../hooks/useDismissable";
import { popoverMotion } from "../../lib/motion";
import { cn } from "../../lib/utils";

/** Pop-over anchor relative to the trigger. The default `bottom-start`
 *  drops below the trigger, left-aligned — covers Select, the sidebar
 *  switchers, and the user-menu anchor. */
export type DropdownPlacement =
  | "bottom-start"
  | "bottom-stretch"
  | "right-bottom"
  | "right-top";

export type DropdownState = {
  /** Whether the popover is currently shown. */
  open: boolean;
  /** Imperative close — useful from async pick handlers that want to
   *  defer the close until a network round-trip settles. */
  close: () => void;
  /** Imperative toggle. Triggers usually wire this to `onClick`. */
  toggle: () => void;
};

type Coords = { top: number; left: number; width?: number };

// Space between trigger edge and menu, and between menu and viewport edge.
// Kept small so the popover still feels "rooted" to the trigger.
const TRIGGER_GAP = 4;
const VIEWPORT_MARGIN = 8;

/** Generic open/close + click-outside + Escape primitive with
 *  viewport-aware positioning. The trigger and the menu body are
 *  caller-supplied — Dropdown only owns visibility state, dismissable
 *  wiring, and the popover position math. Built so `Select` and the
 *  per-domain switchers (AgentSwitcher, OrgSwitcher, future menus) share
 *  one source of truth for behaviour instead of three almost-identical
 *  hand-rolls.
 *
 *  The menu is rendered with `position: fixed` and measured on open so
 *  it flips/clamps when it would otherwise overflow the viewport. This
 *  is why long Select / Columns dropdowns near the right or bottom edge
 *  of the window don't render clipped off-screen. */
export function Dropdown({
  renderTrigger,
  children,
  placement = "bottom-stretch",
  menuClassName,
  rootClassName,
}: {
  renderTrigger: (state: DropdownState) => ReactNode;
  children: (state: DropdownState) => ReactNode;
  placement?: DropdownPlacement;
  /** Extra classes on the popover container. Callers usually override
   *  width, max-height, or scroll behaviour here. */
  menuClassName?: string;
  /** Extra classes on the positioned wrapper. */
  rootClassName?: string;
}) {
  const [open, setOpen] = useState(false);
  const [coords, setCoords] = useState<Coords | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const close = useCallback(() => {
    setOpen(false);
    setCoords(null);
  }, []);
  const toggle = useCallback(() => setOpen((v) => !v), []);
  useDismissable(rootRef, open, close);

  const reposition = useCallback(() => {
    const root = rootRef.current;
    const menu = menuRef.current;
    if (!root || !menu) return;

    // Measure the trigger element directly so a block-level `relative`
    // wrapper around it doesn't inflate the anchor width. The menu is
    // `position: fixed`, so it's out of flow and doesn't contribute to
    // firstChild's box either way.
    const triggerEl = (root.firstElementChild as HTMLElement | null) ?? root;
    const tRect = triggerEl.getBoundingClientRect();
    const mRect = menu.getBoundingClientRect();
    const vw = window.innerWidth;
    const vh = window.innerHeight;

    let top: number;
    let left: number;
    let width: number | undefined;
    switch (placement) {
      case "bottom-stretch":
        top = tRect.bottom + TRIGGER_GAP;
        left = tRect.left;
        width = tRect.width;
        break;
      case "right-bottom":
        // Anchored to the right edge of the trigger, baseline-aligned
        // at the trigger's bottom edge (menu rail's UserMenu).
        top = tRect.bottom - mRect.height;
        left = tRect.right + TRIGGER_GAP * 2;
        break;
      case "right-top":
        // Anchored to the right edge of the trigger, top-aligned with it
        // — drops downward (menu rail's workspace avatar at the top).
        top = tRect.top;
        left = tRect.right + TRIGGER_GAP * 2;
        break;
      case "bottom-start":
      default:
        top = tRect.bottom + TRIGGER_GAP;
        left = tRect.left;
        break;
    }

    // Vertical: flip above the trigger if the menu would overflow the
    // bottom of the viewport. If it doesn't fit either way, clamp.
    if (top + mRect.height > vh - VIEWPORT_MARGIN) {
      const above = tRect.top - mRect.height - TRIGGER_GAP;
      top =
        above >= VIEWPORT_MARGIN
          ? above
          : Math.max(VIEWPORT_MARGIN, vh - mRect.height - VIEWPORT_MARGIN);
    }
    if (top < VIEWPORT_MARGIN) top = VIEWPORT_MARGIN;

    // Horizontal: if the menu overflows the right edge, try aligning
    // its right edge to the trigger's right edge (standard flip). If
    // that still doesn't fit, clamp to viewport.
    const effW = width ?? mRect.width;
    if (left + effW > vw - VIEWPORT_MARGIN) {
      const flipped = tRect.right - effW;
      left =
        flipped >= VIEWPORT_MARGIN
          ? flipped
          : Math.max(VIEWPORT_MARGIN, vw - effW - VIEWPORT_MARGIN);
    }
    if (left < VIEWPORT_MARGIN) left = VIEWPORT_MARGIN;

    setCoords({ top, left, width });
  }, [placement]);

  // Measure on open before paint. `popoverMotion.initial.opacity = 0`
  // keeps the menu invisible during measurement, and the setCoords
  // commit completes before the browser paints — no wrong-position flash.
  useLayoutEffect(() => {
    if (!open) return;
    reposition();
  }, [open, reposition]);

  // Reposition while open if the page scrolls or the window resizes.
  // Capture mode picks up nested scrollable ancestors moving the trigger.
  useEffect(() => {
    if (!open) return;
    window.addEventListener("resize", reposition);
    window.addEventListener("scroll", reposition, true);
    return () => {
      window.removeEventListener("resize", reposition);
      window.removeEventListener("scroll", reposition, true);
    };
  }, [open, reposition]);

  const state: DropdownState = { open, close, toggle };

  return (
    <div ref={rootRef} className={cn("relative", rootClassName)}>
      {renderTrigger(state)}
      <AnimatePresence>
        {open ? (
          <motion.div
            ref={menuRef}
            {...popoverMotion}
            style={{
              position: "fixed",
              top: coords?.top ?? 0,
              left: coords?.left ?? 0,
              width: coords?.width,
              transformOrigin: ORIGIN[placement],
            }}
            className={cn("z-20", menuClassName)}
          >
            {children(state)}
          </motion.div>
        ) : null}
      </AnimatePresence>
    </div>
  );
}

const ORIGIN: Record<DropdownPlacement, string> = {
  "bottom-start": "top left",
  "bottom-stretch": "top center",
  "right-bottom": "bottom left",
  "right-top": "top left",
};
