import type { ReactNode } from "react";
import { cn } from "../../lib/utils";

/** Reusable tab header for the Agent detail (and any future
 *  setting page that follows the same shell). Title left, optional
 *  subtitle below it, right-side action slot for Save / etc. */
export function PageTabHeader({
  title,
  subtitle,
  actions,
  className,
}: {
  title: ReactNode;
  subtitle?: ReactNode;
  actions?: ReactNode;
  className?: string;
}) {
  return (
    <header
      className={cn(
        "flex items-end justify-between gap-4 border-b border-[var(--color-line)] px-8 pt-2 pb-6",
        className,
      )}
    >
      <div className="min-w-0">
        <h1 className="font-[var(--font-display)] text-[32px] leading-tight font-bold text-[var(--color-ink)]">
          {title}
        </h1>
        {subtitle ? (
          <p className="mt-1 max-w-[68ch] text-[14px] text-[var(--color-muted)]">
            {subtitle}
          </p>
        ) : null}
      </div>
      {actions ? (
        <div className="flex shrink-0 items-center gap-2">{actions}</div>
      ) : null}
    </header>
  );
}
