import type { ReactNode } from "react";
import { cn } from "../../lib/utils";

/** Horizontal strip of label-above-value cells with thin vertical
 *  dividers in between. Used by the prompt-diff modal header strip
 *  (`m7WtP`) and by other "metadata at a glance" rows. */
export function MetaRow({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <div
      className={cn(
        "flex items-center gap-6 border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-7 py-3.5",
        className,
      )}
    >
      {children}
    </div>
  );
}

/** Single cell inside a `MetaRow`: small uppercase label over a body
 *  value. Pass `mono` when the value is a timestamp or other
 *  fixed-width string. */
export function MetaCell({
  label,
  value,
  mono,
}: {
  label: string;
  value: ReactNode;
  mono?: boolean;
}) {
  return (
    <div className="flex flex-col gap-0.5">
      <div className="font-[var(--font-mono)] text-[10px] tracking-[0.12em] text-[var(--color-muted-foreground)] uppercase">
        {label}
      </div>
      <div
        className={cn(
          "text-[12px] font-medium text-[var(--color-ink)]",
          mono && "font-[var(--font-mono)]",
        )}
      >
        {value}
      </div>
    </div>
  );
}

/** Thin vertical divider used between `MetaCell`s. */
export function MetaDivider() {
  return <div className="h-[30px] w-px bg-[var(--color-line)]" />;
}
