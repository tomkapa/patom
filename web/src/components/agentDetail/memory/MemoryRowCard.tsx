import { ExternalLink, Eye, Pin } from "lucide-react";
import { Link } from "react-router-dom";
import { cn } from "../../../lib/utils";
import { useTimeAgo } from "../../../lib/time";
import { useT } from "../../../i18n";
import type { MemoryRow, MemoryState } from "../../../types/api";

const STATE_PILL: Record<MemoryState, { fill: string; ink: string }> = {
  core: { fill: "#2D6B3F", ink: "#FFFFFF" },
  validated: { fill: "#1D4ED8", ink: "#FFFFFF" },
  held: { fill: "var(--color-line)", ink: "var(--color-ink)" },
  tentative: { fill: "#FEF3C7", ink: "#92400E" },
};

export function MemoryRowCard({
  row,
  onTogglePin,
  pinPending,
}: {
  row: MemoryRow;
  onTogglePin: () => void;
  pinPending: boolean;
}) {
  const { t } = useT();
  const timeAgo = useTimeAgo();
  const pill = STATE_PILL[row.state];
  return (
    <div
      className={cn(
        "flex flex-col gap-1.5 border-b border-[var(--color-line)] px-5 py-3",
        row.pinned ? "bg-[var(--color-moss-tint)]" : "bg-[var(--color-card)]",
      )}
    >
      <p className="text-[13px] leading-[1.5] text-[var(--color-ink)]">
        {row.content}
      </p>
      <div className="flex flex-wrap items-center gap-2">
        <span
          className="inline-flex items-center px-1.5 py-[2px] font-[var(--font-mono)] text-[10px] font-semibold uppercase"
          style={{ backgroundColor: pill.fill, color: pill.ink }}
        >
          {t(`agent.detail.memory.state.${row.state}` as const)}
        </span>
        <button
          type="button"
          aria-pressed={row.pinned}
          aria-label={t("agent.detail.memory.row.pinToggle", {
            kind: t(`agent.detail.memory.kind.${row.kind}` as const),
          })}
          disabled={pinPending}
          onClick={onTogglePin}
          className={cn(
            "inline-flex items-center gap-1 border px-1.5 py-[2px] font-[var(--font-mono)] text-[10px] font-semibold transition-colors",
            row.pinned
              ? "border-[var(--color-amber)] bg-[var(--color-amber-soft)] text-[var(--color-amber-deep)]"
              : "border-transparent text-[var(--color-fg-muted)] hover:text-[var(--color-ink)]",
            pinPending && "cursor-wait opacity-60",
          )}
        >
          <Pin className="h-2.5 w-2.5" strokeWidth={2} />
          <span>{t("agent.detail.memory.row.pinned")}</span>
        </button>
        <span className="text-[var(--color-fg-muted)]">·</span>
        <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)]">
          {timeAgo(row.created_at)}
        </span>
        <span className="text-[var(--color-fg-muted)]">·</span>
        <span className="inline-flex items-center gap-1 font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)]">
          <Eye className="h-2.5 w-2.5" strokeWidth={1.75} />
          {t("agent.detail.memory.row.access", { n: row.access_count })}
        </span>
        <span className="flex-1" />
        {row.source_turn_id ? (
          <Link
            to={`/?turn=${row.source_turn_id}`}
            className="inline-flex items-center gap-1 text-[var(--color-moss-deep)] hover:text-[var(--color-moss)] hover:underline"
          >
            <ExternalLink className="h-2.5 w-2.5" strokeWidth={1.75} />
            <span className="font-[var(--font-mono)] text-[11px]">
              {t("agent.detail.memory.row.sourceTurn")}
            </span>
          </Link>
        ) : null}
      </div>
    </div>
  );
}
