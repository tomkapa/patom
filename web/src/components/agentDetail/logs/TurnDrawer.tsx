// Per-turn drawer organism for the Logs & Metrics tab
// (doc/logs_metrics_tab.md §5.4). Renders four sections in order —
// Reasoning, Tool calls, Memory writes, Prompt used — backed by one
// `GET /turns/:request_id` call. Hooks: useTurnDetail (react-query).
//
// Atomic-design layering:
//   - atoms:     Spinner (loading state)
//   - molecules: Collapsible (reasoning + memory expand), ToolCallLine
//   - organism:  this file (composes the four sections)
//   - template:  AgentLogs.tsx (slice 1) mounts the drawer below an
//                expanded TurnsTimeline row.

import type { ReactNode } from "react";
import { AlertTriangle, Check, Link as LinkIcon, X } from "lucide-react";
import { Collapsible } from "../../molecules/Collapsible";
import { ToolCallLine } from "../../molecules/ToolCallLine";
import { Spinner } from "../../atoms/Spinner";
import { cn } from "../../../lib/utils";
import { formatMs } from "../../../lib/time";
import { useTurnDetail } from "../../../hooks/useAgentLogs";
import type {
  TurnDetail,
  TurnMemoryEvent,
  TurnReasoningBlock,
  TurnToolCall,
} from "../../../types/api";

export function TurnDrawer({
  requestId,
  onCollapse,
}: {
  requestId: string;
  /** Optional — when omitted, the "Collapse" chip is suppressed. The
   *  parent (`TurnsTimeline`) wires this to the row's expand-state
   *  toggle so a click on the chip behaves like a click on the row. */
  onCollapse?: () => void;
}) {
  const { data, isLoading, isError, error } = useTurnDetail(requestId);

  return (
    <div className="border-l-[3px] border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-8 py-5">
      {isLoading && <LoadingState />}
      {isError && <ErrorState error={error} />}
      {data && <DrawerBody detail={data} onCollapse={onCollapse} />}
    </div>
  );
}

function LoadingState() {
  return (
    <div className="flex items-center gap-2 py-4 text-[12px] text-[var(--color-muted)]">
      <Spinner size={12} />
      <span>Loading turn detail…</span>
    </div>
  );
}

function ErrorState({ error }: { error: unknown }) {
  const message =
    error instanceof Error ? error.message : "failed to load turn detail";
  return (
    <div className="flex items-start gap-2 border border-[var(--color-rose)] bg-[var(--color-rose-soft)] px-3 py-2 text-[12px] text-[var(--color-rose)]">
      <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
      <span>{message}</span>
    </div>
  );
}

function DrawerBody({
  detail,
  onCollapse,
}: {
  detail: TurnDetail;
  onCollapse?: () => void;
}) {
  const { turn, reasoning_blocks, tool_calls, memory_writes, prompt_version } =
    detail;
  return (
    <div className="flex flex-col gap-4">
      <DrawerHeader detail={detail} onCollapse={onCollapse} />
      {turn.failure_reason && (
        <FailureBanner
          reason={turn.failure_reason}
          stopReason={turn.stop_reason}
          durationMs={turn.duration_ms}
        />
      )}
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
        <ReasoningSection blocks={reasoning_blocks} />
        <div className="flex flex-col gap-4">
          <ToolCallsSection calls={tool_calls} />
          <MemoryWritesSection events={memory_writes} />
          <PromptUsedSection version={prompt_version} />
        </div>
      </div>
    </div>
  );
}

// ─── Header ────────────────────────────────────────────────────────────

function DrawerHeader({
  detail,
  onCollapse,
}: {
  detail: TurnDetail;
  onCollapse?: () => void;
}) {
  const { turn } = detail;
  return (
    <div className="flex flex-wrap items-center justify-between gap-3">
      <div className="flex flex-wrap items-center gap-2">
        <HeaderChip label="REQUEST" value={shortId(turn.request_id)} />
        <HeaderChip label="STOP" value={turn.stop_reason} />
        <HeaderChip
          label="HISTORY"
          value={`${turn.history_count} msgs`}
        />
        <HeaderChip
          label="PROMPT"
          value={`v${detail.prompt_version.version}`}
        />
      </div>
      {onCollapse && (
        <button
          type="button"
          onClick={onCollapse}
          className="flex items-center gap-1.5 border border-[var(--color-line)] bg-[var(--color-card)] px-2.5 py-1 text-[11px] text-[var(--color-muted)] transition-colors hover:text-[var(--color-ink)]"
        >
          <X className="h-3 w-3" strokeWidth={2} />
          <span>Collapse</span>
        </button>
      )}
    </div>
  );
}

