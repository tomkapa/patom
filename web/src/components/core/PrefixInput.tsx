import { forwardRef } from "react";
import type { InputHTMLAttributes, ReactNode } from "react";
import { cn } from "../../lib/utils";

/**
 * Text input with a left-side prefix label, e.g. `relay.app/`. Matches
 * the design's slug field on the General tab.
 */
export const PrefixInput = forwardRef<HTMLInputElement, {
  prefix: ReactNode;
  invalid?: boolean;
  className?: string;
} & Omit<InputHTMLAttributes<HTMLInputElement>, "className">>(function PrefixInput(
  { prefix, invalid, className, ...rest },
  ref,
) {
  return (
    <div
      className={cn(
        "inline-flex w-full items-stretch border bg-[var(--color-card)] focus-within:ring-1 focus-within:ring-[var(--color-moss)]",
        invalid
          ? "border-[var(--color-rose)]"
          : "border-[var(--color-line)]",
        className,
      )}
    >
      <span className="inline-flex items-center border-r border-[var(--color-line)] bg-[var(--color-paper-2)] px-3 font-[var(--font-mono)] text-[12px] text-[var(--color-muted)]">
        {prefix}
      </span>
      <input
        ref={ref}
        spellCheck={false}
        autoCorrect="off"
        autoCapitalize="off"
        className="min-w-0 flex-1 bg-transparent px-3 py-2 font-[var(--font-mono)] text-[13px] text-[var(--color-ink)] outline-none placeholder:text-[var(--color-muted-2)]"
        {...rest}
      />
    </div>
  );
});
