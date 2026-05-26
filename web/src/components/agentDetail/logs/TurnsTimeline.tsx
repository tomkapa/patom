import { useMemo, useState, type ReactNode } from "react";
import {
  ArrowDownNarrowWide,
  ArrowRight,
  ArrowUpNarrowWide,
  ChevronsUpDown,
  Columns3,
  GitCommitHorizontal,
  ListFilter,
  Lock,
  RotateCcw,
} from "lucide-react";
import { SectionCard } from "../../molecules/SectionCard";
import { Dropdown } from "../../molecules/Dropdown";
import { Button } from "../../atoms/Button";
import { Spinner } from "../../atoms/Spinner";
import { EmptyState } from "../../molecules/EmptyState";
import type { AgentTurnRow, PromptEditMarker } from "../../../types/api";
import { TurnDrawer } from "./TurnDrawer";
import { useT } from "../../../i18n";
import type { TranslationKey } from "../../../i18n/en";
import { cn } from "../../../lib/utils";

// ── Column model ──────────────────────────────────────────────────────────
// Mirrors pencil frame `b70ay3` (the floating "Columns" panel): 11 columns,
// 8 visible by default. `time` is locked — design shows it with a lock icon
// and no drag handle — so users can't hide the time pivot.

type ColumnId =
  | "time"
  | "prompt"
  | "kind"
  | "model"
  | "provider"
  | "tokens"
  | "cache_read"
  | "latency"
  | "context"
  | "stop_reason"
  | "request_id";

type ColumnDef = {
  id: ColumnId;
  /** i18n key for the column's display label. */
  labelKey: TranslationKey;
  /** Tailwind grid `minmax(...)` width. */
  width: string;
  /** True for columns that cannot be hidden. */
  locked?: boolean;
  /** Default-on per the design — 8 of 11. */
  defaultVisible: boolean;
  /** Sort key if this column drives sort. */
  sortKey?: SortKey;
};

const COLUMNS: ColumnDef[] = [
  { id: "time", labelKey: "agent.detail.logs.columns.time", width: "minmax(96px,110px)", locked: true, defaultVisible: true, sortKey: "started_at" },
  { id: "prompt", labelKey: "agent.detail.logs.columns.prompt", width: "minmax(60px,80px)", defaultVisible: true, sortKey: "prompt_version" },
  { id: "kind", labelKey: "agent.detail.logs.columns.kind", width: "minmax(90px,110px)", defaultVisible: true, sortKey: "kind" },
  { id: "model", labelKey: "agent.detail.logs.columns.model", width: "minmax(140px,200px)", defaultVisible: true, sortKey: "model" },
  { id: "provider", labelKey: "agent.detail.logs.columns.provider", width: "minmax(90px,120px)", defaultVisible: false },
  { id: "tokens", labelKey: "agent.detail.logs.columns.tokens", width: "minmax(80px,110px)", defaultVisible: true, sortKey: "tokens" },
  { id: "cache_read", labelKey: "agent.detail.logs.columns.cacheRead", width: "minmax(90px,120px)", defaultVisible: false },
  { id: "latency", labelKey: "agent.detail.logs.columns.latency", width: "minmax(80px,110px)", defaultVisible: true, sortKey: "latency" },
  { id: "context", labelKey: "agent.detail.logs.columns.context", width: "minmax(80px,110px)", defaultVisible: true },
  { id: "stop_reason", labelKey: "agent.detail.logs.columns.stopReason", width: "minmax(110px,1fr)", defaultVisible: true, sortKey: "outcome" },
  { id: "request_id", labelKey: "agent.detail.logs.columns.requestId", width: "minmax(110px,1fr)", defaultVisible: false },
];

const DEFAULT_VISIBLE = COLUMNS.filter((c) => c.defaultVisible).map((c) => c.id);
const EXPAND_COL_WIDTH = "28px";

type SortKey = "started_at" | "prompt_version" | "kind" | "model" | "tokens" | "latency" | "outcome";
type SortDir = "asc" | "desc";

/** Mirrors the grid in AgentActivityCard. Sortable headers, reflection
 *  rows tinted moss, failed rows tinted rose, prompt-edit separator banners
 *  inserted whenever consecutive turns straddle a version boundary, and
 *  a "Load more" button for `useInfiniteQuery` cursor pagination.
 *
 *  Click a row to mount `TurnDrawer` in-place below it. */
