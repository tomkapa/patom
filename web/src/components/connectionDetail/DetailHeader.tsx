import { ShieldOff, Unplug } from "lucide-react";
import { Button } from "../atoms/Button";
import { CatalogIcon } from "../atoms/CatalogIcon";
import { StatusPill } from "../atoms/StatusPill";
import { useCatalogLookup } from "../../hooks/useMcpServers";
import { STATUS_KEY, statusToneOf } from "../../data/connectionStatus";
import { useT } from "../../i18n";
import type { TranslationKey } from "../../i18n/en";
import { CREDENTIALS_KIND, type CredentialsKind, type McpServer } from "../../types/api";

const AUTH_LABEL_KEY: Record<CredentialsKind | "none", TranslationKey> = {
  [CREDENTIALS_KIND.OAUTH2]: "connections.detail.auth.oauth",
  [CREDENTIALS_KIND.STATIC_HEADERS]: "connections.detail.auth.token",
  none: "connections.detail.auth.none",
};

export function DetailHeader({
  server,
  onDisableToggle,
  onDisconnect,
  disableBusy,
  disconnectBusy,
}: {
  server: McpServer;
  onDisableToggle: () => void;
  onDisconnect: () => void;
  disableBusy?: boolean;
  disconnectBusy?: boolean;
}) {
  const { t } = useT();
  const catalogLookup = useCatalogLookup();
  const entry = catalogLookup(server.catalog_id);
  const displayName = entry?.display_name ?? server.catalog_id;
  const tone = statusToneOf(server);
  const toolsCount = server.discovered_tools?.length ?? 0;
  const authLabel = t(AUTH_LABEL_KEY[server.credentials_kind ?? "none"]);

  return (
    <header className="flex items-center justify-between gap-4 border-b border-[var(--color-line)] px-8 pb-6 pt-2">
      <div className="flex min-w-0 items-center gap-4">
        <CatalogIcon name={displayName} iconUrl={entry?.icon_url} size={56} />
        <div className="flex min-w-0 flex-col gap-1.5">
          <div className="flex items-center gap-2.5">
            <h1 className="truncate font-[var(--font-display)] text-[28px] font-bold leading-none text-[var(--color-ink)]">
              {displayName}
            </h1>
            <StatusPill tone={tone} label={t(STATUS_KEY[tone])} />
          </div>
          <p className="truncate text-[13px] text-[var(--color-muted-foreground)]">
            {t("connections.detail.header.subtitle", {
              auth: authLabel,
              count: toolsCount,
            })}
          </p>
        </div>
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <Button
          variant="ghost"
          size="md"
          onClick={onDisableToggle}
          loading={disableBusy}
          className="border border-[var(--color-line)]"
        >
          <ShieldOff className="h-3.5 w-3.5" strokeWidth={1.75} />
          {server.enabled
            ? t("connections.detail.header.disable")
            : t("connections.detail.header.enable")}
        </Button>
        {server.has_credentials ? (
          <Button
            variant="danger"
            size="md"
            onClick={onDisconnect}
            loading={disconnectBusy}
            className="border border-[var(--color-rose)]"
          >
            <Unplug className="h-3.5 w-3.5" strokeWidth={1.75} />
            {t("connections.detail.header.disconnect")}
          </Button>
        ) : null}
      </div>
    </header>
  );
}
