import type { ReactNode } from "react";
import { cn } from "../../lib/utils";

export function EmptyState({
  icon,
  title,
  description,
  action,
  className,
}: {
  icon?: ReactNode;
  title: ReactNode;
  description?: ReactNode;
  action?: ReactNode;
  className?: string;
}) {
  return (
    <div
      className={cn(
        "flex flex-col items-center justify-center text-center gap-2 px-6 py-12",
        className,
      )}
    >
      {icon && (
        <div className="mb-1 flex h-10 w-10 items-center justify-center rounded-full bg-[var(--color-moss-soft)] text-[var(--color-moss-2)]">
          {icon}
        </div>
      )}
      <h3 className="font-[var(--font-display)] text-[15px] font-semibold text-[var(--color-ink)]">
        {title}
      </h3>
      {description && (
        <p className="max-w-[36ch] text-[13px] leading-[1.5] text-[var(--color-muted-foreground)]">
          {description}
        </p>
      )}
      {action && <div className="mt-2">{action}</div>}
    </div>
  );
}
