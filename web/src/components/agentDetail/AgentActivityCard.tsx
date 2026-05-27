import { useMemo } from "react";
import { ArrowRight, Check, X } from "lucide-react";
import { Link } from "react-router-dom";
import { SectionCard } from "../molecules/SectionCard";
import { SectionHeader } from "../atoms/SectionHeader";
import { Spinner } from "../atoms/Spinner";
import { CatalogIcon } from "../atoms/CatalogIcon";
import { useT } from "../../i18n";
import { useAgentToolCalls } from "../../hooks/useAgents";
import { useCatalogLookup } from "../../hooks/useMcpServers";
import type { CatalogLookup } from "../../data/mcpCatalog";
import { formatMs, useTimeAgo } from "../../lib/time";
import type { AgentToolCall, McpServer } from "../../types/api";

const COL_WIDTHS =
  "minmax(72px,88px) minmax(120px,160px) minmax(0,1fr) minmax(140px,200px)";

export function AgentActivityCard({
  agentId,
  agentName,
  servers,
}: {
  agentId: string;
  agentName: string;
  servers: McpServer[];
}) {
  const { t } = useT();
  const query = useAgentToolCalls(agentId);
  const catalogLookup = useCatalogLookup();
  const items = query.data?.pages.flatMap((p) => p.items) ?? [];
  const serversById = useMemo(
    () => new Map(servers.map((s) => [s.id, s] as const)),
    [servers],
  );

  return (
    <SectionCard
      header={
        <SectionHeader
          eyebrow={
            <>
              {t("agent.detail.activity.eyebrow", { name: agentName })}
              <span className="font-[var(--font-mono)] text-[10px] font-normal normal-case tracking-normal text-[var(--color-muted-foreground)]">
                {t("agent.detail.activity.subtitle")}
              </span>
            </>
          }
          right={
            <Link
              to="/connections"
              className="inline-flex items-center gap-1 text-[12px] text-[var(--color-moss-deep)] hover:underline"
            >
              {t("agent.detail.activity.auditLink")}
              <ArrowRight className="h-3 w-3" strokeWidth={2} aria-hidden />
            </Link>
          }
        />
      }
    >
      <ActivityBody
        query={query}
        items={items}
        serversById={serversById}
        catalogLookup={catalogLookup}
      />
    </SectionCard>
  );
}

type Query = ReturnType<typeof useAgentToolCalls>;

function ActivityBody({
  query,
  items,
  serversById,
  catalogLookup,
}: {
  query: Query;
  items: AgentToolCall[];
  serversById: Map<string, McpServer>;
  catalogLookup: CatalogLookup;
}) {
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
        {t("agent.detail.activity.loadError")}
      </div>
    );
  }
  if (items.length === 0) {
    return (
      <div className="px-5 py-6 text-center text-[12px] text-[var(--color-muted-foreground)]">
        {t("agent.detail.activity.empty")}
      </div>
    );
  }

  return (
    <>
      <div
        className="grid items-center gap-3 border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-2.5 font-[var(--font-mono)] text-[9px] tracking-[0.15em] uppercase text-[var(--color-muted-foreground)]"
        style={{ gridTemplateColumns: COL_WIDTHS }}
      >
        <span>{t("agent.detail.activity.col.time")}</span>
        <span>{t("agent.detail.activity.col.connection")}</span>
        <span>{t("agent.detail.activity.col.tool")}</span>
        <span className="text-right">
          {t("agent.detail.activity.col.outcome")}
        </span>
      </div>
      {items.map((row) => (
        <ActivityRow
          key={row.id}
          row={row}
          serversById={serversById}
          catalogLookup={catalogLookup}
        />
      ))}
    </>
  );
}

function ActivityRow({
  row,
  serversById,
  catalogLookup,
}: {
  row: AgentToolCall;
  serversById: Map<string, McpServer>;
  catalogLookup: CatalogLookup;
}) {
  const { t } = useT();
  const timeAgo = useTimeAgo();
  const server = row.mcp_server_id ? serversById.get(row.mcp_server_id) : null;
  const catalog = row.mcp_server_catalog_id
    ? catalogLookup(row.mcp_server_catalog_id)
    : undefined;
  const connLabel =
    catalog?.display_name ??
    server?.catalog_id ??
    row.mcp_server_catalog_id ??
    t("agent.detail.activity.unknownConnection");
  const outcomeText = row.is_error
    ? (row.error_message ?? t("agent.detail.activity.outcomeError"))
    : `${formatMs(row.duration_ms)} · 200 OK`;

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
      <span className="flex items-center gap-2 min-w-0">
        <CatalogIcon name={connLabel} iconUrl={catalog?.icon_url} size={16} />
        <span className="truncate text-[12px] font-medium text-[var(--color-ink)]">
          {connLabel}
        </span>
      </span>
      <span
        className="truncate font-[var(--font-mono)] text-[12px] text-[var(--color-ink)]"
        title={row.tool_name}
      >
        {row.tool_name}
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
