import { cn } from "../../lib/utils";

/** Pill toggle. Tracks `checked` externally — purely presentational
 *  state, no internal store. Default sizing matches the connections-list
 *  call-site (h-5 w-9 with a 16×16 thumb); the Add Operator Note modal
 *  uses the same size at the design's 1× scale. */
export function Switch({
  checked,
  onChange,
  ariaLabel,
  disabled,
  className,
}: {
  checked: boolean;
  onChange: (next: boolean) => void;
  ariaLabel: string;
  disabled?: boolean;
  className?: string;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={ariaLabel}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      className={cn(
        "flex h-5 w-9 items-center p-0.5 transition-colors",
        checked
          ? "justify-end bg-[var(--color-moss)]"
          : "justify-start bg-[var(--color-line)]",
        disabled && "cursor-not-allowed opacity-50",
        className,
      )}
    >
      <span aria-hidden className="block h-4 w-4 bg-white" />
    </button>
  );
}
