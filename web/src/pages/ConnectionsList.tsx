import { useMemo, useState } from "react";
import { ListFilter, Plus, RefreshCw, Trash2 } from "lucide-react";
import { Link, useNavigate } from "react-router-dom";
import {
  ConnectionsBreadcrumb,
  ConnectionsLayout,
} from "../components/templates/ConnectionsLayout";
import { Button } from "../components/atoms/Button";
import { Spinner } from "../components/atoms/Spinner";
import { StatusSquare } from "../components/atoms/StatusSquare";
import { Monogram } from "../components/atoms/Monogram";
import { CatalogIcon } from "../components/atoms/CatalogIcon";
import { Switch } from "../components/atoms/Switch";
import { EmptyState } from "../components/molecules/EmptyState";
import { ConnectModal } from "../components/organisms/ConnectModal";
import { useT } from "../i18n";
import {
  useCatalogLookup,
  useDeleteMcpServer,
  useMcpServers,
  useUpdateMcpServer,
} from "../hooks/useMcpServers";
import type { CatalogLookup } from "../data/mcpCatalog";
import {
  STATUS_COLOR,
  STATUS_KEY,
  STATUS_SQUARE,
  statusToneOf,
  type StatusTone,
} from "../data/connectionStatus";
import type { McpServer } from "../types/api";
import { cn } from "../lib/utils";
import { useTimeAgo } from "../lib/time";
import { useAuthStore } from "../stores/authStore";

