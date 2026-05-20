import { useEffect, useMemo, useRef, useState } from "react";
import { Check, ExternalLink, Plug } from "lucide-react";
import { Monogram } from "../atoms/Monogram";
import { useT } from "../../i18n";
import {
  useCreateMcpServer,
  useMcpServers,
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

// Set before the OAuth nav, consumed on bounce-back so the auto-resume
// fires exactly once across the page reload. TTL bounds stale leftovers
// from abandoned auths.
const RESUME_MARKER_PREFIX = "relay:wire-resume:";
const RESUME_TTL_MS = 15 * 60 * 1000;

function markResumeTarget(catalogId: string): void {
  try {
    sessionStorage.setItem(
      RESUME_MARKER_PREFIX + catalogId,
      String(Date.now()),
    );
  } catch {
    // Private-mode / sandboxed contexts can throw — degrade to no auto-resume.
  }
}

function consumeResumeMarker(catalogId: string): boolean {
  try {
    const key = RESUME_MARKER_PREFIX + catalogId;
    const v = sessionStorage.getItem(key);
    if (!v) return false;
    sessionStorage.removeItem(key);
    const ts = Number(v);
    if (!Number.isFinite(ts)) return false;
    return Date.now() - ts <= RESUME_TTL_MS;
  } catch {
    return false;
  }
}

/** Inline click-to-wire card rendered inside an agent's reply bubble in
 *  response to a `request_user_wire_mcp` tool call. `oauth2` runs the
 *  full create + `oauth/start` + redirect path; `static_headers` /
 *  `none` stop at create and finish from the connections page.
 *
 *  `onConnected` fires once when the catalog_id transitions to wired
 *  AND we initiated the wire in this card (gated by an in-session ref
 *  for the non-OAuth path plus a sessionStorage marker that survives
 *  the OAuth bounce) — so already-wired cards loaded from history
 *  never trigger spurious follow-ups. */
export function WireMcpRequestCard({
  entry,
  callbackUrl,
  onConnected,
}: {
  entry: McpWireRequest;
  /** Where the OAuth flow should return the user after vendor consent.
   *  Defaults to the current location. */
  callbackUrl?: (serverId: string) => string;
  /** Fires once after the catalog_id transitions to wired AND this card
   *  initiated the wire (mid-session for static_headers / none, or
   *  before the OAuth bounce for oauth2). Parents typically respond by
   *  submitting an auto-resume prompt back to the same thread. */
  onConnected?: (entry: McpWireRequest) => void;
}) {
  const { t } = useT();
  const create = useCreateMcpServer();
  const startOAuth = useStartOAuth();
  const servers = useMcpServers();
  const visual = entryById(entry.catalog_id);
  const homepageUrl = safeHttpUrl(entry.homepage_url);
  const submitting = create.isPending || startOAuth.isPending;
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);

  // initiatedRef covers the non-OAuth path (ref survives in-tab);
  // OAuth additionally writes the sessionStorage marker since the ref
  // dies in the page reload.
  const initiatedRef = useRef(false);
  const firedRef = useRef(false);

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

  useEffect(() => {
    if (firedRef.current) return;
    if (!wired) return;
    const fromMarker = consumeResumeMarker(entry.catalog_id);
    if (!initiatedRef.current && !fromMarker) return;
    firedRef.current = true;
    onConnected?.(entry);
  }, [wired, entry, onConnected]);

  const onConnect = async () => {
    setError(null);
    try {
      initiatedRef.current = true;
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
        // Set the marker before the redirect so the freshly-mounted
        // card on bounce-back fires onConnected exactly once.
        markResumeTarget(entry.catalog_id);
        window.location.href = res.authorize_url;
        return;
      }
      // static_headers / none land in AuthPending — show the "finish
      // setup in Connections" hint until the user wires credentials.
      setDone(true);
    } catch (e) {
      initiatedRef.current = false;
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