export function TurnsTimeline({
  pages,
  isLoading,
  hasNextPage,
  isFetchingNextPage,
  onLoadMore,
  onSeparatorClick,
  promptEdits,
}: {
  pages: AgentTurnRow[];
  isLoading: boolean;
  hasNextPage: boolean;
  isFetchingNextPage: boolean;
  onLoadMore: () => void;
  /** Click on a separator banner opens the prompt-diff modal for the
   *  *newer* version (the doc calls this slice 3 + §5.3). */
  onSeparatorClick?: (newerVersion: number) => void;
  /** Prompt-edit markers from the metrics endpoint. Used to surface
   *  author/timestamp on the separator banner. */
  promptEdits?: PromptEditMarker[];
}) {
  const { t } = useT();
  const [sortKey, setSortKey] = useState<SortKey>("started_at");
  const [sortDir, setSortDir] = useState<SortDir>("desc");
  const [expanded, setExpanded] = useState<string | null>(null);
  const [failedOnly, setFailedOnly] = useState(false);
  const [visibleCols, setVisibleCols] = useState<Set<ColumnId>>(
    () => new Set<ColumnId>(DEFAULT_VISIBLE),
  );

  const filtered = useMemo(
    () => (failedOnly ? pages.filter((p) => p.status === "failed") : pages),
    [pages, failedOnly],
  );

  const sorted = useMemo(() => sortRows(filtered, sortKey, sortDir), [filtered, sortKey, sortDir]);

  // Insert separator rows between consecutive turns that straddle a
  // version boundary. The sorted list runs newest-first; a separator
  // appears before the *older* version's first row.
  const rows = useMemo<
    Array<{ type: "row"; row: AgentTurnRow } | { type: "sep"; from: number; to: number }>
  >(() => {
    const out: Array<{ type: "row"; row: AgentTurnRow } | { type: "sep"; from: number; to: number }> = [];
    for (let i = 0; i < sorted.length; i++) {
      const r = sorted[i]!;
      out.push({ type: "row", row: r });
      const next = sorted[i + 1];
      if (next && next.prompt_version !== r.prompt_version) {
        out.push({ type: "sep", from: r.prompt_version, to: next.prompt_version });
      }
    }
    return out;
  }, [sorted]);

  const visibleColumnDefs = useMemo(
    () => COLUMNS.filter((c) => visibleCols.has(c.id)),
    [visibleCols],
  );

  const gridTemplate = useMemo(
    () => `${visibleColumnDefs.map((c) => c.width).join(" ")} ${EXPAND_COL_WIDTH}`,
    [visibleColumnDefs],
  );

  const toggleSort = (k: SortKey) => {
    if (sortKey === k) {
      setSortDir((d) => (d === "asc" ? "desc" : "asc"));
    } else {
      setSortKey(k);
      setSortDir("desc");
    }
  };

  const toggleExpanded = (requestId: string) => {
    setExpanded((cur) => (cur === requestId ? null : requestId));
  };

  const toggleColumn = (id: ColumnId) => {
    setVisibleCols((cur) => {
      const next = new Set(cur);
      if (next.has(id)) {
        const def = COLUMNS.find((c) => c.id === id);
        if (def?.locked) return cur;
        next.delete(id);
      } else {
        next.add(id);
      }
      return next;
    });
  };

  const resetColumns = () => setVisibleCols(new Set<ColumnId>(DEFAULT_VISIBLE));

  return (
    <SectionCard
      header={
        <TimelineHeader
          totalInWindow={pages.length}
          failedOnly={failedOnly}
          onToggleFailed={() => setFailedOnly((v) => !v)}
          visibleCols={visibleCols}
          onToggleColumn={toggleColumn}
          onResetColumns={resetColumns}
        />
      }
    >
      <div
        className="grid items-center gap-3 border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-8 py-2.5 font-[var(--font-mono)] text-[10px] font-semibold tracking-[0.12em] uppercase text-[var(--color-muted)]"
        style={{ gridTemplateColumns: gridTemplate }}
      >
        {visibleColumnDefs.map((col) => (
          <HeaderCell
            key={col.id}
            label={t(col.labelKey).toUpperCase()}
            sortable={Boolean(col.sortKey)}
            active={col.sortKey != null && sortKey === col.sortKey}
            dir={sortDir}
            onClick={col.sortKey ? () => toggleSort(col.sortKey!) : undefined}
          />
        ))}
        <span />
      </div>
      {isLoading && rows.length === 0 ? (
        <div className="flex items-center justify-center px-8 py-8">
          <Spinner size={14} />
        </div>
      ) : rows.length === 0 ? (
        <div className="px-8 py-8">
          <EmptyState
            title={t(
              failedOnly
                ? "agent.detail.logs.turns.emptyFailed.title"
                : "agent.detail.logs.turns.empty.title",
            )}
            description={t(
              failedOnly
                ? "agent.detail.logs.turns.emptyFailed.body"
                : "agent.detail.logs.turns.empty.body",
            )}
          />
        </div>
      ) : (
        rows.map((r, i) =>
          r.type === "sep" ? (
            <SeparatorBanner
              key={`sep-${i}`}
              from={r.from}
              to={r.to}
              edit={promptEdits?.find((e) => e.version === r.from)}
              onClick={onSeparatorClick ? () => onSeparatorClick(r.from) : undefined}
            />
          ) : (
            <TurnRowView
              key={r.row.request_id}
              row={r.row}
              open={expanded === r.row.request_id}
              onToggle={() => toggleExpanded(r.row.request_id)}
              visibleColumnDefs={visibleColumnDefs}
              gridTemplate={gridTemplate}
            />
          ),
        )
      )}
      {hasNextPage ? (
        <div className="flex items-center justify-center border-t border-[var(--color-line)] px-5 py-3">
          <Button variant="ghost" size="sm" onClick={onLoadMore} disabled={isFetchingNextPage}>
            {isFetchingNextPage
              ? t("agent.detail.logs.turns.loading")
              : t("agent.detail.logs.turns.loadMore")}
          </Button>
        </div>
      ) : null}
    </SectionCard>
  );
}

