import { useMemo, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import {
  AgentBreadcrumb,
  AgentLayout,
} from "../components/templates/AgentLayout";
import { Button } from "../components/atoms/Button";
import { Spinner } from "../components/atoms/Spinner";
import { EmptyState } from "../components/molecules/EmptyState";
import { PageTabHeader } from "../components/molecules/PageTabHeader";
import { ScopeStrip } from "../components/agentDetail/logs/ScopeStrip";
import { TokenSpendChart } from "../components/agentDetail/logs/TokenSpendChart";
import { TurnsTimeline } from "../components/agentDetail/logs/TurnsTimeline";
import { PromptDiffModal } from "../components/agentDetail/logs/PromptDiffModal";
import {
  useAgentMetricsTimeseries,
  useAgentTurns,
} from "../hooks/useAgentLogs";
import { useAgent } from "../hooks/useAgents";
import { useT } from "../i18n";
import { useAuthStore } from "../stores/authStore";
import { ApiError, formatError } from "../lib/errors";
import type {
  LogsCompareMode,
  LogsKindFilter,
  LogsTimeRange,
} from "../types/api";

const RANGE_LABEL_KEYS = {
  "1h": "agent.detail.logs.scope.windowLabel.1h",
  "24h": "agent.detail.logs.scope.windowLabel.24h",
  "7d": "agent.detail.logs.scope.windowLabel.7d",
  "30d": "agent.detail.logs.scope.windowLabel.30d",
} as const;

export function AgentLogs() {
  const { t } = useT();
  const nav = useNavigate();
  const { id } = useParams<{ id: string }>();
  const agentQuery = useAgent(id ?? null);
  const me = useAuthStore((s) => s.me);
  const activeOrg = me?.orgs.find((o) => o.id === me?.active_org_id);
  const workspaceLabel =
    activeOrg?.name ?? t("connections.breadcrumb.workspace");

  const [range, setRange] = useState<LogsTimeRange>("24h");
  const [kind, setKind] = useState<LogsKindFilter>("all");
  const [compare, setCompare] = useState<LogsCompareMode>("prev_window");
  const [diffTarget, setDiffTarget] = useState<number | null>(null);

  const metrics = useAgentMetricsTimeseries(id ?? null, range, compare);
  const turns = useAgentTurns(id ?? null, range, kind);

  const allTurns = useMemo(
    () => turns.data?.pages.flatMap((p) => p.items) ?? [],
    [turns.data],
  );
  const updatedAt = metrics.dataUpdatedAt
    ? new Date(metrics.dataUpdatedAt)
    : null;

  const agent = agentQuery.data ?? null;

  return (
    <AgentLayout agent={agent} active="logs">
      <AgentBreadcrumb
        trail={[
          { label: workspaceLabel },
          { label: t("agent.detail.breadcrumb.agents") },
          { label: agent?.name ?? "…" },
          { label: t("agent.detail.breadcrumb.logs"), current: true },
        ]}
      />
      {agentQuery.isLoading && !agentQuery.isError ? (
        <div className="flex flex-1 items-center justify-center text-[var(--color-muted)]">
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
            title={t("agent.detail.logs.title")}
            subtitle={t("agent.detail.logs.subtitle")}
          />
          <ScopeStrip
            range={range}
            onRangeChange={setRange}
            kind={kind}
            onKindChange={setKind}
            compare={compare}
            onCompareChange={setCompare}
            updatedAt={updatedAt}
            bucketLabel={metrics.data?.bucket_label}
          />
          <div className="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto px-8 py-4">
            <TokenSpendChart
              buckets={metrics.data?.buckets ?? []}
              totals={
                metrics.data?.totals ?? {
                  tokens: 0,
                  turns: 0,
                  latency_p50_ms: 0,
                  latency_p95_ms: 0,
                  failure_count: 0,
                }
              }
              deltas={
                metrics.data?.deltas_vs_compare ?? {
                  tokens: null,
                  latency_p95_ms: null,
                  failure_count: null,
                }
              }
              promptEdits={metrics.data?.prompt_edits ?? []}
              loading={metrics.isLoading}
              onMarkerClick={(v) => setDiffTarget(v)}
            />
            <TurnsTimeline
              pages={allTurns}
              isLoading={turns.isLoading}
              hasNextPage={Boolean(turns.hasNextPage)}
              isFetchingNextPage={turns.isFetchingNextPage}
              onLoadMore={() => turns.fetchNextPage()}
              onSeparatorClick={(v) => setDiffTarget(v)}
              promptEdits={metrics.data?.prompt_edits ?? []}
            />
          </div>
        </>
      )}
      {id ? (
        <PromptDiffModal
          agentId={id}
          targetVersion={diffTarget}
          open={diffTarget != null}
          onClose={() => setDiffTarget(null)}
          metricsTotals={metrics.data?.totals}
          metricsDeltas={metrics.data?.deltas_vs_compare}
          windowLabel={t(RANGE_LABEL_KEYS[range])}
        />
      ) : null}
    </AgentLayout>
  );
}

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
