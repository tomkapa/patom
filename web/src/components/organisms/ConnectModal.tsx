import { useCallback, useMemo, useState } from "react";
import { ArrowRight, Check, RefreshCw, TriangleAlert } from "lucide-react";
import { Button } from "../atoms/Button";
import { Monogram } from "../atoms/Monogram";
import { Modal, ModalFooter, ModalHeader } from "../molecules/Modal";
import { Banner } from "../molecules/Banner";
import {
  entryForServer,
  type CatalogEntry,
} from "../../data/mcpCatalog";
import {
  useCreateMcpServer,
  useStartOAuth,
  useTestConnect,
} from "../../hooks/useMcpServers";
import { formatError } from "../../lib/errors";
import { useT } from "../../i18n";
import type { McpServer } from "../../types/api";

const callbackUrl = (serverId: string) =>
  `/connections/oauth-callback?server_id=${serverId}`;

/** Wraps an async submit handler with shared `errorText` state. Caller
 *  passes an async fn; failures are caught and surfaced via the returned
 *  banner string. Keeps the four modal bodies from each repeating the
 *  same try/catch + useState pair. */
function useAsyncSubmit() {
  const [errorText, setErrorText] = useState<string | null>(null);
  const run = useCallback(async (fn: () => Promise<void>) => {
    setErrorText(null);
    try {
      await fn();
    } catch (e) {
      setErrorText(formatError(e));
    }
  }, []);
  return { errorText, setErrorText, run };
}

type Mode = "oauth" | "apiToken" | "reconnect" | "customUrl";

type Props =
  | { mode: "oauth"; entry: CatalogEntry; onClose: () => void; server?: never }
  | {
      mode: "apiToken";
      entry: CatalogEntry;
      onClose: () => void;
      server?: never;
    }
  | {
      mode: "reconnect";
      server: McpServer;
      onClose: () => void;
      entry?: never;
    }
  | { mode: "customUrl"; onClose: () => void; entry?: never; server?: never };

export function ConnectModal(props: Props) {
  const width = props.mode === "customUrl" ? 500 : 460;
  return (
    <Modal
      open
      onClose={props.onClose}
      width={width}
      ariaLabel={`Connect modal (${props.mode})`}
    >
      {renderBody(props)}
    </Modal>
  );
}

function renderBody(props: Props) {
  switch (props.mode) {
    case "oauth":
      return <OAuthBody entry={props.entry} onClose={props.onClose} />;
    case "apiToken":
      return <ApiTokenBody entry={props.entry} onClose={props.onClose} />;
    case "reconnect":
      return <ReconnectBody server={props.server} onClose={props.onClose} />;
    case "customUrl":
      return <CustomUrlBody onClose={props.onClose} />;
  }
}

function EntryTile({ entry }: { entry: CatalogEntry }) {
  return (
    <Monogram
      name={entry.name}
      size={36}
      bg={entry.tileBg}
      fg={entry.tileFg}
      glyph={entry.monogram}
      iconSlug={entry.iconSlug}
    />
  );
}

function BulletList({ items }: { items: string[] }) {
  return (
    <ul className="flex flex-col gap-2.5">
      {items.map((line, i) => (
        <li
          key={i}
          className="flex items-start gap-2 text-[13px] leading-[1.45] text-[var(--color-ink)]"
        >
          <Check
            className="mt-0.5 h-3.5 w-3.5 shrink-0 text-[var(--color-moss)]"
            strokeWidth={2}
          />
          <span className="min-w-0 flex-1">{line}</span>
        </li>
      ))}
    </ul>
  );
}

