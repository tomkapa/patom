import { useMemo, useState } from "react";
import { Plus, Search } from "lucide-react";
import {
  ConnectionsBreadcrumb,
  ConnectionsLayout,
} from "../components/templates/ConnectionsLayout";
import { CatalogIcon } from "../components/atoms/CatalogIcon";
import { Spinner } from "../components/atoms/Spinner";
import { ConnectModal } from "../components/organisms/ConnectModal";
import { useMcpCatalog } from "../hooks/useMcpServers";
import { useT } from "../i18n";
import type { McpCatalogEntry } from "../types/api";
import { cn } from "../lib/utils";

type Pending =
  | { kind: "entry"; entry: McpCatalogEntry }
  | { kind: "custom" };

export function ConnectionsCatalog() {
  const { t } = useT();
  const catalog = useMcpCatalog();
  const [query, setQuery] = useState("");
  const [pending, setPending] = useState<Pending | null>(null);

  const filtered = useMemo(() => {
    const rows = catalog.data ?? [];
    const q = query.trim().toLowerCase();
    if (!q) return rows;
    return rows.filter(
      (e) =>
        e.display_name.toLowerCase().includes(q) ||
        e.description.toLowerCase().includes(q),
    );
  }, [catalog.data, query]);

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
          <p className="mt-1 max-w-[60ch] text-[14px] text-[var(--color-muted-foreground)]">
            {t("connections.catalog.subtitle")}
          </p>
        </div>
        <label className="flex shrink-0 items-center gap-2 border border-[var(--color-line)] bg-[var(--color-card)] px-3.5 py-2.5">
          <Search className="h-3.5 w-3.5 text-[var(--color-muted-foreground)]" strokeWidth={1.75} />
          <input
            type="search"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("connections.catalog.search")}
            aria-label={t("connections.catalog.search")}
            className="w-[220px] bg-transparent text-[13px] text-[var(--color-ink)] outline-none placeholder:text-[var(--color-muted-foreground)]"
          />
        </label>
      </header>

      <div className="min-h-0 flex-1 overflow-auto px-8 pt-6 pb-10">
        {catalog.isLoading ? (
          <div className="flex items-center justify-center py-10 text-[var(--color-muted-foreground)]">
            <Spinner />
          </div>
        ) : (
          <div className="grid grid-cols-4 gap-4">
            {filtered.map((row) => (
              <CatalogTileButton
                key={row.catalog_id}
                row={row}
                onClick={() => setPending({ kind: "entry", entry: row })}
              />
            ))}
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
              <p className="text-[13px] leading-[1.4] text-[var(--color-muted-foreground)]">
                {t("connections.catalog.custom.blurb")}
              </p>
            </button>
            {filtered.length === 0 && query.trim() ? (
              <p className="col-span-4 py-10 text-center text-[13px] text-[var(--color-muted-foreground)]">
                {t("connections.catalog.empty")}
              </p>
            ) : null}
          </div>
        )}
      </div>

      {pending?.kind === "entry" ? (
        <ConnectModal
          mode={
            pending.entry.auth_kind === "oauth2"
              ? "oauth"
              : pending.entry.auth_kind === "none"
                ? "noAuth"
                : "apiToken"
          }
          entry={pending.entry}
          onClose={() => setPending(null)}
        />
      ) : pending?.kind === "custom" ? (
        <ConnectModal mode="customUrl" onClose={() => setPending(null)} />
      ) : null}
    </ConnectionsLayout>
  );
}

function CatalogTileButton({
  row,
  onClick,
}: {
  row: McpCatalogEntry;
  onClick: () => void;
}) {
  const { t } = useT();
  return (
    <button
      type="button"
      onClick={onClick}
      className="flex flex-col gap-3 border border-[var(--color-line)] bg-[var(--color-card)] p-5 text-left transition-colors hover:border-[var(--color-moss)] hover:bg-[var(--color-paper-2)]"
    >
      <div className="flex items-center justify-between gap-2">
        <CatalogIcon name={row.display_name} iconUrl={row.icon_url} size={32} />
        <span
          className={cn(
            "font-[var(--font-mono)] text-[10px] uppercase",
            row.wired
              ? "text-[var(--color-moss-deep)]"
              : "text-[var(--color-muted-foreground)]",
          )}
        >
          {row.wired ? t("connections.catalog.added") : ""}
        </span>
      </div>
      <div className="font-[var(--font-display)] text-[18px] font-bold text-[var(--color-ink)]">
        {row.display_name}
      </div>
      <p className="text-[13px] leading-[1.4] text-[var(--color-muted-foreground)]">
        {row.description}
      </p>
    </button>
  );
}
