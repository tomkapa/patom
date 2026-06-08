import { useRef, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { AnimatePresence, motion } from "motion/react";
import {
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  Clock,
  Info,
  Repeat,
  SlidersHorizontal,
  TriangleAlert,
  X,
} from "lucide-react";
import { AgentLayout } from "../components/templates/AgentLayout";
import { Button } from "../components/atoms/Button";
import { Spinner } from "../components/atoms/Spinner";
import { EmptyState } from "../components/molecules/EmptyState";
import {
  useCancelScheduledTask,
  useScheduledTasks,
} from "../hooks/useScheduledTasks";
import { useAgent } from "../hooks/useAgents";
import { useOverlayA11y } from "../hooks/useOverlayA11y";
import { modalMotion, scrimMotion } from "../lib/motion";
import { useT } from "../i18n";
import { ApiError, formatError } from "../lib/errors";
import type {
  ScheduledTask,
  ScheduledTaskStatus,
  ScheduledTaskSummary,
} from "../types/api";

const PER_PAGE = 5;
const MONO = "var(--font-mono)"; // IBM Plex Mono — numeric / schedule text

/** Geist Mono is the design's label face (status pills, column heads). */
const LABEL_MONO = "'Geist Mono'";

const STATUS_TONE: Record<ScheduledTaskStatus, string> = {
  active: "#2D6B3F",
  completed: "#808A80",
  cancelled: "#DC2626",
};

export function ScheduledTasks() {
  const { t } = useT();
  const nav = useNavigate();
  const { id } = useParams<{ id: string }>();
  const agentQuery = useAgent(id ?? null);
  const agent = agentQuery.data ?? null;

  const [page, setPage] = useState(1);
  const [cancelTarget, setCancelTarget] = useState<ScheduledTask | null>(null);
  const tasksQuery = useScheduledTasks(id ?? null, page, PER_PAGE);
  const cancel = useCancelScheduledTask(id ?? null);

  const data = tasksQuery.data;
  const items = data?.items ?? [];
  const total = data?.total ?? 0;
  const summary: ScheduledTaskSummary =
    data?.summary ?? { active: 0, completed: 0, cancelled: 0 };
  const pageCount = Math.max(1, Math.ceil(total / PER_PAGE));
  const from = total === 0 ? 0 : (page - 1) * PER_PAGE + 1;
  const to = Math.min(page * PER_PAGE, total);

  return (
    <AgentLayout agent={agent} active="scheduled">
      <div className="flex min-h-0 flex-1 flex-col bg-[var(--color-surface-primary)]">
        <Breadcrumb agentName={agent?.name ?? "…"} />
        <PageHeader />
        <StatsStrip summary={summary} />
        {agentQuery.isLoading && !agentQuery.isError ? (
          <div className="flex flex-1 items-center justify-center text-[var(--color-fg-muted)]">
            <Spinner size={16} />
          </div>
        ) : !agent ? (
          <AgentLoadFallback
            error={agentQuery.error}
            onRetry={() => agentQuery.refetch()}
            onHome={() => nav("/")}
          />
        ) : tasksQuery.isError ? (
          <div className="flex flex-1 items-center justify-center p-8">
            <EmptyState
              title={t("agent.detail.scheduled.loadError.title")}
              description={formatError(tasksQuery.error)}
              action={
                <Button variant="primary" onClick={() => tasksQuery.refetch()}>
                  {t("agent.detail.scheduled.loadError.cta")}
                </Button>
              }
            />
          </div>
        ) : (
          <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto px-8 pt-5 pb-6">
            <InfoBanner />
            <TaskTable
              items={items}
              loading={tasksQuery.isLoading}
              onCancel={(task) => setCancelTarget(task)}
              footer={
                <TableFooter
                  from={from}
                  to={to}
                  total={total}
                  page={page}
                  pageCount={pageCount}
                  onPage={setPage}
                />
              }
            />
          </div>
        )}
      </div>

      <CancelDialog
        task={cancelTarget}
        agentName={agent?.name ?? ""}
        pending={cancel.isPending}
        onClose={() => (cancel.isPending ? undefined : setCancelTarget(null))}
        onConfirm={() => {
          if (!cancelTarget) return;
          cancel.mutate(cancelTarget.id, {
            onSuccess: () => setCancelTarget(null),
          });
        }}
      />
    </AgentLayout>
  );
}

function Breadcrumb({ agentName }: { agentName: string }) {
  const { t } = useT();
  const sep = (
    <ChevronRight className="h-3 w-3 text-[var(--color-fg-muted)]" aria-hidden />
  );
  return (
    <div className="flex items-center gap-2 px-8 pt-4 pb-3 text-[12px] text-[var(--color-fg-muted)]">
      <span>{t("agent.detail.breadcrumb.agents")}</span>
      {sep}
      <span>{agentName}</span>
      {sep}
      <span className="font-medium text-[var(--color-fg-primary)]">
        {t("agent.detail.breadcrumb.scheduled")}
      </span>
    </div>
  );
}

function PageHeader() {
  const { t } = useT();
  return (
    <div className="flex items-center justify-between border-b border-[var(--color-border-subtle)] px-8 pt-2 pb-6">
      <div className="flex flex-col gap-1.5">
        <h1 className="font-[var(--font-display)] text-[24px] leading-tight font-bold text-[var(--color-fg-primary)]">
          {t("agent.detail.scheduled.title")}
        </h1>
        <p className="text-[13px] text-[var(--color-fg-secondary)]">
          {t("agent.detail.scheduled.subtitle")}
        </p>
      </div>
      <button
        type="button"
        className="inline-flex cursor-pointer items-center gap-1.5 border border-[var(--color-border-subtle)] bg-[var(--color-surface-primary)] px-3.5 py-[7px] text-[13px] text-[var(--color-fg-secondary)] transition-colors duration-150 ease-out hover:bg-[var(--color-surface-secondary)]"
      >
        <SlidersHorizontal className="h-3.5 w-3.5 text-[var(--color-fg-muted)]" />
        {t("agent.detail.scheduled.filter")}
      </button>
    </div>
  );
}

function StatsStrip({ summary }: { summary: ScheduledTaskSummary }) {
  const { t } = useT();
  const cells: { label: string; value: number; tone: string }[] = [
    {
      label: t("agent.detail.scheduled.stat.active"),
      value: summary.active,
      tone: "#2D6B3F",
    },
    {
      label: t("agent.detail.scheduled.stat.completed"),
      value: summary.completed,
      tone: "#808A80",
    },
    {
      label: t("agent.detail.scheduled.stat.cancelled"),
      value: summary.cancelled,
      tone: "#6B7B6B",
    },
  ];
  return (
    <div className="flex items-stretch px-8">
      {cells.map((c, i) => (
        <div
          key={c.label}
          className={
            "flex flex-1 flex-col gap-1 px-5 py-4" +
            (i < cells.length - 1
              ? " border-r border-[var(--color-border-subtle)]"
              : "")
          }
        >
          <span
            className="text-[9px] font-semibold tracking-[1.5px] text-[var(--color-fg-muted)]"
            style={{ fontFamily: LABEL_MONO }}
          >
            {c.label}
          </span>
          <span
            className="text-[28px] font-bold leading-none"
            style={{ fontFamily: MONO, color: c.tone }}
          >
            {c.value}
          </span>
        </div>
      ))}
    </div>
  );
}

function InfoBanner() {
  const { t } = useT();
  return (
    <div className="flex items-center gap-2.5 bg-[var(--color-accent-soft)] px-4 py-2.5">
      <Info className="h-3.5 w-3.5 shrink-0 text-[var(--color-accent-primary)]" />
      <span className="text-[12px] text-[var(--color-fg-secondary)]">
        {t("agent.detail.scheduled.info")}
      </span>
    </div>
  );
}

function TaskTable({
  items,
  loading,
  onCancel,
  footer,
}: {
  items: ScheduledTask[];
  loading: boolean;
  onCancel: (task: ScheduledTask) => void;
  footer: React.ReactNode;
}) {
  const { t } = useT();
  const isEmpty = !loading && items.length === 0;
  return (
    <div className="flex flex-col border border-[var(--color-border-subtle)]">
      {/* head */}
      <div className="flex items-center gap-3 border-b border-[var(--color-border-subtle)] bg-[var(--color-surface-secondary)] px-4 py-2.5">
        <ColHead className="w-20">{t("agent.detail.scheduled.col.status")}</ColHead>
        <ColHead className="flex-1">{t("agent.detail.scheduled.col.name")}</ColHead>
        <ColHead className="w-[220px]">
          {t("agent.detail.scheduled.col.schedule")}
        </ColHead>
        <ColHead className="w-[140px]">
          {t("agent.detail.scheduled.col.nextRun")}
        </ColHead>
        <ColHead className="w-[140px]">
          {t("agent.detail.scheduled.col.lastRun")}
        </ColHead>
        <span className="w-16" aria-hidden />
      </div>

      {isEmpty ? (
        <div className="flex items-center justify-center px-4 py-16">
          <EmptyState
            title={t("agent.detail.scheduled.empty.title")}
            description={t("agent.detail.scheduled.empty.body")}
          />
        </div>
      ) : (
        items.map((task, i) => (
          <TaskRow
            key={task.id}
            task={task}
            last={i === items.length - 1}
            onCancel={() => onCancel(task)}
          />
        ))
      )}

      {footer}
    </div>
  );
}

function ColHead({
  children,
  className,
}: {
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <span
      className={
        "text-[9px] font-semibold tracking-[1.5px] text-[var(--color-fg-muted)] " +
        (className ?? "")
      }
      style={{ fontFamily: LABEL_MONO }}
    >
      {children}
    </span>
  );
}

function TaskRow({
  task,
  last,
  onCancel,
}: {
  task: ScheduledTask;
  last: boolean;
  onCancel: () => void;
}) {
  const { t } = useT();
  const tone = STATUS_TONE[task.status];
  const isCancelled = task.status === "cancelled";
  const isActive = task.status === "active";
  const statusLabel = t(`agent.detail.scheduled.status.${task.status}`);
  const KindIcon = task.kind === "recurring" ? Repeat : Clock;
  const kindLabel =
    task.kind === "recurring"
      ? t("agent.detail.scheduled.kind.recurring")
      : t("agent.detail.scheduled.kind.oneTime");

  return (
    <div
      className={
        "flex items-center gap-3 px-4 py-3.5 transition-colors duration-150 ease-out hover:bg-[#2D6B3F08]" +
        (last ? "" : " border-b border-[var(--color-border-subtle)]") +
        (isCancelled ? " opacity-50" : "")
      }
    >
      {/* status */}
      <div className="flex w-20 items-center gap-1.5">
        <span
          className="h-1.5 w-1.5 rounded-full"
          style={{ backgroundColor: tone }}
          aria-hidden
        />
        <span
          className="text-[11px] font-semibold tracking-[0.5px]"
          style={{ fontFamily: LABEL_MONO, color: tone }}
        >
          {statusLabel}
        </span>
      </div>

      {/* name + kind */}
      <div className="flex min-w-0 flex-1 flex-col gap-0.5">
        <div className="flex items-center gap-2">
          <span
            className="truncate text-[13px] font-semibold"
            style={{
              color: isCancelled ? "#808A80" : "#1E3322",
            }}
          >
            {task.name}
          </span>
          <span className="flex shrink-0 items-center gap-1 bg-[var(--color-surface-secondary)] px-1.5 py-0.5">
            <KindIcon className="h-2.5 w-2.5 text-[var(--color-fg-muted)]" />
            <span
              className="text-[9px] font-medium tracking-[0.5px] text-[var(--color-fg-muted)]"
              style={{ fontFamily: LABEL_MONO }}
            >
              {kindLabel}
            </span>
          </span>
        </div>
      </div>

      {/* schedule */}
      <span
        className="w-[220px] truncate text-[11px]"
        style={{
          fontFamily: MONO,
          color: isCancelled ? "#808A80" : "#6B7B6B",
        }}
      >
        {task.schedule_label}
      </span>

      {/* next run */}
      <span
        className="w-[140px] truncate text-[11px] font-medium"
        style={{
          fontFamily: MONO,
          color: task.next_run_label ? "#1E3322" : "#808A80",
        }}
      >
        {task.next_run_label ?? "—"}
      </span>

      {/* last run */}
      <span
        className="w-[140px] truncate text-[11px]"
        style={{
          fontFamily: MONO,
          color: isCancelled ? "#808A80" : "#6B7B6B",
        }}
      >
        {task.last_run_label ?? "—"}
      </span>

      {/* actions */}
      <div className="flex h-7 w-16 items-center justify-end gap-1">
        {isActive ? (
          <button
            type="button"
            aria-label={t("agent.detail.scheduled.cancelAria", {
              name: task.name,
            })}
            onClick={onCancel}
            className="flex h-7 w-7 cursor-pointer items-center justify-center border border-[var(--color-border-subtle)] text-[#DC2626] transition-colors duration-150 ease-out hover:bg-[#DC262610]"
          >
            <X className="h-3.5 w-3.5" />
          </button>
        ) : null}
      </div>
    </div>
  );
}

function TableFooter({
  from,
  to,
  total,
  page,
  pageCount,
  onPage,
}: {
  from: number;
  to: number;
  total: number;
  page: number;
  pageCount: number;
  onPage: (p: number) => void;
}) {
  const { t } = useT();
  const pages = Array.from({ length: pageCount }, (_, i) => i + 1);
  return (
    <div className="flex items-center justify-between gap-4 border-t border-[var(--color-border-subtle)] bg-[var(--color-surface-secondary)] px-4 py-2.5">
      <div className="flex items-center gap-3">
        <span className="text-[12px] text-[var(--color-fg-muted)]">
          {t("agent.detail.scheduled.footer.count", { from, to, total })}
        </span>
        <div className="flex items-center gap-2">
          <span className="text-[12px] text-[var(--color-fg-muted)]">
            {t("agent.detail.scheduled.footer.perPage")}
          </span>
          <div className="flex items-center gap-1.5 border border-[var(--color-border-subtle)] bg-[var(--color-surface-primary)] px-2 py-1">
            <span
              className="text-[12px] font-medium text-[var(--color-fg-primary)]"
              style={{ fontFamily: MONO }}
            >
              {PER_PAGE}
            </span>
            <ChevronDown className="h-3 w-3 text-[var(--color-fg-muted)]" />
          </div>
        </div>
      </div>

      <div className="flex items-center gap-1">
        <PagerButton
          disabled={page <= 1}
          onClick={() => onPage(page - 1)}
        >
          <ChevronLeft className="h-3.5 w-3.5" />
          {t("agent.detail.scheduled.footer.prev")}
        </PagerButton>
        {pages.map((p) => {
          const isActive = p === page;
          return (
            <button
              key={p}
              type="button"
              onClick={() => onPage(p)}
              aria-current={isActive ? "page" : undefined}
              className={
                "flex h-8 w-8 cursor-pointer items-center justify-center text-[12px] transition-colors duration-150 ease-out " +
                (isActive
                  ? "bg-[#1E3322] font-semibold text-white"
                  : "border border-[var(--color-border-subtle)] text-[var(--color-fg-secondary)] hover:bg-[var(--color-surface-primary)]")
              }
              style={{ fontFamily: MONO }}
            >
              {p}
            </button>
          );
        })}
        <PagerButton
          disabled={page >= pageCount}
          emphasis
          onClick={() => onPage(page + 1)}
        >
          {t("agent.detail.scheduled.footer.next")}
          <ChevronRight className="h-3.5 w-3.5" />
        </PagerButton>
      </div>
    </div>
  );
}

function PagerButton({
  children,
  onClick,
  disabled,
  emphasis,
}: {
  children: React.ReactNode;
  onClick: () => void;
  disabled?: boolean;
  emphasis?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      className={
        "flex items-center gap-1.5 border border-[var(--color-border-subtle)] px-2.5 py-[5px] text-[12px] transition-colors duration-150 ease-out disabled:cursor-not-allowed disabled:opacity-40 " +
        (emphasis
          ? "text-[var(--color-fg-primary)] hover:bg-[var(--color-surface-primary)]"
          : "text-[var(--color-fg-muted)] hover:bg-[var(--color-surface-primary)]") +
        (disabled ? "" : " cursor-pointer")
      }
    >
      {children}
    </button>
  );
}

function CancelDialog({
  task,
  agentName,
  pending,
  onClose,
  onConfirm,
}: {
  task: ScheduledTask | null;
  agentName: string;
  pending: boolean;
  onClose: () => void;
  onConfirm: () => void;
}) {
  const { t } = useT();
  const ref = useRef<HTMLDivElement | null>(null);
  const open = task !== null;
  useOverlayA11y(ref, open, onClose);

  return (
    <AnimatePresence>
      {open && task ? (
        <motion.div
          {...scrimMotion}
          className="fixed inset-0 z-50 flex items-center justify-center p-8"
          style={{ backgroundColor: "#1E332266" }}
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) onClose();
          }}
        >
          <motion.div
            ref={ref}
            role="dialog"
            aria-modal="true"
            aria-label={t("agent.detail.scheduled.dialog.title")}
            tabIndex={-1}
            {...modalMotion}
            className="w-full max-w-[480px] border border-[var(--color-border-subtle)] bg-[var(--color-surface-primary)] shadow-xl focus:outline-none"
          >
            {/* header */}
            <div className="flex items-center justify-between px-7 pt-7">
              <div className="flex h-10 w-10 items-center justify-center bg-[#FEF3C7]">
                <TriangleAlert className="h-5 w-5 text-[#D97706]" />
              </div>
              <button
                type="button"
                aria-label={t("connections.modal.close")}
                onClick={onClose}
                className="flex h-7 w-7 cursor-pointer items-center justify-center text-[var(--color-fg-muted)] transition-colors duration-150 ease-out hover:text-[var(--color-fg-primary)]"
              >
                <X className="h-4 w-4" />
              </button>
            </div>

            {/* body */}
            <div className="flex flex-col gap-5 px-7 pt-5">
              <h2 className="font-[var(--font-display)] text-[20px] font-bold text-[var(--color-fg-primary)]">
                {t("agent.detail.scheduled.dialog.title")}
              </h2>
              <div className="flex flex-col border border-[var(--color-border-subtle)] bg-[var(--color-surface-secondary)]">
                <DetailRow
                  label={t("agent.detail.scheduled.dialog.task")}
                  value={task.name}
                />
                <DetailRow
                  label={t("agent.detail.scheduled.dialog.schedule")}
                  value={task.schedule_full}
                />
                <DetailRow
                  label={t("agent.detail.scheduled.dialog.agent")}
                  value={agentName || task.agent_name}
                  last
                />
              </div>
              <p className="text-[13px] leading-relaxed text-[var(--color-fg-secondary)]">
                {t("agent.detail.scheduled.dialog.warning")}
              </p>
            </div>

            <div className="mt-5 h-px w-full bg-[var(--color-border-subtle)]" />

            {/* actions */}
            <div className="flex items-center justify-end gap-3 px-7 py-4">
              <button
                type="button"
                onClick={onClose}
                disabled={pending}
                className="cursor-pointer border border-[var(--color-border-subtle)] bg-[var(--color-surface-primary)] px-5 py-2 text-[13px] font-medium text-[var(--color-fg-primary)] transition-colors duration-150 ease-out hover:bg-[var(--color-surface-secondary)] disabled:opacity-50"
              >
                {t("agent.detail.scheduled.dialog.keep")}
              </button>
              <button
                type="button"
                onClick={onConfirm}
                disabled={pending}
                className="flex cursor-pointer items-center justify-center gap-2 bg-[#DC2626] px-5 py-2 text-[13px] font-semibold text-white transition-colors duration-150 ease-out hover:bg-[#B91C1C] disabled:opacity-60"
              >
                {pending ? <Spinner size={12} /> : null}
                {t("agent.detail.scheduled.dialog.confirm")}
              </button>
            </div>
          </motion.div>
        </motion.div>
      ) : null}
    </AnimatePresence>
  );
}

function DetailRow({
  label,
  value,
  last,
}: {
  label: string;
  value: string;
  last?: boolean;
}) {
  return (
    <div
      className={
        "flex items-center gap-3 px-4 py-2.5" +
        (last ? "" : " border-b border-[var(--color-border-subtle)]")
      }
    >
      <span className="w-[72px] shrink-0 text-[12px] text-[var(--color-fg-muted)]">
        {label}
      </span>
      <span
        className="min-w-0 truncate text-[12px] font-medium text-[var(--color-fg-primary)]"
        style={{ fontFamily: MONO }}
      >
        {value}
      </span>
    </div>
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
  return (
    <div className="flex flex-1 items-center justify-center p-8">
      <EmptyState
        title={
          isNotFound
            ? t("agent.detail.notFound.title")
            : t("agent.detail.loadError.title")
        }
        description={
          isNotFound
            ? t("agent.detail.notFound.body")
            : error
              ? formatError(error)
              : t("agent.detail.loadError.body")
        }
        action={
          <Button
            variant="primary"
            onClick={isNotFound ? onHome : onRetry}
          >
            {isNotFound
              ? t("agent.detail.notFound.cta")
              : t("agent.detail.loadError.cta")}
          </Button>
        }
      />
    </div>
  );
}
