import { useMemo, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { Plus } from "lucide-react";
import {
  AgentBreadcrumb,
  AgentLayout,
} from "../components/templates/AgentLayout";
import { Button } from "../components/atoms/Button";
import { Spinner } from "../components/atoms/Spinner";
import { EmptyState } from "../components/molecules/EmptyState";
import { PageTabHeader } from "../components/molecules/PageTabHeader";
import { MemoryFilterBar } from "../components/agentDetail/memory/MemoryFilterBar";
import { MemoryQuotaBar } from "../components/agentDetail/memory/MemoryQuotaBar";
import { MemoryGroupHeader } from "../components/agentDetail/memory/MemoryGroupHeader";
import { MemoryRowCard } from "../components/agentDetail/memory/MemoryRowCard";
import { EventJournalPanel } from "../components/agentDetail/memory/EventJournalPanel";
import { AddOperatorNoteModal } from "../components/agentDetail/memory/AddOperatorNoteModal";
import {
  applyFilters,
  EMPTY_FILTERS,
  groupByKind,
  isAnyTentative,
  type MemoryFilters,
} from "../components/agentDetail/memory/memoryFilterState";
import {
  useAgentMemory,
  useAgentMemoryEvents,
  useRevertMemoryEvent,
  useSetMemoryPinned,
} from "../hooks/useAgentMemory";
import { useAgent } from "../hooks/useAgents";
import { useT } from "../i18n";
import { useAuthStore } from "../stores/authStore";
import { ApiError, formatError } from "../lib/errors";
import type { MemoryEventsFilter, MemoryKind } from "../types/api";

export function AgentMemory() {
  const { t } = useT();
  const nav = useNavigate();
  const { id } = useParams<{ id: string }>();
  const agentQuery = useAgent(id ?? null);
  const me = useAuthStore((s) => s.me);
  const activeOrg = me?.orgs.find((o) => o.id === me?.active_org_id);
  const workspaceLabel =
    activeOrg?.name ?? t("connections.breadcrumb.workspace");

  const [filters, setFilters] = useState<MemoryFilters>(EMPTY_FILTERS);
  const [eventFilter, setEventFilter] = useState<MemoryEventsFilter>({});
  const [collapsed, setCollapsed] = useState<Set<MemoryKind>>(() => new Set());
  const [modalOpen, setModalOpen] = useState(false);

  const memoryQuery = useAgentMemory(id ?? null);
  const eventsQuery = useAgentMemoryEvents(id ?? null, eventFilter);
  const setPinned = useSetMemoryPinned();
  const revert = useRevertMemoryEvent();

  const rows = useMemo(() => memoryQuery.data ?? [], [memoryQuery.data]);
  const filtered = useMemo(
    () => applyFilters(rows, filters, Date.now()),
    [rows, filters],
  );
  const grouped = useMemo(() => groupByKind(filtered), [filtered]);
  const agingAvailable = useMemo(() => isAnyTentative(rows), [rows]);

  const agent = agentQuery.data ?? null;

  const toggleGroup = (kind: MemoryKind) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(kind)) next.delete(kind);
      else next.add(kind);
      return next;
    });
  };

  return (
    <AgentLayout agent={agent} active="memory">
      <AgentBreadcrumb
        trail={[
          { label: workspaceLabel },
          { label: t("agent.detail.breadcrumb.agents") },
          { label: agent?.name ?? "…" },
          { label: t("agent.detail.breadcrumb.memory"), current: true },
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
            title={t("agent.detail.memory.title")}
            subtitle={t("agent.detail.memory.subtitle")}
            actions={
              <Button
                variant="primary"
                size="md"
                onClick={() => setModalOpen(true)}
              >
                <span className="inline-flex items-center gap-1.5">
                  <Plus className="h-3.5 w-3.5" strokeWidth={1.75} />
                  {t("agent.detail.memory.addNote")}
                </span>
              </Button>
            }
          />
          <div className="flex min-h-0 flex-1 flex-col lg:flex-row">
            <section
              className="flex h-full min-h-0 flex-col border-b border-[var(--color-line)] lg:w-[620px] lg:shrink-0 lg:border-r lg:border-b-0"
              aria-label={t("agent.detail.memory.title")}
            >
              <MemoryFilterBar
                filters={filters}
                onChange={setFilters}
                agingAvailable={agingAvailable}
              />
              <MemoryQuotaBar used={rows.length} />
              <div className="min-h-0 flex-1 overflow-y-auto">
                {memoryQuery.isLoading && rows.length === 0 ? (
                  <div className="flex items-center justify-center p-6 text-[var(--color-muted)]">
                    <Spinner size={14} />
                  </div>
                ) : memoryQuery.isError ? (
                  <div className="p-6">
                    <EmptyState
                      title={t("agent.detail.memory.loadError.title")}
                      description={
                        memoryQuery.error
                          ? formatError(memoryQuery.error)
                          : t("agent.detail.memory.loadError.body")
                      }
                      action={
                        <Button
                          variant="primary"
                          onClick={() => memoryQuery.refetch()}
                        >
                          {t("agent.detail.memory.loadError.cta")}
                        </Button>
                      }
                    />
                  </div>
                ) : rows.length === 0 ? (
                  <div className="p-6">
                    <EmptyState
                      title={t("agent.detail.memory.empty.title")}
                      description={t("agent.detail.memory.empty.body")}
                    />
                  </div>
                ) : grouped.length === 0 ? (
                  <div className="p-6">
                    <EmptyState
                      title={t("agent.detail.memory.empty.filtered.title")}
                      description={t("agent.detail.memory.empty.filtered.body")}
                    />
                  </div>
                ) : (
                  grouped.map((g) => {
                    const isOpen = !collapsed.has(g.kind);
                    return (
                      <div key={g.kind}>
                        <MemoryGroupHeader
                          kind={g.kind}
                          count={g.rows.length}
                          open={isOpen}
                          onToggle={() => toggleGroup(g.kind)}
                        />
                        {isOpen
                          ? g.rows.map((r) => (
                              <MemoryRowCard
                                key={r.id}
                                row={r}
                                onTogglePin={() =>
                                  setPinned.mutate({
                                    agentId: agent.id,
                                    memoryId: r.id,
                                    pinned: !r.pinned,
                                  })
                                }
                                pinPending={
                                  setPinned.isPending &&
                                  setPinned.variables?.memoryId === r.id
                                }
                              />
                            ))
                          : null}
                      </div>
                    );
                  })
                )}
              </div>
            </section>
            <EventJournalPanel
              events={eventsQuery.data ?? []}
              loading={eventsQuery.isLoading}
              filter={eventFilter}
              onFilterChange={setEventFilter}
              onRevert={(eventId) =>
                revert.mutate({ agentId: agent.id, eventId })
              }
              revertingId={
                revert.isPending ? (revert.variables?.eventId ?? null) : null
              }
            />
          </div>
          <AddOperatorNoteModal
            agentId={agent.id}
            open={modalOpen}
            onClose={() => setModalOpen(false)}
          />
        </>
      )}
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
