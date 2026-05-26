import type { ReactNode } from "react";
import { cn } from "../../lib/utils";

/** Caption-above-value tile with an optional inline delta. Used by the
 *  Logs & metrics chart strip and the Prompt diff "since vN" KPI strip
 *  (pencil frames `a3ULi` and `nfz26`). The two callers feed the same
 *  shape so this lives at the molecule layer rather than as ad-hoc
 *  components inside `logs/`. */
export function KpiTile({
  label,
  value,
  delta,
  tone,
  className,
}: {
  label: string;
  value: ReactNode;
  delta?: ReactNode;
  /** `rose` tints the value red — used when a KPI represents a failure
   *  count above zero. */
  tone?: "rose";
  className?: string;
}) {
  const valueColor =
    tone === "rose" ? "text-[var(--color-rose)]" : "text-[var(--color-ink)]";
  return (
    <div
      className={cn(
        "flex flex-col gap-1 rounded border border-[var(--color-line)] bg-[var(--color-card)] px-4 py-3",
        className,
      )}
    >
      <span className="font-[var(--font-mono)] text-[10px] tracking-[0.15em] uppercase text-[var(--color-muted)]">
        {label}
      </span>
      <div className="flex items-baseline gap-2">
        <span
          className={cn(
            "font-[var(--font-display)] text-[22px] leading-none font-semibold",
            valueColor,
          )}
        >
          {value}
        </span>
        {delta ? (
          <span className="font-[var(--font-mono)] text-[11px]">{delta}</span>
        ) : null}
      </div>
    </div>
  );
}
