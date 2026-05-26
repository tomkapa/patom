import { ScrollText } from "lucide-react";
import { Spinner } from "../../atoms/Spinner";
import { Button } from "../../atoms/Button";
import { EmptyState } from "../../molecules/EmptyState";
import { TogglePill, type TogglePillTone } from "../../molecules/TogglePill";
import { formatError } from "../../../lib/errors";
import { useT } from "../../../i18n";
import type {
  MemoryEvent,
  MemoryEventsFilter,
  MutationKind,
  MutationSource,
} from "../../../types/api";
import { EventJournalRow } from "./EventJournalRow";

const SOURCES: readonly MutationSource[] = ["turn", "operator", "librarian"];
const MUTATIONS: readonly MutationKind[] = ["write", "update", "forget"];

const SOURCE_CHIP: Record<MutationSource, TogglePillTone> = {
  turn: { fill: "#EFF6FF", ink: "#1D4ED8", border: "#1D4ED8" },
  operator: { fill: "#F5F3FF", ink: "#7C3AED", border: "#7C3AED" },
  librarian: { fill: "#ECFDF5", ink: "#065F46", border: "#065F46" },
};

const MUTATION_CHIP: Record<MutationKind, TogglePillTone> = {
  write: { fill: "#E8F0EA", ink: "#2D6B3F", border: "#2D6B3F" },
  update: { fill: "#EFF6FF", ink: "#1D4ED8", border: "#1D4ED8" },
  forget: { fill: "#FEF2F2", ink: "#DC2626", border: "#DC2626" },
};

export function EventJournalPanel({
  events,
  loading,
  error,
  filter,
  onFilterChange,
  onRetry,
  onRevert,
  revertingId,
}: {
  events: MemoryEvent[];
  loading: boolean;
  /** Surfaces a fetch failure as a retryable error state instead of
   *  letting the panel collapse into the "no events" empty state. */
  error: unknown;
  filter: MemoryEventsFilter;
  onFilterChange: (next: MemoryEventsFilter) => void;
  onRetry: () => void;
  onRevert: (eventId: string) => void;
  revertingId: string | null;
}) {
  const { t } = useT();

  return (
    <section
      className="flex h-full min-h-0 flex-1 flex-col bg-[var(--color-card)]"
      aria-label={t("agent.detail.memory.journal.title")}
    >
      <header className="flex flex-col gap-2.5 border-b border-[var(--color-line)] px-5 py-3.5">
        <div className="flex items-center gap-2">
          <ScrollText
            className="h-4 w-4 text-[var(--color-ink)]"
            strokeWidth={1.75}
          />
          <h2 className="font-[var(--font-display)] text-[16px] font-bold text-[var(--color-ink)]">
            {t("agent.detail.memory.journal.title")}
          </h2>
        </div>
        <LegendRow
          label={t("agent.detail.memory.journal.source")}
          values={SOURCES}
          current={filter.source ?? null}
          renderLabel={(v) =>
            t(`agent.detail.memory.journal.source.${v}` as const)
          }
          style={SOURCE_CHIP}
          onSelect={(v) =>
            onFilterChange({
              ...filter,
              source: filter.source === v ? undefined : v,
            })
          }
        />
        <LegendRow
          label={t("agent.detail.memory.journal.mutation")}
          values={MUTATIONS}
          current={filter.mutation ?? null}
          renderLabel={(v) =>
            t(`agent.detail.memory.journal.mutation.${v}` as const)
          }
          style={MUTATION_CHIP}
          onSelect={(v) =>
            onFilterChange({
              ...filter,
              mutation: filter.mutation === v ? undefined : v,
            })
          }
        />
      </header>
      <div className="min-h-0 flex-1 overflow-y-auto">
        {loading && events.length === 0 ? (
          <div className="flex items-center justify-center p-6 text-[var(--color-muted)]">
            <Spinner size={14} />
          </div>
        ) : error ? (
          <div className="p-6">
            <EmptyState
              title={t("agent.detail.memory.loadError.title")}
              description={formatError(error)}
              action={
                <Button variant="primary" onClick={onRetry}>
                  {t("agent.detail.memory.loadError.cta")}
                </Button>
              }
            />
          </div>
        ) : events.length === 0 ? (
          <div className="px-5 py-6 text-[13px] text-[var(--color-muted)]">
            {t("agent.detail.memory.journal.empty")}
          </div>
        ) : (
          events.map((e) => (
            <EventJournalRow
              key={e.id}
              event={e}
              onRevert={() => onRevert(e.id)}
              reverting={revertingId === e.id}
            />
          ))
        )}
      </div>
    </section>
  );
}

function LegendRow<V extends string>({
  label,
  values,
  current,
  renderLabel,
  style,
  onSelect,
}: {
  label: string;
  values: readonly V[];
  current: V | null;
  renderLabel: (v: V) => string;
  style: Record<V, TogglePillTone>;
  onSelect: (v: V) => void;
}) {
  return (
    <div className="flex flex-wrap items-center gap-1.5">
      <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]">
        {label}
      </span>
      {values.map((v) => {
        const isActive = current === v;
        return (
          <TogglePill
            key={v}
            active={isActive}
            dimmed={current !== null && !isActive}
            tone={style[v]}
            onClick={() => onSelect(v)}
          >
            {renderLabel(v)}
          </TogglePill>
        );
      })}
    </div>
  );
}
