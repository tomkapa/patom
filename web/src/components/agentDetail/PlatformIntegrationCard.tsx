import { BookOpen, MoreHorizontal, Plus, Radio, Trash2 } from "lucide-react";
import { useT } from "../../i18n";

/** A platform's live binding for the current agent, or `null` when the
 *  agent has no app of this platform yet. `extraValue` is the
 *  tenant_key / bot_user_id (or a "pending" placeholder) and drives the
 *  third info row — the API returns no timestamp for the design's SINCE. */
export type PlatformApp = {
  appId: string;
  boundValue: string;
  extraKey: string;
  extraValue: string;
  live: boolean;
};

/** One platform card, rendering the connected or disconnected state from
 *  the same shell so Lark and Discord stay visually identical. The card
 *  is purely presentational; the page owns data + mutations. */
export function PlatformIntegrationCard({
  name,
  logo,
  desc,
  app,
  canManage,
  onConnect,
  onRemove,
}: {
  name: string;
  logo: string;
  desc: string;
  app: PlatformApp | null;
  canManage: boolean;
  onConnect: () => void;
  onRemove: () => void;
}) {
  const { t } = useT();

  if (app) {
    return (
      <section
        className="flex flex-col overflow-hidden border border-[var(--color-line-strong)] bg-[var(--color-card)]"
        data-testid="integration-card"
        data-state="connected"
      >
        {/* Head */}
        <header className="flex items-center justify-between gap-3 px-4 pt-4 pb-3">
          <div className="flex min-w-0 items-center gap-2.5">
            <LogoTile logo={logo} name={name} size={36} img={18} />
            <div className="flex min-w-0 flex-col gap-0.5">
              <span className="truncate font-[var(--font-display)] text-[16px] font-bold text-[var(--color-ink-2)]">
                {name}
              </span>
              <span className="flex items-center gap-1.5">
                <span
                  aria-hidden
                  className="h-1.5 w-1.5 rounded-full bg-[var(--color-moss)]"
                />
                <span className="font-[var(--font-mono)] text-[11px] font-medium text-[var(--color-moss)]">
                  {t("agent.detail.integrations.card.connectedTag")}
                </span>
              </span>
            </div>
          </div>
          <button
            type="button"
            aria-label={t("agent.detail.integrations.card.menu")}
            className="flex h-7 w-7 shrink-0 cursor-pointer items-center justify-center border border-[var(--color-line-2)] text-[var(--color-muted-foreground)] transition-colors duration-150 ease-out hover:text-[var(--color-ink)]"
          >
            <MoreHorizontal className="h-[15px] w-[15px]" strokeWidth={1.75} />
          </button>
        </header>

        <div aria-hidden className="h-px bg-[var(--color-line-2)]" />

        {/* Info */}
        <div className="flex flex-col gap-3 px-4 py-3.5">
          <InfoRow k={t("agent.detail.integrations.card.appId")}>
            <span className="inline-flex items-center border border-[var(--color-line-2)] bg-[var(--color-paper-2)] px-2 py-1 font-[var(--font-mono)] text-[12px] text-[var(--color-ink-2)]">
              {app.appId}
            </span>
          </InfoRow>
          <InfoRow k={t("agent.detail.integrations.card.bound")}>
            <span className="text-[13px] text-[var(--color-muted-foreground)]">
              {app.boundValue}
            </span>
          </InfoRow>
          <InfoRow k={app.extraKey}>
            <span className="font-[var(--font-mono)] text-[12px] text-[var(--color-muted-foreground)]">
              {app.extraValue}
            </span>
          </InfoRow>
        </div>

        <div className="flex-1" />

        {/* Foot */}
        <footer className="flex items-center justify-between gap-2 border-t border-[var(--color-line-2)] bg-[var(--color-paper-2)] px-4 py-3">
          <span className="flex items-center gap-1.5 text-[11px] text-[var(--color-muted-foreground)]">
            <Radio
              className={
                app.live
                  ? "h-3.5 w-3.5 text-[var(--color-moss)]"
                  : "h-3.5 w-3.5 text-[var(--color-fg-muted)]"
              }
              strokeWidth={1.75}
            />
            <span className="font-[var(--font-mono)]">
              {app.live
                ? t("agent.detail.integrations.card.live")
                : t("agent.detail.integrations.card.awaiting")}
            </span>
          </span>
          {canManage ? (
            <button
              type="button"
              onClick={onRemove}
              data-testid="integration-remove"
              className="inline-flex cursor-pointer items-center gap-1.5 border border-[var(--color-rose)] bg-[var(--color-card)] px-[11px] py-[7px] text-[13px] font-medium text-[var(--color-rose)] transition-colors duration-150 ease-out hover:bg-[var(--color-rose-soft)]"
            >
              <Trash2 className="h-3 w-3" strokeWidth={1.75} />
              {t("agent.detail.integrations.card.remove")}
            </button>
          ) : null}
        </footer>
      </section>
    );
  }

  // Disconnected
  return (
    <section
      className="flex flex-col overflow-hidden border border-[var(--color-line-2)] bg-[var(--color-card)]"
      data-testid="integration-card"
      data-state="disconnected"
    >
      <div className="flex flex-1 flex-col items-center justify-center gap-2.5 px-5 pt-7 pb-4 text-center">
        <LogoTile logo={logo} name={name} size={64} img={30} />
        <span className="font-[var(--font-display)] text-[18px] font-bold text-[var(--color-ink-2)]">
          {name}
        </span>
        <p className="max-w-[34ch] text-[13px] leading-[1.45] text-[var(--color-muted-foreground)]">
          {desc}
        </p>
        <span className="flex items-center gap-1.5 text-[12px] font-medium text-[var(--color-moss)]">
          <BookOpen className="h-3.5 w-3.5" strokeWidth={1.75} />
          {t("agent.detail.integrations.card.guide")}
        </span>
      </div>
      <div className="px-4 pt-3 pb-4">
        {canManage ? (
          <button
            type="button"
            onClick={onConnect}
            data-testid="integration-connect"
            className="flex w-full cursor-pointer items-center justify-center gap-2 bg-[var(--color-moss)] px-4 py-2.5 text-[14px] font-semibold text-white transition-colors duration-150 ease-out hover:bg-[var(--color-moss-deep)]"
          >
            <Plus className="h-[15px] w-[15px]" strokeWidth={2} />
            {t("agent.detail.integrations.card.connect", { name })}
          </button>
        ) : (
          <p className="py-1.5 text-center font-[var(--font-mono)] text-[11px] text-[var(--color-fg-muted)]">
            {t("agent.detail.integrations.memberOnly")}
          </p>
        )}
      </div>
    </section>
  );
}

function InfoRow({ k, children }: { k: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center gap-2.5">
      <span className="w-[84px] shrink-0 font-[var(--font-mono)] text-[10px] tracking-[0.1em] text-[var(--color-fg-muted)] uppercase">
        {k}
      </span>
      <div className="min-w-0">{children}</div>
    </div>
  );
}

function LogoTile({
  logo,
  name,
  size,
  img,
}: {
  logo: string;
  name: string;
  size: number;
  img: number;
}) {
  return (
    <span
      aria-hidden
      className="flex shrink-0 items-center justify-center border border-[var(--color-line-2)] bg-[var(--color-paper-2)]"
      style={{ width: size, height: size }}
    >
      <img src={logo} alt={name} style={{ width: img, height: img }} />
    </span>
  );
}
