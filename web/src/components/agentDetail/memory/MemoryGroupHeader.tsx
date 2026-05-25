import { ChevronDown, ChevronRight } from "lucide-react";
import { cn } from "../../../lib/utils";
import { useT } from "../../../i18n";
import type { MemoryKind } from "../../../types/api";

export function MemoryGroupHeader({
  kind,
  count,
  open,
  onToggle,
}: {
  kind: MemoryKind;
  count: number;
  open: boolean;
  onToggle: () => void;
}) {
  const { t } = useT();
  const label = t(`agent.detail.memory.kind.${kind}` as const).toUpperCase();
  return (
    <button
      type="button"
      aria-expanded={open}
      onClick={onToggle}
      className={cn(
        "flex w-full items-center gap-2 border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-2 text-left transition-colors hover:bg-[var(--color-paper)]",
      )}
    >
      <span className="font-[var(--font-mono)] text-[11px] font-bold tracking-[0.1em] text-[var(--color-ink)]">
        {label}
      </span>
      <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]">
        {count}
      </span>
      <span className="flex-1" />
      {open ? (
        <ChevronDown
          className="h-3 w-3 text-[var(--color-muted)]"
          strokeWidth={1.75}
        />
      ) : (
        <ChevronRight
          className="h-3 w-3 text-[var(--color-muted)]"
          strokeWidth={1.75}
        />
      )}
    </button>
  );
}
