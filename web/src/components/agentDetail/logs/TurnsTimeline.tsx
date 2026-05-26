import { useMemo, useState } from "react";
import { SectionCard } from "../../molecules/SectionCard";
import { Button } from "../../atoms/Button";
import { Spinner } from "../../atoms/Spinner";
import { EmptyState } from "../../molecules/EmptyState";
import type { AgentTurnRow } from "../../../types/api";
import { TurnDrawer } from "./TurnDrawer";

const COL_WIDTHS =
  "minmax(90px,110px) minmax(60px,80px) minmax(80px,110px) minmax(140px,200px) minmax(80px,110px) minmax(80px,110px) minmax(0,1fr) 28px";

type SortKey = "started_at" | "prompt_version" | "kind" | "model" | "tokens" | "latency" | "outcome";
type SortDir = "asc" | "desc";

/** Mirrors the grid in AgentActivityCard. Sortable headers, reflection
 *  rows tinted moss, failed rows tinted rose, prompt-edit separator rows
 *  inserted whenever consecutive turns straddle a version boundary, and
 *  a "Load more" button for `useInfiniteQuery` cursor pagination.
 *
 *  Click a row to mount `TurnDrawer` in-place below it (doc §5.4). */
export function TurnsTimeline({
  pages,
  isLoading,
  hasNextPage,
  isFetchingNextPage,
  onLoadMore,
  onSeparatorClick,
}: {
  pages: AgentTurnRow[];
  isLoading: boolean;
  hasNextPage: boolean;
  isFetchingNextPage: boolean;
  onLoadMore: () => void;
  /** Click on a `── prompt edited · v6 → v7 ──` separator opens the
   *  prompt-diff modal for the newer version (slice 3 + doc §5.3). */
  onSeparatorClick?: (newerVersion: number) => void;
}) {
  const [sortKey, setSortKey] = useState<SortKey>("started_at");
  const [sortDir, setSortDir] = useState<SortDir>("desc");
  const [expanded, setExpanded] = useState<string | null>(null);

  const sorted = useMemo(() => sortRows(pages, sortKey, sortDir), [pages, sortKey, sortDir]);

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

  return (
    <SectionCard
      header={
        <div className="flex items-baseline justify-between gap-3 border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-3">
          <div className="flex flex-col gap-0.5">
            <span className="font-[var(--font-mono)] text-[10px] font-semibold tracking-[0.15em] uppercase text-[var(--color-muted)]">
              TURNS
            </span>
            <h2 className="font-[var(--font-display)] text-[15px] font-semibold text-[var(--color-ink)]">
              Recent turns
            </h2>
          </div>
          <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]">
            {pages.length} in window
          </span>
        </div>
      }
    >
      <div
        className="grid items-center gap-3 border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-2.5 font-[var(--font-mono)] text-[9px] tracking-[0.15em] uppercase text-[var(--color-muted)]"
        style={{ gridTemplateColumns: COL_WIDTHS }}
      >
        <HeaderCell label="TIME" active={sortKey === "started_at"} dir={sortDir} onClick={() => toggleSort("started_at")} />
        <HeaderCell label="PROMPT" active={sortKey === "prompt_version"} dir={sortDir} onClick={() => toggleSort("prompt_version")} />
        <HeaderCell label="KIND" active={sortKey === "kind"} dir={sortDir} onClick={() => toggleSort("kind")} />
        <HeaderCell label="MODEL" active={sortKey === "model"} dir={sortDir} onClick={() => toggleSort("model")} />
        <HeaderCell label="TOKENS" active={sortKey === "tokens"} dir={sortDir} onClick={() => toggleSort("tokens")} />
        <HeaderCell label="LATENCY" active={sortKey === "latency"} dir={sortDir} onClick={() => toggleSort("latency")} />
        <HeaderCell label="OUTCOME" active={sortKey === "outcome"} dir={sortDir} onClick={() => toggleSort("outcome")} />
        <span />
      </div>
      {isLoading && rows.length === 0 ? (
        <div className="flex items-center justify-center px-5 py-8">
          <Spinner size={14} />
        </div>
      ) : rows.length === 0 ? (
        <div className="px-5 py-8">
          <EmptyState title="No turns in this window" description="Adjust the time range or send a new prompt to populate the timeline." />
        </div>
      ) : (
        rows.map((r, i) =>
          r.type === "sep" ? (
            <SeparatorRow
              key={`sep-${i}`}
              from={r.from}
              to={r.to}
              onClick={
                onSeparatorClick ? () => onSeparatorClick(r.to) : undefined
              }
            />
          ) : (
            <TurnRowView
              key={r.row.request_id}
              row={r.row}
              open={expanded === r.row.request_id}
              onToggle={() => toggleExpanded(r.row.request_id)}
            />
          ),
        )
      )}
      {hasNextPage ? (
        <div className="flex items-center justify-center border-t border-[var(--color-line)] px-5 py-3">
          <Button variant="ghost" size="sm" onClick={onLoadMore} disabled={isFetchingNextPage}>
            {isFetchingNextPage ? "Loading…" : "Load more"}
          </Button>
        </div>
      ) : null}
    </SectionCard>
  );
}