function OAuthBody({
  entry,
  onClose,
}: {
  entry: CatalogEntry;
  onClose: () => void;
}) {
  const { t } = useT();
  const create = useCreateMcpServer();
  const startOAuth = useStartOAuth();
  const submitting = create.isPending || startOAuth.isPending;
  const { errorText, run } = useAsyncSubmit();

  const onContinue = () =>
    run(async () => {
      // Short-form when the FE doesn't know the URL (tenant-custom catalog
      // entry surfaced from the backend); full-form when it does, so the
      // operator's tile-level expectation matches the persisted row.
      const server = await create.mutateAsync({
        catalog_id: entry.id,
        config: entry.defaultUrl
          ? { type: "http", url: entry.defaultUrl }
          : undefined,
        description: entry.blurb,
        enabled: true,
      });
      const res = await startOAuth.mutateAsync({
        id: server.id,
        input: { redirect_to: callbackUrl(server.id) },
      });
      window.location.href = res.authorize_url;
    });

  return (
    <>
      <ModalHeader
        eyebrow={`${t("connections.modal.oauth.eyebrow")} · ${entry.name}`}
        title={t("connections.modal.oauth.title", { name: entry.name })}
        icon={<EntryTile entry={entry} />}
        onClose={onClose}
      />
      <div className="flex flex-col gap-3 px-5 py-5">
        <BulletList
          items={[
            t("connections.modal.oauth.bullet1"),
            t("connections.modal.oauth.bullet2"),
            t("connections.modal.oauth.bullet3", { name: entry.name }),
          ]}
        />
        {errorText ? <Banner variant="rose">{errorText}</Banner> : null}
      </div>
      <ModalFooter>
        <Button variant="ghost" onClick={onClose}>
          {t("connections.modal.cancel")}
        </Button>
        <Button
          variant="primary"
          onClick={onContinue}
          loading={submitting}
          data-testid="oauth-continue"
        >
          {t("connections.modal.oauth.continue", { name: entry.name })}
          <ArrowRight className="h-3.5 w-3.5" strokeWidth={2} />
        </Button>
      </ModalFooter>
    </>
  );
}

function ApiTokenBody({
  entry,
  onClose,
}: {
  entry: CatalogEntry;
  onClose: () => void;
}) {
  const { t } = useT();
  const create = useCreateMcpServer();
  const test = useTestConnect();
  const [token, setToken] = useState("");
  const { errorText, setErrorText, run } = useAsyncSubmit();
  const submitting = create.isPending || test.isPending;

  const onConnect = () =>
    run(async () => {
      if (!token.trim()) {
        setErrorText(t("connections.modal.token.error", { name: entry.name }));
        return;
      }
      const headerName = entry.apiTokenHeader ?? "Authorization";
      const credentials = {
        kind: "static_headers" as const,
        headers: { [headerName]: `${entry.apiTokenPrefix ?? ""}${token}` },
      };
      const result = await test.mutateAsync({
        config: { type: "http", url: entry.defaultUrl },
        credentials,
      });
      if (result.outcome === "failed") {
        setErrorText(result.error);
        return;
      }
      await create.mutateAsync({
        catalog_id: entry.id,
        config: { type: "http", url: entry.defaultUrl },
        description: entry.blurb,
        enabled: true,
        credentials,
      });
      onClose();
    });

  return (
    <>
      <ModalHeader
        eyebrow={`${t("connections.modal.oauth.eyebrow")} · ${entry.name}`}
        title={t("connections.modal.oauth.title", { name: entry.name })}
        icon={<EntryTile entry={entry} />}
        onClose={onClose}
      />
      <div className="flex flex-col gap-4 px-5 py-5">
        <BulletList
          items={[
            t("connections.modal.oauth.bullet1"),
            t("connections.modal.oauth.bullet2"),
          ]}
        />
        <div className="flex flex-col gap-1.5">
          <div className="flex items-center justify-between gap-2">
            <label
              htmlFor="mcp-token"
              className="text-[12px] font-semibold text-[var(--color-ink)]"
            >
              {t("connections.modal.token.tokenLabel", { name: entry.name })}
            </label>
            {entry.apiTokenHelpUrl ? (
              <a
                href={entry.apiTokenHelpUrl}
                target="_blank"
                rel="noreferrer"
                className="text-[11.5px] text-[var(--color-moss-deep)] hover:underline"
              >
                {t("connections.modal.token.help")} ↗
              </a>
            ) : null}
          </div>
          <input
            id="mcp-token"
            type="password"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            placeholder={t("connections.modal.token.placeholder", {
              name: entry.name,
            })}
            className="w-full border border-[var(--color-line)] bg-[var(--color-card)] px-3 py-2 font-[var(--font-mono)] text-[12.5px] text-[var(--color-ink)] outline-none focus:border-[var(--color-moss)]"
            autoComplete="off"
          />
          <span className="text-[11px] text-[var(--color-muted)]">
            {t("connections.modal.token.note")}
          </span>
        </div>
        {errorText ? <Banner variant="rose">{errorText}</Banner> : null}
      </div>
      <ModalFooter>
        <Button variant="ghost" onClick={onClose}>
          {t("connections.modal.cancel")}
        </Button>
        <Button
          variant="primary"
          onClick={onConnect}
          loading={submitting}
          data-testid="token-connect"
        >
          {submitting
            ? t("connections.modal.token.testing")
            : t("connections.modal.token.connect")}
        </Button>
      </ModalFooter>
    </>
  );
}