export function ConnectionsList() {
  const { t } = useT();
  const nav = useNavigate();
  const servers = useMcpServers();
  const catalogLookup = useCatalogLookup();
  const updateServer = useUpdateMcpServer();
  const deleteServer = useDeleteMcpServer();
  const [reconnectTarget, setReconnectTarget] = useState<McpServer | null>(null);
  const [statusFilter, setStatusFilter] = useState<StatusTone | "all">("all");
  const [showFilter, setShowFilter] = useState(false);

  const rows = useMemo(() => {
    const all = servers.data ?? [];
    if (statusFilter === "all") return all;
    return all.filter((s) => statusToneOf(s) === statusFilter);
  }, [servers.data, statusFilter]);

  const isLoading = servers.isLoading;
  const isEmpty = !isLoading && (servers.data?.length ?? 0) === 0;

  return (
    <ConnectionsLayout active="list">
      <ConnectionsBreadcrumb
        trail={[
          { label: t("connections.breadcrumb.workspace") },
          { label: t("connections.breadcrumb.connections") },
          { label: t("connections.breadcrumb.my"), current: true },
        ]}
      />
      <header className="flex items-end justify-between gap-4 border-b border-[var(--color-line)] px-8 pt-2 pb-6">
        <div className="min-w-0">
          <h1 className="font-[var(--font-display)] text-[32px] leading-tight font-bold text-[var(--color-ink)]">
            {t("connections.list.title")}
          </h1>
          <p className="mt-1 max-w-[60ch] text-[14px] text-[var(--color-muted)]">
            {t("connections.list.subtitle")}
          </p>
        </div>
        <div className="relative flex shrink-0 items-center gap-2">
          <button
            type="button"
            onClick={() => setShowFilter((v) => !v)}
            className={cn(
              "flex items-center gap-2 border border-[var(--color-line)] bg-[var(--color-card)] px-3 py-2 text-[12px] text-[var(--color-ink)] hover:bg-[var(--color-paper-2)]",
              statusFilter !== "all" && "border-[var(--color-moss)] text-[var(--color-moss-deep)]",
            )}
          >
            <ListFilter className="h-3.5 w-3.5" strokeWidth={1.75} />
            <span>
              {statusFilter === "all"
                ? t("connections.list.filter")
                : t(STATUS_KEY[statusFilter])}
            </span>
          </button>
          {showFilter ? (
            <div
              role="menu"
              className="absolute top-full right-12 z-10 mt-1 flex flex-col border border-[var(--color-line)] bg-[var(--color-card)] py-1 shadow-md"
            >
              {(["all", "ok", "reconnect", "error", "pending"] as const).map(
                (opt) => (
                  <button
                    key={opt}
                    type="button"
                    onClick={() => {
                      setStatusFilter(opt);
                      setShowFilter(false);
                    }}
                    className={cn(
                      "px-4 py-1.5 text-left text-[12px]",
                      statusFilter === opt
                        ? "bg-[var(--color-moss-tint)] text-[var(--color-moss-deep)]"
                        : "text-[var(--color-ink)] hover:bg-[var(--color-paper-2)]",
                    )}
                  >
                    {opt === "all"
                      ? t("connections.list.filter")
                      : t(STATUS_KEY[opt])}
                  </button>
                ),
              )}
            </div>
          ) : null}
          <Button
            variant="primary"
            onClick={() => nav("/connections/catalog")}
          >
            <Plus className="h-3.5 w-3.5" strokeWidth={2} />
            {t("connections.list.add")}
          </Button>
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-auto p-8">
        {isLoading ? (
          <div className="flex h-32 items-center justify-center text-[var(--color-muted)]">
            <Spinner size={16} />
          </div>
        ) : isEmpty ? (
          <EmptyState
            title={t("connections.list.empty.title")}
            description={t("connections.list.empty.body")}
            action={
              <Button
                variant="primary"
                onClick={() => nav("/connections/catalog")}
              >
                {t("connections.list.empty.cta")}
              </Button>
            }
          />
        ) : (
          <div className="border border-[var(--color-line)] bg-[var(--color-card)]">
            <div
              className="grid items-center gap-4 border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-2.5 font-[var(--font-mono)] text-[9px] tracking-[0.14em] text-[var(--color-muted)] uppercase"
              style={{
                gridTemplateColumns:
                  "88px minmax(0,1fr) 160px 56px 130px 56px 88px",
              }}
            >
              <span>{t("connections.list.col.status")}</span>
              <span>{t("connections.list.col.connection")}</span>
              <span>{t("connections.list.col.owner")}</span>
              <span className="text-right">
                {t("connections.list.col.tools")}
              </span>
              <span>{t("connections.list.col.lastSeen")}</span>
              <span className="text-center">
                {t("connections.list.col.enable")}
              </span>
              <span />
            </div>
            {rows.map((server, i) => (
              <ConnectionRow
                key={server.id}
                server={server}
                catalogLookup={catalogLookup}
                isLast={i === rows.length - 1}
                onToggle={(enabled) =>
                  updateServer.mutate({ id: server.id, patch: { enabled } })
                }
                onReconnect={() => setReconnectTarget(server)}
                onRemove={() => {
                  if (
                    window.confirm(
                      `${t("connections.confirm.removeTitle")}\n\n${t(
                        "connections.confirm.removeBody",
                      )}`,
                    )
                  ) {
                    deleteServer.mutate(server.id);
                  }
                }}
              />
            ))}
            {rows.length === 0 ? (
              <div className="px-5 py-8 text-center text-[12px] text-[var(--color-muted)]">
                {t("connections.catalog.empty")}
              </div>
            ) : null}
          </div>
        )}
      </div>

      {reconnectTarget ? (
        <ConnectModal
          mode="reconnect"
          server={reconnectTarget}
          onClose={() => setReconnectTarget(null)}
        />
      ) : null}
    </ConnectionsLayout>
  );
}

