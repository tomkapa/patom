import { useCallback } from "react";
import { useNavigate, useParams } from "react-router-dom";
import {
  ConnectionsBreadcrumb,
  ConnectionsLayout,
} from "../components/templates/ConnectionsLayout";
import { Button } from "../components/atoms/Button";
import { Spinner } from "../components/atoms/Spinner";
import { EmptyState } from "../components/molecules/EmptyState";
import { DetailHeader } from "../components/connectionDetail/DetailHeader";
import { StatusCard } from "../components/connectionDetail/StatusCard";
import { ToolsExposedCard } from "../components/connectionDetail/ToolsExposedCard";
import { RecentActivityCard } from "../components/connectionDetail/RecentActivityCard";
import { UsedByCard } from "../components/connectionDetail/UsedByCard";
import { DangerZone } from "../components/connectionDetail/DangerZone";
import {
  useCatalogLookup,
  useDeleteMcpCredentials,
  useDisconnectOAuth,
  useMcpServer,
  useUpdateMcpServer,
} from "../hooks/useMcpServers";
import { useT } from "../i18n";
import { CREDENTIALS_KIND } from "../types/api";

export function ConnectionDetail() {
  const { t } = useT();
  const nav = useNavigate();
  const { id } = useParams<{ id: string }>();
  const serverQuery = useMcpServer(id ?? null, { refetchInterval: 15_000 });
  const catalogLookup = useCatalogLookup();
  const updateServer = useUpdateMcpServer();
  const disconnectOAuth = useDisconnectOAuth();
  const deleteCredentials = useDeleteMcpCredentials();

  const server = serverQuery.data ?? null;
  const onDisableToggle = useCallback(() => {
    if (!server) return;
    updateServer.mutate({
      id: server.id,
      patch: { enabled: !server.enabled },
    });
  }, [server, updateServer]);

  const onDisconnect = useCallback(() => {
    if (!server) return;
    if (server.credentials_kind === CREDENTIALS_KIND.OAUTH2) {
      disconnectOAuth.mutate(server.id);
    } else if (server.credentials_kind === CREDENTIALS_KIND.STATIC_HEADERS) {
      deleteCredentials.mutate(server.id);
    }
  }, [server, disconnectOAuth, deleteCredentials]);

  const breadcrumbLabel = server
    ? (catalogLookup(server.catalog_id)?.display_name ?? server.catalog_id)
    : t("connections.breadcrumb.connections");

  return (
    <ConnectionsLayout active="list">
      <ConnectionsBreadcrumb
        trail={[
          { label: t("connections.breadcrumb.workspace") },
          { label: t("connections.breadcrumb.connections") },
          { label: breadcrumbLabel, current: true },
        ]}
      />
      {serverQuery.isLoading && !serverQuery.isError ? (
        <div className="flex flex-1 items-center justify-center text-[var(--color-muted)]">
          <Spinner size={16} />
        </div>
      ) : !server ? (
        <div className="flex flex-1 items-center justify-center p-8">
          <EmptyState
            title={t("connections.detail.notFound.title")}
            description={t("connections.detail.notFound.body")}
            action={
              <Button variant="primary" onClick={() => nav("/connections")}>
                {t("connections.detail.notFound.cta")}
              </Button>
            }
          />
        </div>
      ) : (
        <>
          <DetailHeader
            server={server}
            onDisableToggle={onDisableToggle}
            onDisconnect={onDisconnect}
            disableBusy={updateServer.isPending}
            disconnectBusy={
              disconnectOAuth.isPending || deleteCredentials.isPending
            }
          />
          <div className="min-h-0 flex-1 overflow-auto p-8">
            <div className="flex flex-col gap-6 lg:flex-row">
              <div className="flex min-w-0 flex-1 flex-col gap-5">
                <StatusCard server={server} />
                <ToolsExposedCard tools={server.discovered_tools ?? []} />
                <RecentActivityCard serverId={server.id} />
              </div>
              <div className="flex w-full flex-col gap-5 lg:w-[380px] lg:shrink-0">
                <UsedByCard serverId={server.id} />
                <DangerZone
                  server={server}
                  onDisableToggle={onDisableToggle}
                  onDisconnect={onDisconnect}
                  disableBusy={updateServer.isPending}
                  disconnectBusy={
                    disconnectOAuth.isPending || deleteCredentials.isPending
                  }
                />
              </div>
            </div>
          </div>
        </>
      )}
    </ConnectionsLayout>
  );
}
