import { useState } from "react";
import { ExternalLink, Plug } from "lucide-react";
import { Monogram } from "../atoms/Monogram";
import { useT } from "../../i18n";
import {
  useCreateMcpServer,
  useStartOAuth,
} from "../../hooks/useMcpServers";
import { entryById } from "../../data/mcpCatalog";
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
 *  response to a `request_user_wire_mcp` tool call.
 *
 *  Connect flow:
 *    1. `POST /mcp-servers {catalog_id}` mints the `mcp_servers` row
 *       from catalog defaults.
 *    2. For `auth_kind === "oauth2"`, kick `POST /mcp-servers/:id/oauth/start`
 *       and redirect to the returned authorize URL — the OAuth round-trip
 *       lands on `GET /mcp-oauth/callback`.
 *    3. `static_headers` / `none` finish at step 1; the user completes
 *       setup from the connections page. */
export function WireMcpRequestCard({
  entry,
  callbackUrl,
}: {
  entry: McpWireRequest;
  /** Where the OAuth flow should return the user after vendor consent.
   *  Defaults to the current location. */
  callbackUrl?: (serverId: string) => string;
}) {
  const { t } = useT();
  const create = useCreateMcpServer();
  const startOAuth = useStartOAuth();
  const visual = entryById(entry.catalog_id);
  // Catalog rows are operator-controlled today, but defense in depth —
  // refuse to render anything other than http/https on the user-clickable
  // "Learn more" link so a future bad row can't smuggle a `javascript:`
  // URL into the chat surface.
  const homepageUrl = safeHttpUrl(entry.homepage_url);
  const submitting = create.isPending || startOAuth.isPending;
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);

  const onConnect = async () => {
    setError(null);
    try {
      const server = await create.mutateAsync({
        catalog_id: entry.catalog_id,
      });
      if (entry.auth_kind === "oauth2") {
        const res = await startOAuth.mutateAsync({
          id: server.id,
          input: {
            redirect_to: callbackUrl
              ? callbackUrl(server.id)
              : window.location.pathname,
          },
        });
        window.location.href = res.authorize_url;
        return;
      }
      // static_headers / none: server row created in AuthPending /
      // unwired state. The connections page handles the rest; surface a
      // gentle "open settings" hint here.
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
        <Monogram
          name={entry.display_name}
          id={entry.catalog_id}
          size={28}
          bg={visual?.tileBg}
          fg={visual?.tileFg}
          glyph={visual?.monogram ?? entry.display_name[0]?.toUpperCase() ?? "?"}
          iconSlug={visual?.iconSlug}
        />
        <div className="flex-1">
          <div className="flex items-center gap-2">
            <span className="font-[var(--font-display)] text-[13px] font-semibold text-[var(--color-ink)]">
              {entry.display_name}
            </span>
            <span className="border border-[var(--color-moss)] px-1 font-[var(--font-mono)] text-[9.5px] font-bold uppercase tracking-[0.14em] text-[var(--color-moss)]">
              {t("thread.wireRequest.badge")}
            </span>
          </div>
          {homepageUrl ? (
            <a
              href={homepageUrl}
              target="_blank"
              rel="noopener noreferrer"
              className="mt-0.5 inline-flex items-center gap-1 font-[var(--font-mono)] text-[11px] text-[var(--color-muted)] hover:text-[var(--color-ink)]"
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
        {done ? (
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
