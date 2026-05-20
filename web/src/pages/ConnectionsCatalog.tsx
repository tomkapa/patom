import { useMemo, useState } from "react";
import { ChevronDown, Plus, Search } from "lucide-react";
import {
  ConnectionsBreadcrumb,
  ConnectionsLayout,
} from "../components/templates/ConnectionsLayout";
import { Monogram } from "../components/atoms/Monogram";
import { ConnectModal } from "../components/organisms/ConnectModal";
import { useMcpServers } from "../hooks/useMcpServers";
import { useT } from "../i18n";
import { MCP_CATALOG, type CatalogCategory, type CatalogEntry } from "../data/mcpCatalog";
import { cn } from "../lib/utils";

type Tab = "all" | CatalogCategory;

const TAB_ORDER: Tab[] = [
  "all",
  "productivity",
  "dev",
  "comms",
  "data",
  "custom",
];

const TAB_LABEL: Record<Tab, Parameters<ReturnType<typeof useT>["t"]>[0]> = {
  all: "connections.catalog.tabs.all",
  productivity: "connections.catalog.tabs.productivity",
  dev: "connections.catalog.tabs.dev",
  comms: "connections.catalog.tabs.comms",
  data: "connections.catalog.tabs.data",
  custom: "connections.catalog.tabs.custom",
};

type Pending =
  | { kind: "entry"; entry: CatalogEntry }
  | { kind: "custom" };

export function ConnectionsCatalog() {
  const { t } = useT();
  const servers = useMcpServers();
  const [tab, setTab] = useState<Tab>("all");
  const [query, setQuery] = useState("");
  const [pending, setPending] = useState<Pending | null>(null);

  const aliases = useMemo(
    () => new Set((servers.data ?? []).map((s) => s.catalog_id)),
    [servers.data],
  );

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return MCP_CATALOG.filter((e) => {
      if (tab !== "all" && e.category !== tab) return false;
      if (!q) return true;
      return (
        e.name.toLowerCase().includes(q) || e.blurb.toLowerCase().includes(q)
      );
    });
  }, [tab, query]);

  const showCustomTile = tab === "all" || tab === "custom";

  const tabCounts = useMemo<Record<Tab, number>>(() => {
    const counts = {
      all: MCP_CATALOG.length,
      productivity: 0,
      dev: 0,
      comms: 0,
      data: 0,
      custom: 0,
    };
    for (const e of MCP_CATALOG) counts[e.category]++;
    return counts;
  }, []);

  return (
    <ConnectionsLayout active="catalog">
      <ConnectionsBreadcrumb
        trail={[
          { label: t("connections.breadcrumb.workspace") },
          { label: t("connections.breadcrumb.connections") },
          { label: t("connections.breadcrumb.add"), current: true },
        ]}
      />
      <header className="flex items-end justify-between gap-4 border-b border-[var(--color-line)] px-8 pt-2 pb-6">
        <div className="min-w-0">
          <h1 className="font-[var(--font-display)] text-[32px] leading-tight font-bold text-[var(--color-ink)]">
            {t("connections.catalog.title")}
          </h1>
          <p className="mt-1 max-w-[60ch] text-[14px] text-[var(--color-muted)]">
            {t("connections.catalog.subtitle")}
          </p>
        </div>
        <label className="flex shrink-0 items-center gap-2 border border-[var(--color-line)] bg-[var(--color-card)] px-3.5 py-2.5">
          <Search className="h-3.5 w-3.5 text-[var(--color-muted)]" strokeWidth={1.75} />
          <input
            type="search"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("connections.catalog.search")}
            aria-label={t("connections.catalog.search")}
            className="w-[220px] bg-transparent text-[13px] text-[var(--color-ink)] outline-none placeholder:text-[var(--color-muted)]"
          />
        </label>
      </header>

      <div className="px-8 pt-5">
        <div className="flex items-center gap-2">
          {TAB_ORDER.filter((id) => id !== "custom").map((id) => {
            const isActive = id === tab;
            return (
              <button
                key={id}
                type="button"
                onClick={() => setTab(id)}
                aria-pressed={isActive}
                className={cn(
                  "flex items-center gap-1.5 px-3 py-1.5 font-[var(--font-mono)] text-[11.5px] transition-colors",
                  isActive
                    ? "bg-[var(--color-ink)] text-white"
                    : "border border-[var(--color-line)] bg-[var(--color-card)] text-[var(--color-muted)] hover:text-[var(--color-ink)]",
                )}
              >
                <span>{t(TAB_LABEL[id])}</span>
                <span
                  className={cn(
                    "font-[var(--font-mono)] text-[10px]",
                    isActive ? "text-white/70" : "text-[var(--color-muted-2)]",
                  )}
                >
                  {tabCounts[id]}
                </span>
              </button>
            );
          })}
          <div className="ml-auto flex items-center gap-1.5 font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]">
            <span>{t("connections.catalog.sort")}</span>
            <span className="font-medium text-[var(--color-ink)]">
              {t("connections.catalog.sort.mostUsed")}
            </span>
            <ChevronDown className="h-3 w-3" strokeWidth={1.75} />
          </div>
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-auto px-8 pt-6 pb-10">
        {filtered.length === 0 && !showCustomTile ? (
          <p className="py-10 text-center text-[13px] text-[var(--color-muted)]">
            {t("connections.catalog.empty")}
          </p>
        ) : (
          <div className="grid grid-cols-4 gap-4">
            {filtered.map((entry) => (
              <CatalogTile
                key={entry.id}
                entry={entry}
                added={aliases.has(entry.id)}
                onClick={() => setPending({ kind: "entry", entry })}
              />
            ))}
            {showCustomTile ? (
              <button
                type="button"
                onClick={() => setPending({ kind: "custom" })}
                className="flex flex-col gap-3 border border-[var(--color-moss)] bg-[var(--color-moss-tint)] p-5 text-left transition-colors hover:bg-[var(--color-moss-soft)]"
              >
                <div className="flex items-center justify-between gap-2">
                  <div
                    aria-hidden
                    className="flex h-8 w-8 items-center justify-center bg-[var(--color-moss)] text-white"
                  >
                    <Plus className="h-4 w-4" strokeWidth={2.25} />
                  </div>
                  <span className="font-[var(--font-mono)] text-[10px] text-[var(--color-moss-deep)] uppercase">
                    URL
                  </span>
                </div>
                <div className="font-[var(--font-display)] text-[18px] font-bold text-[var(--color-ink)]">
                  {t("connections.catalog.custom.title")}
                </div>
                <p className="text-[13px] leading-[1.4] text-[var(--color-muted)]">
                  {t("connections.catalog.custom.blurb")}
                </p>
              </button>
            ) : null}
          </div>
        )}
      </div>

      {pending?.kind === "entry" ? (
        <ConnectModal
          mode={pending.entry.auth === "oauth" ? "oauth" : "apiToken"}
          entry={pending.entry}
          onClose={() => setPending(null)}
        />
      ) : pending?.kind === "custom" ? (
        <ConnectModal mode="customUrl" onClose={() => setPending(null)} />
      ) : null}
    </ConnectionsLayout>
  );
}

