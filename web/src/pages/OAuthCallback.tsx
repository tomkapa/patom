import { useEffect, useMemo, useState } from "react";
import { ArrowLeft, Check, RefreshCw, X } from "lucide-react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { Button } from "../components/atoms/Button";
import { Spinner } from "../components/atoms/Spinner";
import { Monogram } from "../components/atoms/Monogram";
import { CatalogIcon } from "../components/atoms/CatalogIcon";
import {
  useCatalogLookup,
  useMcpServer,
  useStartOAuth,
} from "../hooks/useMcpServers";
import { useT } from "../i18n";
import type { McpCatalogEntry } from "../types/api";
import { cn } from "../lib/utils";

type View = "connecting" | "authorized" | "failed";
type DiagramState = "pending" | "ok" | "error";
type Step = "redirected" | "awaiting" | "discover";

const RELAY_TILE = { bg: "#FAF7EE", fg: "#1A2B1E", glyph: "R", label: "Relay" } as const;

const WATCHDOG_MS = 30_000;
const AUTO_REDIRECT_MS = 3_200;

const STEP_ORDER: readonly Step[] = ["redirected", "awaiting", "discover"];
const STEP_LABEL: Record<Step, Parameters<ReturnType<typeof useT>["t"]>[0]> = {
  redirected: "connections.callback.steps.redirected",
  awaiting: "connections.callback.steps.awaiting",
  discover: "connections.callback.steps.discover",
};

/** Receives backend redirects from `/mcp-oauth/callback`. Reads
 *  `?status=ok|failed&reason=…&server_id=…` and either polls the server
 *  row until the BE callback finishes its writes (`ok`) or renders the
 *  failure card directly. */
export function OAuthCallback() {
  const nav = useNavigate();
  const [params] = useSearchParams();
  const serverId = params.get("server_id");
  const initialStatus = params.get("status");
  const reason = params.get("reason");
  // No `server_id` means the backend redirect didn't include one — we
  // can't poll our way to "authorized", so render Failed immediately
  // rather than spinning until the 30s watchdog.
  const [view, setView] = useState<View>(
    !serverId || initialStatus === "failed" ? "failed" : "connecting",
  );

  const polling = useMcpServer(serverId, {
    refetchInterval: view === "connecting" ? 800 : false,
    enabled: Boolean(serverId),
  });
  const catalogLookup = useCatalogLookup();

  useEffect(() => {
    if (view !== "connecting") return;
    const s = polling.data;
    if (s?.has_credentials && s.connection_status === "ok") setView("authorized");
  }, [polling.dataUpdatedAt, polling.data, view]);

  // Standalone watchdog — relying on the polling effect alone would miss
  // cases where the query never returns (network drop, 404).
  useEffect(() => {
    if (view !== "connecting") return;
    const id = window.setTimeout(() => setView("failed"), WATCHDOG_MS);
    return () => window.clearTimeout(id);
  }, [view]);

  const entry = polling.data
    ? catalogLookup(polling.data.catalog_id)
    : undefined;
  const name = entry?.display_name ?? polling.data?.catalog_id ?? "the provider";

  if (view === "failed") {
    return (
      <CallbackShell>
        <FailedView
          name={name}
          entry={entry}
          reason={reason}
          referenceId={serverId ?? "—"}
          url={polling.data?.config.url ?? "—"}
          onBack={() => nav("/connections/catalog")}
          serverId={serverId}
        />
      </CallbackShell>
    );
  }
  if (view === "authorized") {
    return (
      <CallbackShell>
        <AuthorizedView
          name={name}
          entry={entry}
          toolsCount={polling.data?.discovered_tools?.length ?? 0}
          onDone={() => nav("/connections")}
        />
      </CallbackShell>
    );
  }
  return (
    <CallbackShell>
      <ConnectingView name={name} entry={entry} />
    </CallbackShell>
  );
}

function CallbackShell({ children }: { children: React.ReactNode }) {
  return (
    <main className="mx-auto flex h-screen w-full max-w-[640px] flex-col items-center justify-center gap-10 bg-[var(--color-paper)] px-6 py-12">
      {children}
    </main>
  );
}

function ConnectingView({
  name,
  entry,
}: {
  name: string;
  entry: McpCatalogEntry | undefined;
}) {
  const { t } = useT();
  return (
    <>
      <Eyebrow tone="muted">{t("connections.callback.eyebrow.connecting")}</Eyebrow>
      <Diagram vendor={vendorTile(entry, name)} state="pending" />
      <div className="flex flex-col items-center gap-2 text-center">
        <h1 className="font-[var(--font-display)] text-[22px] font-semibold text-[var(--color-ink)]">
          {t("connections.callback.connecting.title", { name })}
        </h1>
        <p className="text-[13.5px] text-[var(--color-muted)]">
          {t("connections.callback.connecting.body", { name })}
        </p>
      </div>
      <Progress />
      <StepRow active="awaiting" />
    </>
  );
}