function HeaderChip({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center gap-1.5 border border-[var(--color-line)] bg-[var(--color-card)] px-2 py-1">
      <span className="font-[var(--font-caption)] text-[9px] font-semibold tracking-[0.1em] text-[var(--color-muted)]">
        {label}
      </span>
      <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-ink)]">
        {value}
      </span>
    </div>
  );
}

function FailureBanner({
  reason,
  stopReason,
  durationMs,
}: {
  reason: string;
  stopReason: string;
  durationMs: number;
}) {
  return (
    <div className="border border-[var(--color-rose)] bg-[var(--color-rose-soft)]">
      <div className="flex items-center gap-2 px-3.5 py-2">
        <AlertTriangle className="h-3.5 w-3.5 text-[var(--color-rose)]" />
        <span className="font-[var(--font-caption)] text-[10px] font-semibold tracking-[0.1em] text-[var(--color-rose)]">
          FAILURE
        </span>
        <span className="font-[var(--font-mono)] text-[12px] font-semibold text-[var(--color-rose)]">
          {stopReason} · {formatMs(durationMs)}
        </span>
      </div>
      <div className="px-3.5 pb-3 text-[12px] text-[var(--color-ink)]">
        {reason}
      </div>
    </div>
  );
}

// ─── Reasoning ─────────────────────────────────────────────────────────

function ReasoningSection({ blocks }: { blocks: TurnReasoningBlock[] }) {
  const totalBytes = blocks.reduce((s, b) => s + b.byte_count, 0);
  return (
    <DrawerCard
      eyebrow={`REASONING · ${formatBytes(totalBytes)}`}
    >
      {blocks.length === 0 ? (
        <EmptyHint text="No reasoning blocks recorded for this turn." />
      ) : (
        <div className="px-4 py-3">
          <Collapsible
            trigger={`Show ${blocks.length} block${blocks.length === 1 ? "" : "s"}`}
          >
            <div className="flex flex-col gap-3 pt-2">
              {blocks.map((b, i) => (
                <pre
                  key={i}
                  className="whitespace-pre-wrap break-words font-[var(--font-mono)] text-[11.5px] leading-[1.5] text-[var(--color-ink-2)]"
                >
                  {b.text}
                </pre>
              ))}
            </div>
          </Collapsible>
        </div>
      )}
    </DrawerCard>
  );
}

// ─── Tool calls ────────────────────────────────────────────────────────

function ToolCallsSection({ calls }: { calls: TurnToolCall[] }) {
  return (
    <DrawerCard eyebrow={`TOOL CALLS · ${calls.length}`}>
      {calls.length === 0 ? (
        <EmptyHint text="No tool calls dispatched." />
      ) : (
        <div className="flex flex-col">
          {calls.map((c) => (
            <ToolCallRow key={c.id} call={c} />
          ))}
        </div>
      )}
    </DrawerCard>
  );
}

function ToolCallRow({ call }: { call: TurnToolCall }) {
  // Reuse the existing ToolCallLine molecule by translating the
  // per-turn shape into the streaming `ToolCallEntry` shape. Status
  // comes from `is_error`; input is absent on the per-turn endpoint
  // (no need — the line shows tool name + duration + success icon).
  return (
    <div className="border-b border-[var(--color-line)] px-3.5 py-2 last:border-b-0">
      <ToolCallLine
        call={{
          call_id: call.id,
          name: call.tool_name,
          input: call.mcp_server_alias
            ? { server: call.mcp_server_alias }
            : undefined,
          is_error: call.is_error,
          status: call.is_error ? "error" : "ok",
        }}
        durationMs={call.duration_ms}
      />
      {call.is_error && call.error_message && (
        <div className="mt-1 font-[var(--font-mono)] text-[11px] text-[var(--color-rose)]">
          {call.error_message}
        </div>
      )}
    </div>
  );
}

// ─── Memory writes ─────────────────────────────────────────────────────

