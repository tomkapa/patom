import { useMemo, useState } from "react";
import { ChevronDown, Plus, Search } from "lucide-react";
import {
  ConnectionsBreadcrumb,
  ConnectionsLayout,
} from "../components/templates/ConnectionsLayout";
import { Monogram } from "../components/atoms/Monogram";
import { Spinner } from "../components/atoms/Spinner";
import { ConnectModal } from "../components/organisms/ConnectModal";
import { useMcpCatalog } from "../hooks/useMcpServers";
import { useT } from "../i18n";
import { entryById, type CatalogCategory, type CatalogEntry } from "../data/mcpCatalog";
import type { McpCatalogEntry } from "../types/api";
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

/** Merged view-model for one catalog tile. Backend is the source of truth
 *  for identity (`catalog_id`, `display_name`, `description`, `auth_kind`,
 *  `wired`); the FE-side `mcpCatalog.ts` contributes visual metadata
 *  (monogram, brand colors, icon slug) plus API-token wiring hints (header
 *  name/prefix/help URL) that the backend wire shape does not yet expose. */
type CatalogTile = {
  id: string;
  name: string;
  blurb: string;
  category: CatalogCategory;
  wired: boolean;
  fe?: CatalogEntry;
  auth: McpCatalogEntry["auth_kind"];
  /** R2-hosted tile icon — built-ins seeded via migration 33, org-scoped
   *  entries via the upload endpoint. Renders inline; falls back to the
   *  FE-side Monogram when absent. */
  iconUrl: string | null;
};

function toTile(row: McpCatalogEntry): CatalogTile {
  const fe = entryById(row.catalog_id);
  const category: CatalogCategory =
    fe?.category ?? (row.is_custom ? "custom" : "productivity");
  return {
    id: row.catalog_id,
    name: row.display_name,
    blurb: row.description,
    category,
    wired: row.wired,
    fe,
    auth: row.auth_kind,
    iconUrl: row.icon_url ?? null,
  };
}

type Pending =
  | { kind: "entry"; entry: CatalogEntry }
  | { kind: "custom" };

export function ConnectionsCatalog() {
  const { t } = useT();
  const catalog = useMcpCatalog();
  const [tab, setTab] = useState<Tab>("all");
  const [query, setQuery] = useState("");
  const [pending, setPending] = useState<Pending | null>(null);

  const tiles = useMemo<CatalogTile[]>(
    () => (catalog.data ?? []).map(toTile),
    [catalog.data],
  );

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return tiles.filter((e) => {
      if (tab !== "all" && e.category !== tab) return false;
      if (!q) return true;
      return (
        e.name.toLowerCase().includes(q) || e.blurb.toLowerCase().includes(q)
      );
    });
  }, [tiles, tab, query]);

  const showCustomTile = tab === "all" || tab === "custom";

  const tabCounts = useMemo<Record<Tab, number>>(() => {
    const counts: Record<Tab, number> = {
      all: tiles.length,
      productivity: 0,
      dev: 0,
      comms: 0,
      data: 0,
      custom: 0,
    };
    for (const e of tiles) counts[e.category]++;
    return counts;
  }, [tiles]);

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
        {catalog.isLoading ? (
          <div className="flex items-center justify-center py-10 text-[var(--color-muted)]">
            <Spinner />
          </div>
        ) : filtered.length === 0 && !showCustomTile ? (
          <p className="py-10 text-center text-[13px] text-[var(--color-muted)]">
            {t("connections.catalog.empty")}
          </p>
        ) : (
          <div className="grid grid-cols-4 gap-4">
            {filtered.map((tile) => (
              <CatalogTileButton
                key={tile.id}
                tile={tile}
                onClick={() => {
                  // Synthesize a `CatalogEntry` from the backend row +
                  // (when known) FE visual metadata, so `ConnectModal`
                  // can stay shaped around `CatalogEntry`.
                  const entry = materializeEntry(tile);
                  setPending({ kind: "entry", entry });
                }}
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

/** Adapt a backend tile to the `CatalogEntry` shape `ConnectModal`
 *  takes. Backend `display_name`/`description` always win; FE visuals
 *  + API-token hints come from the hardcoded entry when known.
 *  Unknown ids get a monogram fallback and an empty `defaultUrl`, which
 *  OAuthBody treats as a signal to use the backend short-form create. */
function materializeEntry(tile: CatalogTile): CatalogEntry {
  if (tile.fe) {
    return {
      ...tile.fe,
      name: tile.name,
      blurb: tile.blurb,
      iconUrl: tile.iconUrl ?? tile.fe.iconUrl,
    };
  }
  return {
    id: tile.id,
    name: tile.name,
    blurb: tile.blurb,
    category: tile.category,
    monogram: (tile.name[0] ?? "?").toUpperCase(),
    tileBg: "var(--color-rail)",
    tileFg: "#fff",
    iconUrl: tile.iconUrl ?? undefined,
    defaultUrl: "",
    auth: tile.auth === "oauth2" ? "oauth" : "apiToken",
  };
}

function CatalogTileButton({
  tile,
  onClick,
}: {
  tile: CatalogTile;
  onClick: () => void;
}) {
  const { t } = useT();
  const [iconFailed, setIconFailed] = useState(false);
  const monogram = tile.fe?.monogram ?? (tile.name[0] ?? "?").toUpperCase();
  const tileBg = tile.fe?.tileBg ?? "var(--color-rail)";
  const tileFg = tile.fe?.tileFg ?? "#fff";
  const toolCount = tile.fe?.toolCount;
  const toolsLabel =
    toolCount === 1
      ? t("connections.catalog.tool")
      : t("connections.catalog.tools");
  const showImage: string | null = !iconFailed && tile.iconUrl ? tile.iconUrl : null;
  return (
    <button
      type="button"
      onClick={onClick}
      className="flex flex-col gap-3 border border-[var(--color-line)] bg-[var(--color-card)] p-5 text-left transition-colors hover:border-[var(--color-moss)] hover:bg-[var(--color-paper-2)]"
    >
      <div className="flex items-center justify-between gap-2">
        {showImage ? (
          <img
            src={showImage}
            alt=""
            onError={() => setIconFailed(true)}
            className="h-8 w-8 shrink-0 border border-[var(--color-line)] bg-white object-contain p-1"
          />
        ) : (
          <Monogram
            name={tile.name}
            size={32}
            bg={tileBg}
            fg={tileFg}
            glyph={monogram}
            iconSlug={tile.fe?.iconSlug}
          />
        )}
        <span
          className={cn(
            "font-[var(--font-mono)] text-[10px] uppercase",
            tile.wired
              ? "text-[var(--color-moss-deep)]"
              : "text-[var(--color-muted)]",
          )}
        >
          {tile.wired
            ? t("connections.catalog.added")
            : toolCount
              ? `+${toolCount} ${toolsLabel}`
              : ""}
        </span>
      </div>
      <div className="font-[var(--font-display)] text-[18px] font-bold text-[var(--color-ink)]">
        {tile.name}
      </div>
      <p className="text-[13px] leading-[1.4] text-[var(--color-muted)]">
        {tile.blurb}
      </p>
    </button>
  );
}
