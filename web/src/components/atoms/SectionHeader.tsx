import type { ReactNode } from "react";
import { cn } from "../../lib/utils";

export function SectionHeader({
  eyebrow,
  right,
  tone = "neutral",
  className,
}: {
  eyebrow: ReactNode;
  right?: ReactNode;
  tone?: "neutral" | "danger";
  className?: string;
}) {
  const isDanger = tone === "danger";
  return (
    <div
      className={cn(
        "flex items-center justify-between gap-3 border-b px-5 py-3.5",
        isDanger
          ? "border-[var(--color-rose)] bg-[var(--color-rose-soft)]"
          : "border-[var(--color-line)] bg-[var(--color-paper-2)]",
        className,
      )}
    >
      <span
        className={cn(
          "inline-flex items-center gap-2 font-[var(--font-mono)] text-[11px] font-semibold tracking-[0.12em] uppercase",
          isDanger
            ? "text-[var(--color-rose)]"
            : "text-[var(--color-muted-foreground)]",
        )}
      >
        {eyebrow}
      </span>
      {right ? <div className="flex items-center gap-2.5">{right}</div> : null}
    </div>
  );
}
