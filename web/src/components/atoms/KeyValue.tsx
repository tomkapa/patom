import type { ReactNode } from "react";
import { cn } from "../../lib/utils";

export function KeyValue({
  label,
  value,
  sublabel,
  className,
}: {
  label: ReactNode;
  value: ReactNode;
  sublabel?: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("flex min-w-0 flex-col gap-1.5", className)}>
      <span className="font-[var(--font-mono)] text-[10px] tracking-[0.12em] uppercase text-[var(--color-muted-foreground)]">
        {label}
      </span>
      <span className="truncate font-[var(--font-mono)] text-[18px] font-semibold text-[var(--color-ink)]">
        {value}
      </span>
      {sublabel ? (
        <span className="truncate font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)]">
          {sublabel}
        </span>
      ) : null}
    </div>
  );
}
