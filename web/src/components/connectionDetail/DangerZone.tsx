import { useState } from "react";
import { TriangleAlert } from "lucide-react";
import { Button } from "../atoms/Button";
import { SectionHeader } from "../atoms/SectionHeader";
import { Checkbox } from "../molecules/Checkbox";
import { SectionCard } from "../molecules/SectionCard";
import { useT } from "../../i18n";
import type { McpServer } from "../../types/api";

export function DangerZone({
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
  const [confirmed, setConfirmed] = useState(false);
  const hasCredentials = server.has_credentials;

  return (
    <SectionCard
      tone="danger"
      header={
        <SectionHeader
          tone="danger"
          eyebrow={
            <>
              <TriangleAlert
                className="h-3.5 w-3.5 text-[var(--color-rose)]"
                strokeWidth={2}
                aria-hidden
              />
              {t("connections.detail.danger.eyebrow")}
            </>
          }
        />
      }
    >
      <div className="flex flex-col gap-3 border-b border-[var(--color-line)] px-5 py-4">
        <div className="flex min-w-0 flex-col gap-1">
          <span className="text-[13px] font-semibold text-[var(--color-ink)]">
            {server.enabled
              ? t("connections.detail.danger.disable.title")
              : t("connections.detail.danger.enable.title")}
          </span>
          <p className="text-[12px] leading-snug text-[var(--color-muted-foreground)]">
            {server.enabled
              ? t("connections.detail.danger.disable.body")
              : t("connections.detail.danger.enable.body")}
          </p>
        </div>
        <div>
          <Button
            variant="ghost"
            size="sm"
            onClick={onDisableToggle}
            loading={disableBusy}
            className="border border-[var(--color-line)]"
          >
            {server.enabled
              ? t("connections.detail.danger.disable.cta")
              : t("connections.detail.danger.enable.cta")}
          </Button>
        </div>
      </div>
      {hasCredentials ? (
        <div className="flex flex-col gap-3 px-5 py-4">
          <div className="flex min-w-0 flex-col gap-1.5">
            <span className="text-[13px] font-semibold text-[var(--color-ink)]">
              {t("connections.detail.danger.disconnect.title")}
            </span>
            <p className="text-[12px] leading-snug text-[var(--color-muted-foreground)]">
              {t("connections.detail.danger.disconnect.body")}
            </p>
            <div className="mt-1 bg-[var(--color-paper-2)] px-2.5 py-2">
              <Checkbox
                checked={confirmed}
                onChange={setConfirmed}
                label={t("connections.detail.danger.disconnect.ack")}
              />
            </div>
          </div>
          <div>
            <Button
              variant="moss"
              size="sm"
              onClick={onDisconnect}
              loading={disconnectBusy}
              disabled={!confirmed}
              className="!bg-[var(--color-rose)] !border-[var(--color-rose)] hover:!bg-[var(--color-rose-soft)] hover:!text-[var(--color-rose)]"
            >
              {t("connections.detail.danger.disconnect.cta")}
            </Button>
          </div>
        </div>
      ) : null}
    </SectionCard>
  );
}