function ReconnectBody({
  server,
  onClose,
}: {
  server: McpServer;
  onClose: () => void;
}) {
  const { t } = useT();
  const entry = entryForServer(server);
  const name = entry?.name ?? server.catalog_id;
  const startOAuth = useStartOAuth();
  const { errorText, run } = useAsyncSubmit();

  const onReconnect = () =>
    run(async () => {
      const res = await startOAuth.mutateAsync({
        id: server.id,
        input: { redirect_to: callbackUrl(server.id) },
      });
      window.location.href = res.authorize_url;
    });

  return (
    <>
      <ModalHeader
        eyebrow={
          <span className="flex items-center gap-2">
            <span
              aria-hidden
              className="inline-block h-2 w-2 rounded-full bg-[var(--color-amber)]"
            />
            <span className="text-[var(--color-amber)]">
              {t("connections.modal.reconnect.eyebrow")}
            </span>
            <span aria-hidden>·</span>
            <span>{name}</span>
          </span>
        }
        title={t("connections.modal.reconnect.title", { name })}
        icon={
          entry ? (
            <EntryTile entry={entry} />
          ) : (
            <Monogram
              name={server.catalog_id}
              size={36}
              bg="var(--color-rail)"
              fg="#fff"
              glyph={(server.catalog_id[0] ?? "?").toUpperCase()}
            />
          )
        }
        onClose={onClose}
      />
      <div className="flex flex-col gap-4 px-5 py-5">
        <Banner
          variant="amber"
          icon={<TriangleAlert className="h-3.5 w-3.5" strokeWidth={1.75} />}
        >
          <div className="font-semibold">
            {t("connections.modal.reconnect.alertTitle", { name })}
          </div>
          <div className="opacity-90">
            {server.last_error ?? t("connections.modal.reconnect.alertBody")}
          </div>
        </Banner>
        <dl className="flex flex-col gap-1.5 bg-[var(--color-paper-2)] px-3.5 py-3 font-[var(--font-mono)] text-[12px]">
          <DiagRow
            label={t("connections.modal.reconnect.lastSeen")}
            value={
              server.last_seen_at
                ? new Date(server.last_seen_at).toLocaleString()
                : t("connections.row.never")
            }
          />
          <DiagRow
            label={t("connections.modal.reconnect.upstream")}
            value={server.last_error ?? "—"}
            highlight
          />
        </dl>
        {errorText ? <Banner variant="rose">{errorText}</Banner> : null}
      </div>
      <ModalFooter>
        <Button variant="ghost" onClick={onClose}>
          {t("connections.modal.reconnect.notNow")}
        </Button>
        <Button
          variant="primary"
          onClick={onReconnect}
          loading={startOAuth.isPending}
          className="bg-[var(--color-amber)] border-[var(--color-amber)] hover:bg-[var(--color-amber)]"
          data-testid="reconnect-cta"
        >
          <RefreshCw className="h-3.5 w-3.5" strokeWidth={2} />
          {t("connections.modal.reconnect.cta", { name })}
        </Button>
      </ModalFooter>
    </>
  );
}

function DiagRow({
  label,
  value,
  highlight,
}: {
  label: string;
  value: string;
  highlight?: boolean;
}) {
  return (
    <div className="flex items-center justify-between gap-3">
      <span className="text-[var(--color-muted)]">{label}</span>
      <span
        className={
          highlight ? "text-[var(--color-amber)]" : "text-[var(--color-ink)]"
        }
      >
        {value}
      </span>
    </div>
  );
}

// Catalog-id charset/length mirror src/mcp/types.rs::McpCatalogId.
// The custom-URL form requires the user to type an id that matches an
// existing tenant-custom `mcp_catalog` row — the backend FK trigger
// rejects an unknown id with HTTP 400 (`CatalogIdUnknown`).
const CATALOG_ID_RE = /^[a-z][a-z0-9_-]{0,39}$/;
const URL_RE = /^https:\/\/[^\s]+$/i;

