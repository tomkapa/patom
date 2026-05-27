import type { HTMLAttributes } from "react";
import { cn } from "../../lib/utils";

export function Kbd({
  className,
  ...props
}: HTMLAttributes<HTMLElement>) {
  return (
    <kbd
      {...props}
      className={cn(
        "inline-flex h-[18px] min-w-[18px] items-center justify-center border border-[var(--color-line)] bg-[var(--color-surface)] px-1 font-[var(--font-mono)] text-[10px] font-medium text-[var(--color-muted-foreground)]",
        className,
      )}
    />
  );
}