function HeaderCell({
  label,
  active,
  dir,
  onClick,
}: {
  label: string;
  active: boolean;
  dir: SortDir;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`text-left cursor-pointer ${active ? "text-[var(--color-ink)]" : ""}`}
    >
      {label}
      {active ? (dir === "asc" ? " ▲" : " ▼") : ""}
    </button>
  );
}

function TurnRowView({
  row,
  open,
  onToggle,
}: {
  row: AgentTurnRow;
  open: boolean;
  onToggle: () => void;
}) {
  const tone =
    row.status === "failed"
      ? "bg-[var(--color-rose-soft)]"
      : row.kind === "reflection"
        ? "bg-[var(--color-moss-tint)]"
        : "";
  const tokenTotal = row.input_tokens + row.output_tokens;
  const tooltip = `in ${row.input_tokens.toLocaleString()} · out ${row.output_tokens.toLocaleString()}${row.cache_read_tokens ? ` · cache_read ${row.cache_read_tokens.toLocaleString()}` : ""}${row.cache_creation_tokens ? ` · cache_creation ${row.cache_creation_tokens.toLocaleString()}` : ""}`;
  const outcome =
    row.status === "failed"
      ? row.failure_reason ?? "failed"
      : row.stop_reason === "length"
        ? "✗ tmo"
        : "✓";
  return (
    <>
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={open}
        className={`grid w-full items-center gap-3 border-b border-[var(--color-line)] px-5 py-2.5 text-left last:border-b-0 hover:bg-[var(--color-paper-2)] ${tone}`}
        style={{ gridTemplateColumns: COL_WIDTHS }}
      >
        <span className="font-[var(--font-mono)] text-[12px] text-[var(--color-muted)]">
          {formatTime(row.started_at)}
        </span>
        <span className="font-[var(--font-mono)] text-[12px] text-[var(--color-ink)]">
          v{row.prompt_version}
        </span>
        <span className="font-[var(--font-mono)] text-[12px] text-[var(--color-ink)]">
          {row.kind}
        </span>
        <span className="truncate font-[var(--font-mono)] text-[12px] text-[var(--color-ink)]" title={row.model}>
          {row.model}
        </span>
        <span className="font-[var(--font-mono)] text-[12px] text-[var(--color-ink)]" title={tooltip}>
          {tokenTotal.toLocaleString()}
        </span>
        <span className="font-[var(--font-mono)] text-[12px] text-[var(--color-ink)]">
          {formatMs(row.duration_ms)}
        </span>
        <span
          className={`font-[var(--font-mono)] text-[12px] ${row.status === "failed" ? "text-[var(--color-rose)] font-semibold" : "text-[var(--color-ink)]"}`}
        >
          {outcome}
        </span>
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

function SeparatorRow({
  from,
  to,
  onClick,
}: {
  from: number;
  to: number;
  onClick?: () => void;
}) {
  const content = <>── prompt edited · v{from} → v{to} ──</>;
  const className =
    "flex w-full items-center justify-center gap-2 border-b border-dashed border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-2 font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]";
  if (onClick) {
    return (
      <button
        type="button"
        onClick={onClick}
        className={`${className} cursor-pointer hover:text-[var(--color-ink)]`}
      >
        {content}
      </button>
    );
  }
  return <div className={className}>{content}</div>;
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

function formatMs(n: number): string {
  if (n >= 1000) return `${(n / 1000).toFixed(1)}s`;
  return `${n}ms`;
}