function TimelineHeader({
  totalInWindow,
  failedOnly,
  onToggleFailed,
  visibleCols,
  onToggleColumn,
  onResetColumns,
}: {
  totalInWindow: number;
  failedOnly: boolean;
  onToggleFailed: () => void;
  visibleCols: Set<ColumnId>;
  onToggleColumn: (id: ColumnId) => void;
  onResetColumns: () => void;
}) {
  const { t } = useT();
  return (
    <div className="flex flex-wrap items-end justify-between gap-3 border-b border-[var(--color-line)] bg-[var(--color-card)] px-8 py-4">
      <div className="flex flex-col gap-1">
        <span className="font-[var(--font-mono)] text-[10px] font-medium tracking-[0.15em] uppercase text-[var(--color-muted)]">
          {t("agent.detail.logs.turns.eyebrow", { count: totalInWindow })}
        </span>
        <div className="flex items-baseline gap-3">
          <h2 className="font-[var(--font-display)] text-[18px] font-semibold text-[var(--color-ink)]">
            {t("agent.detail.logs.turns.heading")}
          </h2>
          <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]">
            {t("agent.detail.logs.turns.subtitle")}
          </span>
        </div>
      </div>
      <div className="flex items-center gap-2">
        <HeaderPillButton
          onClick={onToggleFailed}
          active={failedOnly}
          icon={<ListFilter className="h-3 w-3" />}
          label={t("agent.detail.logs.turns.failedOnly")}
          ariaPressed={failedOnly}
        />
        <ColumnsDropdown
          visibleCols={visibleCols}
          onToggle={onToggleColumn}
          onReset={onResetColumns}
        />
      </div>
    </div>
  );
}

function HeaderPillButton({
  onClick,
  active,
  icon,
  label,
  ariaPressed,
}: {
  onClick: () => void;
  active?: boolean;
  icon: ReactNode;
  label: string;
  ariaPressed?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={ariaPressed}
      className={cn(
        "flex cursor-pointer items-center gap-1.5 border px-3 py-1.5 font-[var(--font-body)] text-[12px] font-medium outline-none transition-colors duration-150 ease-out focus-visible:ring-1 focus-visible:ring-[var(--color-ink)]",
        active
          ? "border-[var(--color-moss)] bg-[var(--color-moss-tint)] text-[var(--color-moss-deep)]"
          : "border-[var(--color-line)] bg-[var(--color-card)] text-[var(--color-ink)] hover:bg-[var(--color-paper-2)]",
      )}
    >
      <span
        className={cn(
          "shrink-0",
          active ? "text-[var(--color-moss)]" : "text-[var(--color-muted-2)]",
        )}
      >
        {icon}
      </span>
      <span>{label}</span>
    </button>
  );
}