function CatalogTile({
  entry,
  added,
  onClick,
}: {
  entry: CatalogEntry;
  added: boolean;
  onClick: () => void;
}) {
  const { t } = useT();
  const toolsLabel =
    entry.toolCount === 1
      ? t("connections.catalog.tool")
      : t("connections.catalog.tools");
  return (
    <button
      type="button"
      onClick={onClick}
      className="flex flex-col gap-3 border border-[var(--color-line)] bg-[var(--color-card)] p-5 text-left transition-colors hover:border-[var(--color-moss)] hover:bg-[var(--color-paper-2)]"
    >
      <div className="flex items-center justify-between gap-2">
        <Monogram
          name={entry.name}
          size={32}
          bg={entry.tileBg}
          fg={entry.tileFg}
          glyph={entry.monogram}
          iconSlug={entry.iconSlug}
        />
        <span
          className={cn(
            "font-[var(--font-mono)] text-[10px] uppercase",
            added
              ? "text-[var(--color-moss-deep)]"
              : "text-[var(--color-muted)]",
          )}
        >
          {added
            ? t("connections.catalog.added")
            : entry.toolCount
              ? `+${entry.toolCount} ${toolsLabel}`
              : ""}
        </span>
      </div>
      <div className="font-[var(--font-display)] text-[18px] font-bold text-[var(--color-ink)]">
        {entry.name}
      </div>
      <p className="text-[13px] leading-[1.4] text-[var(--color-muted)]">
        {entry.blurb}
      </p>
    </button>
  );
}
