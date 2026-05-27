import { useMemo, useState } from "react";
import { Check, ExternalLink, Plug } from "lucide-react";
import { CatalogIcon } from "../atoms/CatalogIcon";
import { useT } from "../../i18n";
import {
  useCatalogLookup,
  useCreateMcpServer,
  useMcpServers,
  useStartOAuth,
} from "../../hooks/useMcpServers";
import type { McpWireRequest } from "../../types/api";

/** Parse a URL and only return it if the scheme is http(s). Anything
 *  else — `javascript:`, `data:`, malformed strings — collapses to
 *  `null` so the renderer drops the link entirely. */
function safeHttpUrl(raw: string | undefined): string | null {
  if (!raw) return null;
  try {
    const u = new URL(raw);
    return u.protocol === "https:" || u.protocol === "http:" ? u.toString() : null;
  } catch {
    return null;
  }
}

/** Inline click-to-wire card rendered inside an agent's reply bubble in
 *  response to a `request_user_wire_mcp` tool call. `oauth2` runs the
 *  full create + `oauth/start` + redirect path; `static_headers` /
 *  `none` stop at create and finish from the connections page.
 *
 *  Auto-resume after a successful OAuth is server-driven (the
 *  `mcp-oauth/callback` handler enqueues a synthetic continuation
 *  prompt directly), so this card no longer carries an `onConnected`
 *  callback — the agent's next response arrives on the existing
 *  thread stream. The card just polls `useMcpServers` and flips its
 *  visual state when the row is wired. */
export function WireMcpRequestCard({
  entry,
  callbackUrl,
  sessionId,
  agentId,
}: {
  entry: McpWireRequest;
  /** Where the OAuth flow should return the user after vendor consent.
   *  Defaults to the current location. */
  callbackUrl?: (serverId: string) => string;
  /** Resume context passed to `POST /mcp-servers/{id}/oauth/start` so
   *  the callback's universal auto-continue knows which session +
   *  agent to inject the synthetic prompt into. Both must be present
   *  or both absent; the BE returns 400 otherwise. */
  sessionId?: string | null;
  agentId?: string | null;
}) {
  const { t } = useT();
  const create = useCreateMcpServer();
  const startOAuth = useStartOAuth();
  const servers = useMcpServers();
  const visual = useCatalogLookup()(entry.catalog_id);
  const homepageUrl = safeHttpUrl(entry.homepage_url);
  const submitting = create.isPending || startOAuth.isPending;
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);

  const wired = useMemo(
    () =>
      (servers.data ?? []).some(
        (s) =>
          s.catalog_id === entry.catalog_id &&
          s.has_credentials &&
          s.connection_status === "ok",
      ),
    [servers.data, entry.catalog_id],
  );

  const onConnect = async () => {
    setError(null);
    try {
      const server = await create.mutateAsync({
        catalog_id: entry.catalog_id,
      });
      if (entry.auth_kind === "oauth2") {
        // Both-or-neither for resume context. Forwarding only when both
        // are present matches the BE's both-or-neither validation; in
        // the rare path where one is missing (e.g. catalog-page manual
        // wiring) the callback simply skips auto-continue.
        const hasResumeCtx = !!sessionId && !!agentId;
        const res = await startOAuth.mutateAsync({
          id: server.id,
          input: {
            redirect_to: callbackUrl
              ? callbackUrl(server.id)
              : window.location.pathname,
            ...(hasResumeCtx
              ? { session_id: sessionId, agent_id: agentId }
              : {}),
          },
        });
        window.location.href = res.authorize_url;
        return;
      }
      // static_headers / none land in AuthPending — show the "finish
      // setup in Connections" hint until the user wires credentials.
      setDone(true);
    } catch (e) {
      setError(
        e instanceof Error
          ? e.message
          : t("thread.wireRequest.error.generic"),
      );
    }
  };

  return (
    <aside
      className="mt-3 border border-[var(--color-line)] bg-[var(--color-paper-2)] px-4 py-3"
      data-testid="wire-mcp-request"
    >
      <header className="flex items-center gap-2.5">
        <CatalogIcon
          name={entry.display_name}
          iconUrl={visual?.icon_url}
          size={28}
        />
        <div className="flex-1">
          <div className="flex items-center gap-2">
            <span className="font-[var(--font-display)] text-[13px] font-semibold text-[var(--color-ink)]">
              {entry.display_name}
            </span>
            <span
              className={
                wired
                  ? "border border-[var(--color-moss-deep)] bg-[var(--color-moss-deep)] px-1 font-[var(--font-mono)] text-[9.5px] font-bold uppercase tracking-[0.14em] text-white"
                  : "border border-[var(--color-moss)] px-1 font-[var(--font-mono)] text-[9.5px] font-bold uppercase tracking-[0.14em] text-[var(--color-moss)]"
              }
            >
              {wired
                ? t("thread.wireRequest.badgeConnected")
                : t("thread.wireRequest.badge")}
            </span>
          </div>
          {homepageUrl ? (
            <a
              href={homepageUrl}
              target="_blank"
              rel="noopener noreferrer"
              className="mt-0.5 inline-flex items-center gap-1 font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)] hover:text-[var(--color-ink)]"
            >
              {t("thread.wireRequest.learnMore")}
              <ExternalLink className="h-3 w-3" aria-hidden />
            </a>
          ) : null}
        </div>
      </header>
      <p className="mt-2 text-[12.5px] leading-[1.5] text-[var(--color-ink)]">
        {entry.reason}
      </p>
      {error ? (
        <p className="mt-2 font-[var(--font-mono)] text-[11px] text-[var(--color-rose)]">
          {error}
        </p>
      ) : null}
      <div className="mt-3 flex items-center justify-end gap-2">
        {wired ? (
          <span className="inline-flex items-center gap-1.5 font-[var(--font-mono)] text-[11px] font-bold uppercase tracking-[0.1em] text-[var(--color-moss-deep)]">
            <Check className="h-3.5 w-3.5" strokeWidth={2.25} aria-hidden />
            {t("thread.wireRequest.connected", { name: entry.display_name })}
          </span>
        ) : done ? (
          <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-moss)]">
            {t("thread.wireRequest.created")}
          </span>
        ) : (
          <button
            type="button"
            disabled={submitting}
            onClick={onConnect}
            className="inline-flex items-center gap-1.5 border border-[var(--color-moss)] bg-[var(--color-moss)] px-3 py-1.5 font-[var(--font-mono)] text-[11px] font-bold uppercase tracking-[0.1em] text-white transition-colors hover:bg-[var(--color-moss-deep)] disabled:opacity-60"
          >
            <Plug className="h-3.5 w-3.5" aria-hidden />
            {submitting
              ? t("thread.wireRequest.connecting")
              : t("thread.wireRequest.connect", { name: entry.display_name })}
          </button>
        )}
      </div>
    </aside>
  );
}