function ColumnsDropdown({
  visibleCols,
  onToggle,
  onReset,
}: {
  visibleCols: Set<ColumnId>;
  onToggle: (id: ColumnId) => void;
  onReset: () => void;
}) {
  const { t } = useT();
  const shownCount = visibleCols.size;
  const total = COLUMNS.length;
  return (
    <Dropdown
      placement="bottom-start"
      menuClassName="w-[228px] border border-[var(--color-line)] bg-[var(--color-card)] shadow-md"
      renderTrigger={({ open, toggle }) => (
        <button
          type="button"
          onClick={toggle}
          aria-haspopup="menu"
          aria-expanded={open}
          className="flex cursor-pointer items-center gap-1.5 border border-[var(--color-line)] bg-[var(--color-card)] px-3 py-1.5 font-[var(--font-body)] text-[12px] font-medium text-[var(--color-ink)] outline-none transition-colors duration-150 ease-out hover:bg-[var(--color-paper-2)] focus-visible:ring-1 focus-visible:ring-[var(--color-ink)]"
        >
          <Columns3 className="h-3 w-3 text-[var(--color-muted-2)]" />
          <span>{t("agent.detail.logs.turns.columns")}</span>
        </button>
      )}
    >
      {() => (
        <div className="flex flex-col">
          <div className="flex flex-col gap-1 border-b border-[var(--color-line)] px-4 py-3">
            <span className="font-[var(--font-mono)] text-[10px] font-semibold tracking-[0.12em] uppercase text-[var(--color-muted)]">
              {t("agent.detail.logs.columns.eyebrow")}
            </span>
            <span className="font-[var(--font-mono)] text-[10px] text-[var(--color-muted)]">
              {t("agent.detail.logs.columns.caption")}
            </span>
          </div>
          <ul className="flex flex-col">
            {COLUMNS.map((col) => {
              const checked = visibleCols.has(col.id);
              return (
                <li key={col.id} className="border-b border-[var(--color-line)]">
                  <button
                    type="button"
                    onClick={() => {
                      if (!col.locked) onToggle(col.id);
                    }}
                    disabled={col.locked}
                    aria-pressed={checked}
                    className={cn(
                      "flex w-full items-center gap-2 px-4 py-1.5 text-left text-[12px] outline-none transition-colors duration-100 ease-out",
                      col.locked
                        ? "cursor-not-allowed"
                        : "cursor-pointer hover:bg-[var(--color-paper-2)]",
                    )}
                  >
                    <CheckboxBox checked={checked} />
                    <span
                      className={cn(
                        "flex-1 font-[var(--font-body)] font-medium",
                        checked
                          ? "text-[var(--color-ink)]"
                          : "text-[var(--color-muted)]",
                      )}
                    >
                      {t(col.labelKey)}
                    </span>
                    {col.locked ? (
                      <Lock className="h-3 w-3 text-[var(--color-muted)]" />
                    ) : null}
                  </button>
                </li>
              );
            })}
          </ul>
          <div className="flex items-center justify-between bg-[var(--color-paper-2)] px-4 py-2.5">
            <button
              type="button"
              onClick={onReset}
              className="flex cursor-pointer items-center gap-1 font-[var(--font-body)] text-[11px] font-medium text-[var(--color-moss)] hover:text-[var(--color-moss-deep)]"
            >
              <RotateCcw className="h-3 w-3" />
              {t("agent.detail.logs.columns.reset")}
            </button>
            <span className="font-[var(--font-mono)] text-[10px] text-[var(--color-muted)]">
              {t("agent.detail.logs.columns.counter", {
                shown: shownCount,
                total,
              })}
            </span>
          </div>
        </div>
      )}
    </Dropdown>
  );
}

function CheckboxBox({ checked }: { checked: boolean }) {
  return (
    <span
      className={cn(
        "flex h-3.5 w-3.5 shrink-0 items-center justify-center border",
        checked
          ? "border-[var(--color-moss)] bg-[var(--color-moss)] text-[var(--color-card)]"
          : "border-[var(--color-line-2)] bg-[var(--color-card)]",
      )}
      aria-hidden
    >
      {checked ? (
        <svg viewBox="0 0 10 10" className="h-2 w-2" fill="none">
          <path
            d="M1.5 5.5L4 8L8.5 2"
            stroke="currentColor"
            strokeWidth="1.8"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        </svg>
      ) : null}
    </span>
  );
}

