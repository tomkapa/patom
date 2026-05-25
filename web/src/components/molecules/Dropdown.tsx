import { useCallback, useRef, useState } from "react";
import type { ReactNode } from "react";
import { useDismissable } from "../../hooks/useDismissable";
import { cn } from "../../lib/utils";

/** Pop-over anchor relative to the trigger. The default `bottom-start`
 *  drops below the trigger, left-aligned — covers Select, the sidebar
 *  switchers, and the user-menu anchor. */
export type DropdownPlacement = "bottom-start" | "bottom-stretch" | "right-bottom";

export type DropdownState = {
  /** Whether the popover is currently shown. */
  open: boolean;
  /** Imperative close — useful from async pick handlers that want to
   *  defer the close until a network round-trip settles. */
  close: () => void;
  /** Imperative toggle. Triggers usually wire this to `onClick`. */
  toggle: () => void;
};

/** Generic open/close + click-outside + Escape primitive. The trigger
 *  and the menu body are caller-supplied — Dropdown only owns the
 *  visibility state, the dismissable wiring, and the popover
 *  positioning. Built so `Select` and the per-domain switchers
 *  (AgentSwitcher, OrgSwitcher, future menus) share one source of truth
 *  for behaviour instead of three almost-identical hand-rolls. */
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
  const rootRef = useRef<HTMLDivElement>(null);
  const close = useCallback(() => setOpen(false), []);
  const toggle = useCallback(() => setOpen((v) => !v), []);
  useDismissable(rootRef, open, close);

  const state: DropdownState = { open, close, toggle };

  return (
    <div ref={rootRef} className={cn("relative", rootClassName)}>
      {renderTrigger(state)}
      {open ? (
        <div className={cn(PLACEMENT[placement], "z-20", menuClassName)}>
          {children(state)}
        </div>
      ) : null}
    </div>
  );
}

const PLACEMENT: Record<DropdownPlacement, string> = {
  // Drops below the trigger, left-aligned, natural width.
  "bottom-start": "absolute top-full left-0 mt-1",
  // Drops below the trigger, stretched to its width (Select chip, OrgSwitcher).
  "bottom-stretch": "absolute top-full left-0 right-0 mt-1",
  // Anchored to the right edge of the trigger, baseline-aligned at bottom
  // (the menu rail's UserMenu pops out to the right).
  "right-bottom": "absolute bottom-0 left-full ml-2",
};