function MemoryWritesSection({ events }: { events: TurnMemoryEvent[] }) {
  const counts = events.reduce(
    (acc, e) => {
      if (e.mutation === "write") acc.written += 1;
      else if (e.mutation === "update") acc.updated += 1;
      else if (e.mutation === "forget") acc.forgotten += 1;
      return acc;
    },
    { written: 0, updated: 0, forgotten: 0 },
  );
  const summary = formatMemorySummary(counts);

  return (
    <DrawerCard eyebrow="MEMORY WRITES">
      {events.length === 0 ? (
        <EmptyHint text="No memory writes from this turn." />
      ) : (
        <div className="px-4 py-3">
          <Collapsible trigger={summary}>
            <ul className="mt-2 flex flex-col gap-1.5">
              {events.map((e) => (
                <MemoryWriteRow key={e.id} event={e} />
              ))}
            </ul>
          </Collapsible>
        </div>
      )}
    </DrawerCard>
  );
}

function MemoryWriteRow({ event }: { event: TurnMemoryEvent }) {
  const verb =
    event.mutation === "write"
      ? "+"
      : event.mutation === "forget"
        ? "−"
        : "~";
  const tone =
    event.mutation === "write"
      ? "text-[var(--color-moss-deep)]"
      : event.mutation === "forget"
        ? "text-[var(--color-rose)]"
        : "text-[var(--color-ink)]";
  const shown = event.content_after ?? event.content_before ?? "—";
  return (
    <li className="flex items-start gap-2 font-[var(--font-mono)] text-[11.5px]">
      <span className={cn("shrink-0 font-bold", tone)}>{verb}</span>
      <span className="text-[var(--color-ink)] break-words">{shown}</span>
    </li>
  );
}

// ─── Prompt used ───────────────────────────────────────────────────────

function PromptUsedSection({
  version,
}: {
  version: TurnDetail["prompt_version"];
}) {
  return (
    <DrawerCard
      eyebrow={`PROMPT USED · v${version.version}`}
    >
      {/* <details> per CLAUDE.md guidance — collapsed by default so the
          drawer height stays manageable until the operator drills in. */}
      <details className="group">
        <summary className="flex cursor-pointer list-none items-center gap-2 px-4 py-3 text-[12px] text-[var(--color-muted)] hover:text-[var(--color-ink)]">
          <LinkIcon className="h-3 w-3" />
          <span>
            {version.model ?? "workspace default model"} · view system prompt
          </span>
        </summary>
        <div className="border-t border-[var(--color-line)] px-4 py-3">
          <pre className="whitespace-pre-wrap break-words font-[var(--font-mono)] text-[11.5px] leading-[1.5] text-[var(--color-ink-2)]">
            {version.system_prompt}
          </pre>
        </div>
      </details>
    </DrawerCard>
  );
}

// ─── Building blocks ───────────────────────────────────────────────────

function DrawerCard({
  eyebrow,
  children,
}: {
  eyebrow: string;
  children: ReactNode;
}) {
  return (
    <section className="flex flex-col border border-[var(--color-line)] bg-[var(--color-card)]">
      <header className="flex items-center justify-between border-b border-[var(--color-line)] px-3.5 py-2.5">
        <span className="font-[var(--font-caption)] text-[10px] font-semibold tracking-[0.12em] text-[var(--color-muted)]">
          {eyebrow}
        </span>
      </header>
      {children}
    </section>
  );
}

function EmptyHint({ text }: { text: string }) {
  return (
    <div className="flex items-center gap-2 px-4 py-3 text-[12px] text-[var(--color-muted-2)]">
      <Check className="h-3 w-3" />
      <span>{text}</span>
    </div>
  );
}

// ─── Pure helpers ──────────────────────────────────────────────────────

function shortId(id: string): string {
  if (id.length <= 12) return id;
  return `${id.slice(0, 8)}…${id.slice(-4)}`;
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  return `${(n / 1024).toFixed(1)} KB`;
}

function formatMemorySummary(c: {
  written: number;
  updated: number;
  forgotten: number;
}): string {
  const parts: string[] = [];
  if (c.written) parts.push(`+${c.written} written`);
  if (c.updated) parts.push(`~${c.updated} updated`);
  if (c.forgotten) parts.push(`−${c.forgotten} forgotten`);
  return parts.length ? parts.join(" · ") : "no changes";
}