function CustomUrlBody({ onClose }: { onClose: () => void }) {
  const { t } = useT();
  const create = useCreateMcpServer();
  const [catalogId, setCatalogId] = useState("");
  const [url, setUrl] = useState("https://");
  const [auth, setAuth] = useState<"none" | "apiToken">("none");
  const [token, setToken] = useState("");
  const { errorText, setErrorText, run } = useAsyncSubmit();

  const catalogIdValid = useMemo(() => CATALOG_ID_RE.test(catalogId), [catalogId]);
  const urlValid = useMemo(() => URL_RE.test(url), [url]);

  const onAdd = () =>
    run(async () => {
      if (!catalogIdValid) {
        setErrorText(t("connections.modal.custom.error.alias"));
        return;
      }
      if (!urlValid) {
        setErrorText(t("connections.modal.custom.error.url"));
        return;
      }
      await create.mutateAsync({
        catalog_id: catalogId,
        config: { type: "http", url },
        enabled: true,
        credentials:
          auth === "apiToken" && token.trim()
            ? {
                kind: "static_headers",
                headers: { Authorization: `Bearer ${token}` },
              }
            : undefined,
      });
      onClose();
    });

  return (
    <>
      <ModalHeader
        eyebrow={
          <span className="flex items-center gap-2">
            <span
              aria-hidden
              className="inline-block h-2 w-2 rounded-full bg-[var(--color-moss)]"
            />
            <span>{t("connections.modal.custom.eyebrow")}</span>
          </span>
        }
        title={t("connections.modal.custom.title")}
        icon={<Monogram name="custom" size={36} bg="var(--color-moss)" fg="#fff" glyph="+" />}
        onClose={onClose}
      />
      <div className="flex flex-col gap-4 px-5 py-5">
        <Field label={t("connections.modal.custom.nameLabel")}>
          <input
            value={catalogId}
            onChange={(e) => setCatalogId(e.target.value)}
            placeholder={t("connections.modal.custom.namePlaceholder")}
            className="w-full border border-[var(--color-line)] bg-[var(--color-card)] px-3 py-2 font-[var(--font-mono)] text-[12.5px] text-[var(--color-ink)] outline-none focus:border-[var(--color-moss)]"
          />
        </Field>
        <Field label={t("connections.modal.custom.urlLabel")}>
          <input
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            inputMode="url"
            className="w-full border border-[var(--color-line)] bg-[var(--color-card)] px-3 py-2 font-[var(--font-mono)] text-[12.5px] text-[var(--color-moss-deep)] outline-none focus:border-[var(--color-moss)]"
          />
          <span className="mt-1 block text-[11px] text-[var(--color-muted)]">
            {t("connections.modal.custom.urlHint")}
          </span>
        </Field>
        <Field label={t("connections.modal.custom.authLabel")}>
          <div className="grid grid-cols-2 gap-0 border border-[var(--color-line)]">
            <AuthTab
              active={auth === "none"}
              onClick={() => setAuth("none")}
              label={t("connections.modal.custom.authNone")}
            />
            <AuthTab
              active={auth === "apiToken"}
              onClick={() => setAuth("apiToken")}
              label={t("connections.modal.custom.authToken")}
            />
          </div>
          {auth === "apiToken" ? (
            <input
              value={token}
              onChange={(e) => setToken(e.target.value)}
              type="password"
              placeholder="Bearer …"
              className="mt-2 w-full border border-[var(--color-line)] bg-[var(--color-card)] px-3 py-2 font-[var(--font-mono)] text-[12.5px] text-[var(--color-ink)] outline-none focus:border-[var(--color-moss)]"
            />
          ) : null}
        </Field>
        {auth === "none" ? (
          <Banner
            variant="amber"
            icon={<TriangleAlert className="h-3.5 w-3.5" strokeWidth={1.75} />}
          >
            {t("connections.modal.custom.warn")}
          </Banner>
        ) : null}
        {errorText ? <Banner variant="rose">{errorText}</Banner> : null}
      </div>
      <ModalFooter>
        <Button variant="ghost" onClick={onClose}>
          {t("connections.modal.cancel")}
        </Button>
        <Button
          variant="primary"
          onClick={onAdd}
          loading={create.isPending}
          data-testid="custom-add"
        >
          {t("connections.modal.custom.cta")}
        </Button>
      </ModalFooter>
    </>
  );
}

function Field({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex flex-col gap-1.5">
      <label className="text-[12px] font-semibold text-[var(--color-ink)]">
        {label}
      </label>
      {children}
    </div>
  );
}

function AuthTab({
  active,
  onClick,
  label,
}: {
  active: boolean;
  onClick: () => void;
  label: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      className={
        "flex items-center justify-center gap-2 px-3 py-2 text-[12.5px] transition-colors " +
        (active
          ? "bg-[var(--color-ink)] text-white"
          : "bg-[var(--color-card)] text-[var(--color-muted)] hover:text-[var(--color-ink)]")
      }
    >
      <span
        aria-hidden
        className={
          "inline-block h-2 w-2 rounded-full " +
          (active ? "bg-[var(--color-moss)]" : "bg-[var(--color-line-2)]")
        }
      />
      {label}
    </button>
  );
}