function ConnectionRow({
  server,
  catalogLookup,
  isLast,
  onToggle,
  onReconnect,
  onRemove,
}: {
  server: McpServer;
  catalogLookup: CatalogLookup;
  isLast: boolean;
  onToggle: (enabled: boolean) => void;
  onReconnect: () => void;
  onRemove: () => void;
}) {
  const { t } = useT();
  const timeAgo = useTimeAgo();
  const tone = statusToneOf(server);
  const entry = catalogLookup(server.catalog_id);
  const displayName = entry?.display_name ?? server.catalog_id;
  const toolsCount = server.discovered_tools?.length ?? 0;
  const hostFromConfig = useMemo(() => {
    try {
      return new URL(server.config.url).host;
    } catch {
      return server.config.url;
    }
  }, [server.config.url]);

  return (
    <div
      className={cn(
        "grid items-center gap-4 px-5 py-4",
        !isLast && "border-b border-[var(--color-line)]",
        !server.enabled && "opacity-60",
      )}
      style={{
        gridTemplateColumns: "88px minmax(0,1fr) 160px 56px 130px 56px 88px",
      }}
    >
      <StatusCell tone={tone} />
      <Link
        to={`/connections/${server.id}`}
        className="flex min-w-0 items-center gap-3.5 outline-none focus-visible:ring-2 focus-visible:ring-[var(--color-moss)]"
      >
        <CatalogIcon name={displayName} iconUrl={entry?.icon_url} size={36} />
        <div className="flex min-w-0 flex-col gap-0.5">
          <div className="truncate font-semibold text-[var(--color-ink)] hover:text-[var(--color-moss-deep)] hover:underline">
            {displayName}
          </div>
          <div className="truncate font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]">
            {hostFromConfig}
          </div>
        </div>
      </Link>
      <OwnerCell userId={server.created_by_user_id} createdAt={server.created_at} />
      <div className="text-right font-[var(--font-mono)] text-[14px] font-semibold text-[var(--color-ink)]">
        {toolsCount}
      </div>
      <div className="flex flex-col gap-0.5">
        <span className="font-[var(--font-mono)] text-[12px] text-[var(--color-ink)]">
          {server.last_seen_at
            ? timeAgo(server.last_seen_at)
            : t("connections.row.never")}
        </span>
        {server.last_error ? (
          <span
            className="truncate font-[var(--font-mono)] text-[10px] text-[var(--color-amber)]"
            title={server.last_error}
          >
            {server.last_error.slice(0, 32)}
          </span>
        ) : null}
      </div>
      <div className="flex justify-center">
        <Switch
          checked={server.enabled}
          onChange={onToggle}
          ariaLabel={`${t("connections.list.col.enable")} ${server.catalog_id}`}
        />
      </div>
      <div className="flex justify-end gap-1.5">
        {tone === "reconnect" || tone === "error" ? (
          <button
            type="button"
            onClick={onReconnect}
            aria-label={`${t("connections.row.reconnect")} ${server.catalog_id}`}
            className="flex h-[30px] w-[30px] items-center justify-center bg-[var(--color-amber)] text-white hover:opacity-90"
          >
            <RefreshCw className="h-3.5 w-3.5" strokeWidth={2} />
          </button>
        ) : null}
        <button
          type="button"
          onClick={onRemove}
          aria-label={`${t("connections.row.remove")} ${server.catalog_id}`}
          title={t("connections.row.remove")}
          className="flex h-[30px] w-[30px] items-center justify-center border border-[var(--color-line)] text-[var(--color-muted)] hover:border-[var(--color-rose)] hover:text-[var(--color-rose)]"
        >
          <Trash2 className="h-3.5 w-3.5" strokeWidth={1.75} />
        </button>
      </div>
    </div>
  );
}

function StatusCell({ tone }: { tone: StatusTone }) {
  const { t } = useT();
  return (
    <div
      className="flex items-center gap-2 font-[var(--font-mono)] text-[11px] font-bold tracking-[0.06em] uppercase"
      style={{ color: STATUS_COLOR[tone] }}
    >
      <StatusSquare status={STATUS_SQUARE[tone]} className="rounded-full" />
      <span>{t(STATUS_KEY[tone])}</span>
    </div>
  );
}

function OwnerCell({ userId, createdAt }: { userId: string; createdAt: string }) {
  const me = useAuthStore((s) => s.me);
  const timeAgo = useTimeAgo();
  const isSelf = me?.user.id === userId;
  const name = isSelf ? (me?.user.display_name ?? me?.user.email ?? "you") : userId.slice(0, 8);
  return (
    <div className="flex min-w-0 items-center gap-2">
      <Monogram name={name} tone="ink" size={22} />
      <div className="flex min-w-0 items-baseline gap-1.5 text-[13px] text-[var(--color-ink)]">
        <span className="truncate">{name}</span>
        <span className="shrink-0 font-[var(--font-mono)] text-[10px] text-[var(--color-muted)]">
          {timeAgo(createdAt)}
        </span>
      </div>
    </div>
  );
}