function AuthorizedView({
  name,
  entry,
  toolsCount,
  onDone,
}: {
  name: string;
  entry: McpCatalogEntry | undefined;
  toolsCount: number;
  onDone: () => void;
}) {
  const { t } = useT();
  const [secondsLeft, setSecondsLeft] = useState(Math.ceil(AUTO_REDIRECT_MS / 1000));

  useEffect(() => {
    const id = window.setInterval(() => {
      setSecondsLeft((n) => Math.max(0, n - 1));
    }, 1000);
    const done = window.setTimeout(onDone, AUTO_REDIRECT_MS);
    return () => {
      window.clearInterval(id);
      window.clearTimeout(done);
    };
  }, [onDone]);

  return (
    <>
      <Eyebrow tone="muted">{t("connections.callback.eyebrow.authorized")}</Eyebrow>
      <Diagram vendor={vendorTile(entry, name)} state="ok" />
      <div className="flex flex-col items-center gap-2.5 text-center">
        <h1 className="font-[var(--font-display)] text-[22px] font-semibold text-[var(--color-ink)]">
          {t("connections.callback.authorized.title", { name })}
        </h1>
        <div className="flex items-center gap-2 text-[13px] text-[var(--color-muted)]">
          <Spinner size={12} />
          <span>{t("connections.callback.authorized.discovering")}</span>
        </div>
      </div>
      <div className="flex w-full max-w-[420px] items-center gap-3 border border-[var(--color-line)] bg-[var(--color-paper-2)] px-4 py-3">
        <Check className="h-3.5 w-3.5 shrink-0 text-[var(--color-moss)]" strokeWidth={2} />
        <p className="min-w-0 flex-1 text-[11.5px] text-[var(--color-muted)]">
          {t("connections.callback.authorized.body", { count: toolsCount })}
        </p>
      </div>
      <div className="flex flex-col items-center gap-1.5 text-center">
        <div className="font-[var(--font-mono)] text-[11.5px] text-[var(--color-moss-deep)]">
          {t("connections.callback.authorized.redirect", { seconds: secondsLeft })}
        </div>
        <button
          type="button"
          onClick={onDone}
          className="font-[var(--font-mono)] text-[12px] text-[var(--color-ink)] underline hover:no-underline"
        >
          {t("connections.callback.authorized.goNow")}
        </button>
      </div>
    </>
  );
}

function FailedView({
  name,
  entry,
  reason,
  referenceId,
  url,
  onBack,
  serverId,
}: {
  name: string;
  entry: McpCatalogEntry | undefined;
  reason: string | null;
  referenceId: string;
  url: string;
  onBack: () => void;
  serverId: string | null;
}) {
  const { t } = useT();
  const startOAuth = useStartOAuth();
  const message = useMemo(() => {
    if (reason === "access_denied") return t("connections.callback.failed.bodyDenied", { name });
    return t("connections.callback.failed.bodyGeneric");
  }, [reason, name, t]);

  async function handleRetry() {
    if (!serverId) return;
    try {
      const res = await startOAuth.mutateAsync({
        id: serverId,
        input: { redirect_to: `/connections/oauth-callback?server_id=${serverId}` },
      });
      window.location.href = res.authorize_url;
    } catch {
      // The button's loading state already surfaces the failure; the
      // user can retry. Silent — Banner here would compete with the
      // existing diagnostic card.
    }
  }

  return (
    <>
      <Eyebrow tone="rose">{t("connections.callback.eyebrow.failed")}</Eyebrow>
      <Diagram vendor={vendorTile(entry, name)} state="error" />
      <div className="flex flex-col items-center gap-2 text-center">
        <h1 className="font-[var(--font-display)] text-[22px] font-semibold text-[var(--color-ink)]">
          {t("connections.callback.failed.title")}
        </h1>
        <p className="text-[13.5px] text-[var(--color-muted)]">{message}</p>
      </div>
      <dl className="flex w-full max-w-[480px] flex-col gap-1.5 border border-[var(--color-line)] bg-[var(--color-paper-2)] px-4 py-3 font-[var(--font-mono)] text-[11.5px]">
        <DiagItem label={t("connections.callback.failed.options")} value={url} />
        <DiagItem
          label={t("connections.callback.failed.response")}
          value={reason ? `error: ${reason}` : "access_denied"}
          tone="rose"
        />
        <DiagItem label={t("connections.callback.failed.reference")} value={referenceId} />
      </dl>
      <div className="flex items-center gap-3">
        <Button variant="ghost" onClick={onBack}>
          <ArrowLeft className="h-3.5 w-3.5" strokeWidth={2} />
          {t("connections.callback.failed.back")}
        </Button>
        <Button
          variant="primary"
          onClick={handleRetry}
          loading={startOAuth.isPending}
          disabled={!serverId}
          data-testid="failed-retry"
        >
          <RefreshCw className="h-3.5 w-3.5" strokeWidth={2} />
          {t("connections.callback.failed.retry")}
        </Button>
      </div>
    </>
  );
}