function HeaderCell({
  label,
  sortable,
  active,
  dir,
  onClick,
}: {
  label: string;
  sortable: boolean;
  active: boolean;
  dir: SortDir;
  onClick?: () => void;
}) {
  const icon = !sortable ? null : active ? (
    dir === "asc" ? (
      <ArrowUpNarrowWide className="h-3 w-3 text-[var(--color-ink)]" />
    ) : (
      <ArrowDownNarrowWide className="h-3 w-3 text-[var(--color-ink)]" />
    )
  ) : (
    <ChevronsUpDown className="h-3 w-3 text-[var(--color-muted-2)]" />
  );
  const className = cn(
    "flex items-center gap-1 text-left",
    sortable && "cursor-pointer",
    active ? "text-[var(--color-ink)]" : "",
  );
  if (sortable && onClick) {
    return (
      <button type="button" onClick={onClick} className={className}>
        <span>{label}</span>
        {icon}
      </button>
    );
  }
  return (
    <span className={className}>
      <span>{label}</span>
      {icon}
    </span>
  );
}

function TurnRowView({
  row,
  open,
  onToggle,
  visibleColumnDefs,
  gridTemplate,
}: {
  row: AgentTurnRow;
  open: boolean;
  onToggle: () => void;
  visibleColumnDefs: ColumnDef[];
  gridTemplate: string;
}) {
  const tone =
    row.status === "failed"
      ? "bg-[var(--color-rose-soft)]"
      : row.kind === "reflection"
        ? "bg-[var(--color-moss-tint)]"
        : "";
  return (
    <>
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={open}
        className={cn(
          "grid w-full items-center gap-3 border-b border-[var(--color-line)] px-8 py-2.5 text-left last:border-b-0 hover:bg-[var(--color-paper-2)]",
          tone,
        )}
        style={{ gridTemplateColumns: gridTemplate }}
      >
        {visibleColumnDefs.map((col) => (
          <RowCell key={col.id} col={col} row={row} />
        ))}
        <span className="text-[var(--color-muted)]" aria-hidden>
          {open ? "▾" : "▸"}
        </span>
      </button>
      {open ? (
        <TurnDrawer
          requestId={row.request_id}
          onCollapse={() => onToggle()}
        />
      ) : null}
    </>
  );
}

function RowCell({ col, row }: { col: ColumnDef; row: AgentTurnRow }) {
  const { t } = useT();
  const failed = row.status === "failed";
  const baseCls = "font-[var(--font-mono)] text-[12px]";
  const tokenTotal = row.input_tokens + row.output_tokens;
  const tokenTooltip = [
    t("agent.detail.logs.turns.tokens.tooltip.in", {
      n: row.input_tokens.toLocaleString(),
    }),
    t("agent.detail.logs.turns.tokens.tooltip.out", {
      n: row.output_tokens.toLocaleString(),
    }),
    row.cache_read_tokens
      ? t("agent.detail.logs.turns.tokens.tooltip.cacheRead", {
          n: row.cache_read_tokens.toLocaleString(),
        })
      : null,
    row.cache_creation_tokens
      ? t("agent.detail.logs.turns.tokens.tooltip.cacheCreation", {
          n: row.cache_creation_tokens.toLocaleString(),
        })
      : null,
  ]
    .filter(Boolean)
    .join(" · ");
  const failedLabel =
    row.failure_reason ?? t("agent.detail.logs.turns.stopReason.failed");

  switch (col.id) {
    case "time":
      return (
        <span className={cn(baseCls, "text-[var(--color-muted)]")}>
          {formatTime(row.started_at)}
        </span>
      );
    case "prompt":
      return (
        <span className={cn(baseCls, "text-[var(--color-ink)]")}>
          v{row.prompt_version}
        </span>
      );
    case "kind":
      return (
        <span className={cn(baseCls, "text-[var(--color-ink)]")}>
          {row.kind}
        </span>
      );
    case "model":
      return (
        <span
          className={cn(baseCls, "truncate text-[var(--color-ink)]")}
          title={row.model}
        >
          {row.model}
        </span>
      );
    case "provider":
      return (
        <span className={cn(baseCls, "text-[var(--color-ink)]")}>
          {row.provider}
        </span>
      );
    case "tokens":
      return (
        <span
          className={cn(baseCls, "text-[var(--color-ink)]")}
          title={tokenTooltip}
        >
          {tokenTotal.toLocaleString()}
        </span>
      );
    case "cache_read":
      return (
        <span className={cn(baseCls, "text-[var(--color-ink)]")}>
          {(row.cache_read_tokens ?? 0).toLocaleString()}
        </span>
      );
    case "latency":
      return (
        <span className={cn(baseCls, "text-[var(--color-ink)]")}>
          {formatMs(row.duration_ms)}
        </span>
      );
    case "context":
      return (
        <span className={cn(baseCls, "text-[var(--color-ink)]")}>
          {t("agent.detail.logs.turns.context.value", {
            n: row.history_count,
          })}
        </span>
      );
    case "stop_reason":
      if (failed) {
        return (
          <span
            className={cn(
              baseCls,
              "font-semibold text-[var(--color-rose)]",
            )}
            title={failedLabel}
          >
            {failedLabel}
          </span>
        );
      }
      return (
        <span className={cn(baseCls, "text-[var(--color-ink)]")}>
          {row.stop_reason ?? "—"}
        </span>
      );
    case "request_id":
      return (
        <span
          className={cn(baseCls, "truncate text-[var(--color-muted)]")}
          title={row.request_id}
        >
          {row.request_id.slice(0, 8)}…
        </span>
      );
  }
}

