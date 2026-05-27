import { Check, X } from "lucide-react";
import { SectionHeader } from "../atoms/SectionHeader";
import { SectionCard } from "../molecules/SectionCard";
import { Spinner } from "../atoms/Spinner";
import { useMcpServerToolCalls } from "../../hooks/useMcpServers";
import { useT } from "../../i18n";
import { formatMs, useTimeAgo } from "../../lib/time";
import type { ToolCall } from "../../types/api";

const COL_WIDTHS =
  "minmax(76px,90px) minmax(0,1fr) minmax(96px,140px) 72px minmax(120px,220px)";

export function RecentActivityCard({ serverId }: { serverId: string }) {
  const { t } = useT();
  const query = useMcpServerToolCalls(serverId);
  const items = query.data?.pages.flatMap((p) => p.items) ?? [];

  return (
    <SectionCard
      header={
        <SectionHeader
          eyebrow={
            <>
              {t("connections.detail.activity.eyebrow")}
              <span className="font-[var(--font-mono)] text-[10px] font-normal normal-case tracking-normal text-[var(--color-muted-foreground)]">
                {t("connections.detail.activity.last50")}
              </span>
            </>
          }
        />
      }
    >
      <ActivityBody query={query} items={items} />
    </SectionCard>
  );
}

type Query = ReturnType<typeof useMcpServerToolCalls>;

function ActivityBody({ query, items }: { query: Query; items: ToolCall[] }) {
  const { t } = useT();

  if (query.isLoading) {
    return (
      <div className="flex items-center justify-center px-5 py-8">
        <Spinner size={14} />
      </div>
    );
  }
  if (query.isError) {
    return (
      <div className="px-5 py-6 text-center text-[12px] text-[var(--color-rose)]">
        {t("connections.detail.activity.loadError")}
      </div>
    );
  }
  if (items.length === 0) {
    return (
      <div className="px-5 py-6 text-center text-[12px] text-[var(--color-muted-foreground)]">
        {t("connections.detail.activity.empty")}
      </div>
    );
  }

  return (
    <>
      <div
        className="grid items-center gap-3 border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-2.5 font-[var(--font-mono)] text-[9px] tracking-[0.15em] uppercase text-[var(--color-muted-foreground)]"
        style={{ gridTemplateColumns: COL_WIDTHS }}
      >
        <span>{t("connections.detail.activity.col.time")}</span>
        <span>{t("connections.detail.activity.col.tool")}</span>
        <span>{t("connections.detail.activity.col.agent")}</span>
        <span>{t("connections.detail.activity.col.latency")}</span>
        <span className="text-right">
          {t("connections.detail.activity.col.outcome")}
        </span>
      </div>
      {items.map((row) => (
        <ActivityRow key={row.id} row={row} />
      ))}
    </>
  );
}

function ActivityRow({ row }: { row: ToolCall }) {
  const { t } = useT();
  const timeAgo = useTimeAgo();
  const agentLabel =
    row.agent_name ?? t("connections.detail.activity.agentUnknown");
  const outcomeText = row.is_error
    ? (row.error_message ?? t("connections.detail.activity.outcomeError"))
    : t("connections.detail.activity.outcomeOk");

  return (
    <div
      className={`grid items-center gap-3 border-b border-[var(--color-line)] px-5 py-2.5 last:border-b-0 ${
        row.is_error ? "bg-[var(--color-rose-soft)]" : ""
      }`}
      style={{ gridTemplateColumns: COL_WIDTHS }}
    >
      <span className="font-[var(--font-mono)] text-[12px] text-[var(--color-muted-foreground)]">
        {timeAgo(row.started_at)}
      </span>
      <span
        className="truncate font-[var(--font-mono)] text-[12px] text-[var(--color-ink)]"
        title={row.tool_name}
      >
        {row.tool_name}
      </span>
      <span
        className="truncate text-[12px] text-[var(--color-moss-deep)]"
        title={agentLabel}
      >
        {agentLabel}
      </span>
      <span className="font-[var(--font-mono)] text-[12px] text-[var(--color-ink)]">
        {formatMs(row.duration_ms)}
      </span>
      <span
        className={`flex items-center justify-end gap-1.5 truncate font-[var(--font-mono)] text-[12px] ${
          row.is_error
            ? "font-semibold text-[var(--color-rose)]"
            : "text-[var(--color-ink)]"
        }`}
        title={outcomeText}
      >
        {row.is_error ? (
          <X
            className="h-3 w-3 shrink-0 text-[var(--color-rose)]"
            strokeWidth={2}
            aria-hidden
          />
        ) : (
          <Check
            className="h-3 w-3 shrink-0 text-[var(--color-moss)]"
            strokeWidth={2}
            aria-hidden
          />
        )}
        <span className="truncate">{outcomeText}</span>
      </span>
    </div>
  );
}
