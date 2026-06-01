import { useEffect, useMemo, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { Save } from "lucide-react";
import {
  AgentBreadcrumb,
  AgentLayout,
} from "../components/templates/AgentLayout";
import { Button } from "../components/atoms/Button";
import { Spinner } from "../components/atoms/Spinner";
import { EmptyState } from "../components/molecules/EmptyState";
import { PageTabHeader } from "../components/molecules/PageTabHeader";
import { AllowlistCard } from "../components/agentDetail/AllowlistCard";
import { CapabilitySummary } from "../components/agentDetail/CapabilitySummary";
import { AgentActivityCard } from "../components/agentDetail/AgentActivityCard";
import { useAgent, useUpdateAgent } from "../hooks/useAgents";
import { useMcpServers } from "../hooks/useMcpServers";
import { useT } from "../i18n";
import { useAuthStore } from "../stores/authStore";
import { ApiError, formatError } from "../lib/errors";
import {
  allowlistsEqual,
  type Allowlist,
} from "../components/agentDetail/allowlistState";

export function AgentTools() {
  const { t } = useT();
  const nav = useNavigate();
  const { id } = useParams<{ id: string }>();
  const agentQuery = useAgent(id ?? null);
  const serversQuery = useMcpServers();
  const updateAgent = useUpdateAgent();
  const me = useAuthStore((s) => s.me);
  const activeOrg = me?.orgs.find((o) => o.id === me?.active_org_id);
  const workspaceLabel =
    activeOrg?.name ?? t("connections.breadcrumb.workspace");

  const agent = agentQuery.data ?? null;
  const servers = serversQuery.data ?? [];
  const enabledServers = useMemo(
    () => servers.filter((s) => s.enabled),
    [servers],
  );
  const serverAllowlist: Allowlist = agent?.allowed_mcp_tools ?? {};

  // Hydrate the editor exactly once per agent. Depending on
  // `serverAllowlist` (the live query result) would silently overwrite
  // operator edits on every 15s refetch, since react-query returns a
  // fresh object reference each fetch.
  const [local, setLocal] = useState<Allowlist>(serverAllowlist);
  useEffect(() => {
    if (agent) setLocal(agent.allowed_mcp_tools ?? {});
  }, [agent?.id]);

  const dirty = !allowlistsEqual(local, serverAllowlist);
  const saving = updateAgent.isPending;

  return (
    <AgentLayout agent={agent} active="tools">
      <AgentBreadcrumb
        trail={[
          { label: workspaceLabel },
          { label: t("agent.detail.breadcrumb.agents") },
          { label: agent?.name ?? "…" },
          { label: t("agent.detail.breadcrumb.tools"), current: true },
        ]}
      />
      {agentQuery.isLoading && !agentQuery.isError ? (
        <div className="flex flex-1 items-center justify-center text-[var(--color-muted-foreground)]">
          <Spinner size={16} />
        </div>
      ) : !agent ? (
        <div className="flex flex-1 items-center justify-center p-8">
          <AgentLoadFallback
            error={agentQuery.error}
            onRetry={() => agentQuery.refetch()}
            onHome={() => nav("/")}
          />
        </div>
      ) : (
        <>
          <PageTabHeader
            title={t("agent.detail.tools.title")}
            subtitle={t("agent.detail.tools.subtitle", { name: agent.name })}
            actions={
              <Button
                variant="primary"
                size="md"
                disabled={!dirty}
                loading={saving}
                onClick={() =>
                  updateAgent.mutate({
                    id: agent.id,
                    patch: { allowed_mcp_tools: local },
                  })
                }
              >
                <span className="inline-flex items-center gap-1.5">
                  <Save className="h-3.5 w-3.5" strokeWidth={1.75} />
                  {t("agent.detail.tools.save")}
                </span>
              </Button>
            }
          />
          <div className="min-h-0 flex-1 overflow-auto p-4 md:p-8">
            <div className="flex flex-col gap-6 lg:flex-row">
              <div className="flex min-w-0 flex-1 flex-col gap-5">
                <AllowlistCard
                  servers={enabledServers}
                  list={local}
                  onChange={setLocal}
                  loading={serversQuery.isLoading}
                />
                <AgentActivityCard
                  agentId={agent.id}
                  agentName={agent.name}
                  servers={servers}
                />
              </div>
              <div className="flex w-full flex-col gap-5 lg:w-[340px] lg:shrink-0">
                <CapabilitySummary servers={enabledServers} list={local} />
              </div>
            </div>
          </div>
        </>
      )}
    </AgentLayout>
  );
}

// 404/403 = "not found / hidden" (the same operator surface — we don't
// distinguish "doesn't exist" from "you can't see it"). Any other error
// is transient/system and gets a retry affordance instead.
function AgentLoadFallback({
  error,
  onRetry,
  onHome,
}: {
  error: unknown;
  onRetry: () => void;
  onHome: () => void;
}) {
  const { t } = useT();
  const isNotFound =
    error instanceof ApiError && (error.status === 404 || error.status === 403);
  if (isNotFound) {
    return (
      <EmptyState
        title={t("agent.detail.notFound.title")}
        description={t("agent.detail.notFound.body")}
        action={
          <Button variant="primary" onClick={onHome}>
            {t("agent.detail.notFound.cta")}
          </Button>
        }
      />
    );
  }
  return (
    <EmptyState
      title={t("agent.detail.loadError.title")}
      description={
        error ? formatError(error) : t("agent.detail.loadError.body")
      }
      action={
        <Button variant="primary" onClick={onRetry}>
          {t("agent.detail.loadError.cta")}
        </Button>
      }
    />
  );
}