function SeparatorBanner({
  from,
  to,
  edit,
  onClick,
}: {
  from: number;
  to: number;
  edit?: PromptEditMarker;
  onClick?: () => void;
}) {
  const { t } = useT();
  const author =
    edit?.edited_by ?? t("agent.detail.logs.turns.separator.author.system");
  const time = edit?.created_at ? formatHM(edit.created_at) : null;
  const content = (
    <div className="flex w-full items-center gap-3 px-8 py-2.5 text-left">
      <GitCommitHorizontal className="h-3.5 w-3.5 shrink-0 text-[var(--color-card)]/80" />
      <span className="font-[var(--font-body)] text-[12px] font-medium text-[var(--color-card)]">
        {t("agent.detail.logs.turns.separator.label", {
          author,
          from: to,
          to: from,
        })}
      </span>
      {time ? (
        <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-card)]/70">
          {time}
        </span>
      ) : null}
      <span className="flex-1" />
      <span className="font-[var(--font-mono)] text-[10px] font-semibold tracking-[0.12em] uppercase text-[var(--color-card)]/85">
        {t("agent.detail.logs.turns.separator.viewDiff")}
      </span>
      <ArrowRight className="h-3 w-3 shrink-0 text-[var(--color-card)]/85" />
    </div>
  );
  const cls = "block w-full bg-[var(--color-ink)]";
  if (onClick) {
    return (
      <button
        type="button"
        onClick={onClick}
        className={`${cls} cursor-pointer transition-colors duration-150 ease-out hover:bg-[var(--color-ink-2)]`}
      >
        {content}
      </button>
    );
  }
  return <div className={cls}>{content}</div>;
}

function sortRows(rows: AgentTurnRow[], key: SortKey, dir: SortDir): AgentTurnRow[] {
  const sorted = [...rows];
  const mul = dir === "asc" ? 1 : -1;
  sorted.sort((a, b) => mul * compareBy(a, b, key));
  return sorted;
}

function compareBy(a: AgentTurnRow, b: AgentTurnRow, key: SortKey): number {
  switch (key) {
    case "started_at":
      return a.started_at < b.started_at ? -1 : a.started_at > b.started_at ? 1 : 0;
    case "prompt_version":
      return a.prompt_version - b.prompt_version;
    case "kind":
      return a.kind.localeCompare(b.kind);
    case "model":
      return a.model.localeCompare(b.model);
    case "tokens":
      return a.input_tokens + a.output_tokens - (b.input_tokens + b.output_tokens);
    case "latency":
      return a.duration_ms - b.duration_ms;
    case "outcome":
      return (a.status === "failed" ? 1 : 0) - (b.status === "failed" ? 1 : 0);
  }
}

function formatTime(iso: string): string {
  const d = new Date(iso);
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  const ss = String(d.getSeconds()).padStart(2, "0");
  return `${hh}:${mm}:${ss}`;
}

function formatHM(iso: string): string {
  const d = new Date(iso);
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  return `${hh}:${mm}`;
}

function formatMs(n: number): string {
  if (n >= 1000) return `${(n / 1000).toFixed(1)}s`;
  return `${n}ms`;
}