type RelayTile = { bg: string; fg: string; glyph: string; label: string };

type VendorTile = {
  label: "Vendor";
  name: string;
  iconUrl: string | null;
};

function vendorTile(
  entry: McpCatalogEntry | undefined,
  name: string,
): VendorTile {
  return { label: "Vendor", name, iconUrl: entry?.icon_url ?? null };
}

function Eyebrow({
  children,
  tone,
}: {
  children: React.ReactNode;
  tone: "muted" | "rose";
}) {
  return (
    <div
      className={cn(
        "font-[var(--font-mono)] text-[11px] tracking-[0.18em] uppercase",
        tone === "rose" ? "text-[var(--color-rose)]" : "text-[var(--color-muted)]",
      )}
    >
      {children}
    </div>
  );
}

function DiagItem({
  label,
  value,
  tone = "ink",
}: {
  label: string;
  value: string;
  tone?: "ink" | "rose";
}) {
  return (
    <div className="flex items-center justify-between gap-3">
      <span className="text-[var(--color-muted)]">{label}</span>
      <span
        className={
          tone === "rose" ? "text-[var(--color-rose)]" : "text-[var(--color-ink)]"
        }
      >
        {value}
      </span>
    </div>
  );
}

function Diagram({
  vendor,
  state,
}: {
  vendor: VendorTile;
  state: DiagramState;
}) {
  return (
    <div className="flex items-center gap-5">
      <RelayTileView tile={RELAY_TILE} />
      <div className="flex w-[120px] items-center">
        <div className="h-px flex-1 bg-[var(--color-line)]" />
        <Hub state={state} />
        <div className="h-px flex-1 bg-[var(--color-line)]" />
      </div>
      <VendorTileView tile={vendor} />
    </div>
  );
}

function RelayTileView({ tile }: { tile: RelayTile }) {
  return (
    <div className="flex flex-col items-center gap-2">
      <Monogram
        name={tile.label}
        size={68}
        bg={tile.bg}
        fg={tile.fg}
        glyph={tile.glyph}
        className="border border-[var(--color-line)]"
      />
      <span className="font-[var(--font-mono)] text-[10px] tracking-[0.16em] text-[var(--color-muted)] uppercase">
        {tile.label}
      </span>
    </div>
  );
}

function VendorTileView({ tile }: { tile: VendorTile }) {
  return (
    <div className="flex flex-col items-center gap-2">
      <CatalogIcon name={tile.name} iconUrl={tile.iconUrl} size={68} />
      <span className="font-[var(--font-mono)] text-[10px] tracking-[0.16em] text-[var(--color-muted)] uppercase">
        {tile.label}
      </span>
    </div>
  );
}

const HUB_STYLE: Record<DiagramState, { className: string; icon: React.ReactNode }> = {
  pending: {
    className: "bg-[var(--color-card)] text-[var(--color-moss)] border-[var(--color-line)]",
    icon: <Spinner size={14} />,
  },
  ok: {
    className: "bg-[var(--color-moss)] text-white border-[var(--color-moss)]",
    icon: <Check className="h-4 w-4" strokeWidth={2.25} />,
  },
  error: {
    className: "bg-[var(--color-rose)] text-white border-[var(--color-rose)]",
    icon: <X className="h-4 w-4" strokeWidth={2.25} />,
  },
};

function Hub({ state }: { state: DiagramState }) {
  const { className, icon } = HUB_STYLE[state];
  return (
    <div
      className={cn(
        "flex h-8 w-8 shrink-0 items-center justify-center rounded-full border",
        className,
      )}
    >
      {icon}
    </div>
  );
}

function Progress() {
  return (
    <div className="h-1 w-[360px] overflow-hidden bg-[var(--color-paper-2)]">
      <div className="h-full w-1/3 animate-pulse bg-[var(--color-moss)]" />
    </div>
  );
}

function StepRow({ active }: { active: Step }) {
  const { t } = useT();
  const activeIdx = STEP_ORDER.indexOf(active);
  return (
    <div className="flex items-center gap-5 font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]">
      {STEP_ORDER.map((id, idx) => {
        const done = idx < activeIdx;
        const current = idx === activeIdx;
        return (
          <span key={id} className="flex items-center gap-1.5">
            <span
              aria-hidden
              className={cn(
                "inline-block h-1.5 w-1.5 rounded-full",
                done
                  ? "bg-[var(--color-moss)]"
                  : current
                    ? "bg-[var(--color-amber)]"
                    : "bg-[var(--color-line-2)]",
              )}
            />
            <span
              className={cn(
                current && "text-[var(--color-ink)]",
                done && "text-[var(--color-moss-deep)]",
              )}
            >
              {t(STEP_LABEL[id])}
            </span>
          </span>
        );
      })}
    </div>
  );
}
