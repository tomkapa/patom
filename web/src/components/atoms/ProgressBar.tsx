import { cn } from "../../lib/utils";

/** 4px progress bar — moss fill on a `--color-line` track. Width is
 *  fluid (`100%`) so the caller sizes the row; `value`/`max` clamp to
 *  `[0, max]`. */
export function ProgressBar({
  value,
  max,
  ariaLabel,
  className,
}: {
  value: number;
  max: number;
  ariaLabel?: string;
  className?: string;
}) {
  const safeMax = max > 0 ? max : 1;
  const pct = Math.max(0, Math.min(100, (value / safeMax) * 100));
  return (
    <div
      role="progressbar"
      aria-valuemin={0}
      aria-valuemax={safeMax}
      aria-valuenow={Math.max(0, Math.min(safeMax, value))}
      aria-label={ariaLabel}
      className={cn("h-1 w-full bg-[var(--color-line)]", className)}
    >
      <div
        className="h-full bg-[var(--color-moss)]"
        style={{ width: `${pct}%` }}
      />
    </div>
  );
}
