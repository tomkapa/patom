import type { ReactNode } from "react";
import { cn } from "../../lib/utils";

/** Per-value colour for a TogglePill — caller provides the palette since
 *  these are usually tied to a domain enum (mutation kind, source). */
export type TogglePillTone = {
  fill: string;
  ink: string;
  border: string;
};

/** Coloured toggle pill. Single-value filter affordance — click to apply
 *  the filter, click again (or pick a sibling) to clear. The Event Journal
 *  source / mutation rows are the canonical site; any future single-value
 *  pill row should reuse this rather than re-rolling the styling. State
 *  lives in the caller via `active` + `dimmed`. */
export function TogglePill({
  active,
  dimmed,
  tone,
  onClick,
  ariaLabel,
  children,
}: {
  active: boolean;
  /** When another sibling is selected, fade non-active pills so the
   *  current choice reads as the salient one. */
  dimmed?: boolean;
  tone: TogglePillTone;
  onClick: () => void;
  ariaLabel?: string;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      aria-pressed={active}
      aria-label={ariaLabel}
      onClick={onClick}
      className={cn(
        "inline-flex cursor-pointer items-center border px-2 py-[2px] font-[var(--font-mono)] text-[11px] font-semibold transition-[opacity,transform,background-color,color,border-color] duration-150 ease-out active:scale-[0.97]",
        dimmed && "opacity-40",
      )}
      style={{
        backgroundColor: tone.fill,
        color: tone.ink,
        borderColor: tone.border,
      }}
    >
      {children}
    </button>
  );
}
