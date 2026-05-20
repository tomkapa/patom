import { ShieldOff, Unplug } from "lucide-react";
import { Button } from "../atoms/Button";
import { Monogram } from "../atoms/Monogram";
import { StatusPill } from "../atoms/StatusPill";
import { entryForServer } from "../../data/mcpCatalog";
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
  const entry = entryForServer(server);
  const tone = statusToneOf(server);
  const toolsCount = server.discovered_tools?.length ?? 0;
  const authLabel = t(AUTH_LABEL_KEY[server.credentials_kind ?? "none"]);

  return (
    <header className="flex items-center justify-between gap-4 border-b border-[var(--color-line)] px-8 pb-6 pt-2">
      <div className="flex min-w-0 items-center gap-4">
        <Monogram
          name={entry?.name ?? server.catalog_id}
          size={56}
          bg={entry?.tileBg ?? "var(--color-rail)"}
          fg={entry?.tileFg ?? "#ffffff"}
          glyph={entry?.monogram ?? (server.catalog_id[0] ?? "?").toUpperCase()}
          iconSlug={entry?.iconSlug}
        />
        <div className="flex min-w-0 flex-col gap-1.5">
          <div className="flex items-center gap-2.5">
            <h1 className="truncate font-[var(--font-display)] text-[28px] font-bold leading-none text-[var(--color-ink)]">
              {entry?.name ?? server.catalog_id}
            </h1>
            <StatusPill tone={tone} label={t(STATUS_KEY[tone])} />
          </div>
          <p className="truncate text-[13px] text-[var(--color-muted)]">
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
