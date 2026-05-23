import { ExternalLink, ShieldCheck } from "lucide-react";
import { KeyValue } from "../atoms/KeyValue";
import { SectionHeader } from "../atoms/SectionHeader";
import { SectionCard } from "../molecules/SectionCard";
import { useCatalogLookup } from "../../hooks/useMcpServers";
import { useT } from "../../i18n";
import { useTimeAgo } from "../../lib/time";
import type { McpServer } from "../../types/api";

export function StatusCard({ server }: { server: McpServer }) {
  const { t } = useT();
  const entry = useCatalogLookup()(server.catalog_id);
  const manageUrl = entry?.homepage_url ?? null;
  const timeAgo = useTimeAgo();

  const lastUpdated = timeAgo(server.updated_at);
  const lastSeen = timeAgo(server.last_seen_at);
  const errorAt = server.last_error ? timeAgo(server.updated_at) : "—";

  return (
    <SectionCard
      header={
        <SectionHeader
          eyebrow={t("connections.detail.status.eyebrow")}
          right={
            <span className="font-[var(--font-mono)] text-[10px] text-[var(--color-muted)]">
              {t("connections.detail.status.updated", { value: lastUpdated })}
            </span>
          }
        />
      }
      footer={
        <div className="flex items-center gap-3.5 border-t border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-3">
          <ShieldCheck
            className="h-3.5 w-3.5 shrink-0 text-[var(--color-moss)]"
            strokeWidth={1.75}
            aria-hidden
          />
          <div className="flex min-w-0 flex-1 flex-col">
            <span className="truncate text-[13px] font-medium text-[var(--color-ink)]">
              {server.creator_email
                ? t("connections.detail.status.createdBy", {
                    email: server.creator_email,
                  })
                : t("connections.detail.status.noCreator")}
            </span>
            <span className="truncate font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]">
              {server.token_expires_at
                ? t("connections.detail.status.tokenExpires", {
                    value: timeAgo(server.token_expires_at),
                  })
                : t("connections.detail.status.tokenStandard")}
            </span>
          </div>
          {manageUrl ? (
            <a
              href={manageUrl}
              target="_blank"
              rel="noreferrer noopener"
              className="inline-flex items-center gap-1.5 text-[12px] text-[var(--color-moss-deep)] hover:underline"
            >
              {t("connections.detail.status.manageOn", {
                name: entry?.display_name ?? server.catalog_id,
              })}
              <ExternalLink className="h-3 w-3" strokeWidth={1.75} />
            </a>
          ) : null}
        </div>
      }
    >
      <div className="grid grid-cols-2 divide-x divide-[var(--color-line)]">
        <div className="p-5">
          <KeyValue
            label={t("connections.detail.status.lastCall")}
            value={lastSeen}
            sublabel={
              server.last_seen_at
                ? t("connections.detail.status.lastCallSub")
                : t("connections.detail.status.never")
            }
          />
        </div>
        <div className="p-5">
          <KeyValue
            label={t("connections.detail.status.lastError")}
            value={server.last_error ? errorAt : "—"}
            sublabel={server.last_error ?? t("connections.detail.status.noError")}
          />
        </div>
      </div>
    </SectionCard>
  );
}
