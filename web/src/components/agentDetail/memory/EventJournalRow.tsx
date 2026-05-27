import { MessageSquare, RotateCcw } from "lucide-react";
import { Link } from "react-router-dom";
import { cn } from "../../../lib/utils";
import { clockTime, longDate } from "../../../lib/time";
import { useT } from "../../../i18n";
import type {
  MemoryEvent,
  MutationKind,
  MutationSource,
} from "../../../types/api";

const SOURCE_COLOR: Record<MutationSource, { fill: string; ink: string; border: string }> =
  {
    turn: { fill: "#EFF6FF", ink: "#1D4ED8", border: "#1D4ED8" },
    operator: { fill: "#F5F3FF", ink: "#7C3AED", border: "#7C3AED" },
    librarian: { fill: "#ECFDF5", ink: "#065F46", border: "#065F46" },
  };

const MUTATION_COLOR: Record<MutationKind, { fill: string; ink: string }> = {
  write: { fill: "#2D6B3F", ink: "#FFFFFF" },
  update: { fill: "#1D4ED8", ink: "#FFFFFF" },
  forget: { fill: "#DC2626", ink: "#FFFFFF" },
};

const DIFF_LINE: Record<
  "add" | "remove" | "context",
  { fill: string; ink: string; prefix: string }
> = {
  add: { fill: "#F0FDF4", ink: "#14532D", prefix: "+" },
  remove: { fill: "#FEF2F2", ink: "#7F1D1D", prefix: "−" },
  context: { fill: "var(--color-paper-2)", ink: "var(--color-muted)", prefix: " " },
};

export function EventJournalRow({
  event,
  onRevert,
  reverting,
}: {
  event: MemoryEvent;
  onRevert: () => void;
  reverting: boolean;
}) {
  const { t } = useT();
  const src = SOURCE_COLOR[event.source];
  const mut = MUTATION_COLOR[event.mutation];

  const turnShort =
    event.source_turn_id?.slice(0, 5) ??
    event.target_memory_id.slice(0, 5);

  return (
    <div className="flex flex-col gap-2 border-b border-[var(--color-line)] px-5 py-3.5">
      <div className="flex flex-wrap items-center gap-2">
        <span
          className="inline-flex items-center px-2 py-[2px] font-[var(--font-mono)] text-[10px] font-bold uppercase tracking-[0.05em]"
          style={{ backgroundColor: mut.fill, color: mut.ink }}
        >
          {t(`agent.detail.memory.journal.mutation.${event.mutation}` as const)}
        </span>
        <span
          className="inline-flex items-center border px-2 py-[2px] font-[var(--font-mono)] text-[10px] font-semibold"
          style={{
            backgroundColor: src.fill,
            color: src.ink,
            borderColor: src.border,
          }}
        >
          {t(`agent.detail.memory.journal.source.${event.source}` as const)}
        </span>
        {event.source === "turn" && event.source_turn_id ? (
          <Link
            to={`/?turn=${event.source_turn_id}`}
            className="inline-flex items-center gap-1 text-[var(--color-moss-deep)] hover:text-[var(--color-moss)] hover:underline"
          >
            <MessageSquare className="h-2.5 w-2.5" strokeWidth={1.75} />
            <span className="font-[var(--font-mono)] text-[10px]">
              {t("agent.detail.memory.journal.turnRef", { id: turnShort })}
            </span>
          </Link>
        ) : null}
        <span className="flex-1" />
        <span className="font-[var(--font-mono)] text-[10px] text-[var(--color-muted)]">
          {longDate(event.created_at)} · {clockTime(event.created_at)}
        </span>
      </div>
      <div className="flex flex-col border border-[var(--color-line)]">
        {event.mutation === "update" && event.content_before ? (
          <DiffLine kind="remove" text={event.content_before} />
        ) : null}
        {event.mutation === "forget" && event.content_before ? (
          <DiffLine kind="remove" text={event.content_before} />
        ) : null}
        {event.content_after ? (
          <DiffLine kind="add" text={event.content_after} />
        ) : null}
        {!event.content_before && !event.content_after ? (
          <DiffLine kind="context" text="—" />
        ) : null}
      </div>
      <div className="flex justify-end">
        <button
          type="button"
          onClick={onRevert}
          disabled={reverting}
          className={cn(
            "flex items-center gap-1.5 border border-[var(--color-line)] bg-[var(--color-card)] px-2.5 py-1 text-[12px] text-[var(--color-muted)] transition-colors hover:text-[var(--color-ink)]",
            reverting && "cursor-wait opacity-60",
          )}
        >
          <RotateCcw className="h-3 w-3" strokeWidth={1.75} />
          <span>
            {reverting
              ? t("agent.detail.memory.journal.reverting")
              : t("agent.detail.memory.journal.revert")}
          </span>
        </button>
      </div>
    </div>
  );
}

function DiffLine({
  kind,
  text,
}: {
  kind: "add" | "remove" | "context";
  text: string;
}) {
  const style = DIFF_LINE[kind];
  return (
    <div
      className="flex gap-2 px-2.5 py-1.5"
      style={{ backgroundColor: style.fill, color: style.ink }}
    >
      <span className="font-[var(--font-mono)] text-[12px] font-bold">
        {style.prefix}
      </span>
      <span className="font-[var(--font-mono)] text-[11px] leading-[1.4]">
        {text}
      </span>
    </div>
  );
}
