import {
  ClockAlert,
  CircleDot,
  Layers,
  Pin,
  Search,
} from "lucide-react";
import { Select, type SelectOption } from "../../molecules/Select";
import { cn } from "../../../lib/utils";
import { useT } from "../../../i18n";
import type { MemoryKind, MemoryState } from "../../../types/api";
import {
  type MemoryFilters,
  type PinnedFilter,
} from "./memoryFilterState";

const KIND_VALUES: readonly (MemoryKind | "any")[] = [
  "any",
  "self",
  "other",
  "collaborator",
  "procedure",
  "open",
] as const;

const STATE_VALUES: readonly (MemoryState | "any")[] = [
  "any",
  "core",
  "validated",
  "held",
  "tentative",
] as const;

const PINNED_VALUES: readonly PinnedFilter[] = ["any", "yes", "no"] as const;

export function MemoryFilterBar({
  filters,
  onChange,
  agingAvailable,
}: {
  filters: MemoryFilters;
  onChange: (next: MemoryFilters) => void;
  /** When false, no row currently qualifies as aging — disable the
   *  chip so the operator doesn't toggle into an empty list. */
  agingAvailable: boolean;
}) {
  const { t } = useT();

  const kindOptions: SelectOption<MemoryKind | "any">[] = KIND_VALUES.map(
    (v) => ({
      value: v,
      label:
        v === "any"
          ? t("agent.detail.memory.filter.any")
          : t(`agent.detail.memory.kind.${v}` as const),
    }),
  );
  const stateOptions: SelectOption<MemoryState | "any">[] = STATE_VALUES.map(
    (v) => ({
      value: v,
      label:
        v === "any"
          ? t("agent.detail.memory.filter.any")
          : t(`agent.detail.memory.state.${v}` as const),
    }),
  );
  const pinnedOptions: SelectOption<PinnedFilter>[] = PINNED_VALUES.map((v) => ({
    value: v,
    label:
      v === "any"
        ? t("agent.detail.memory.filter.any")
        : v === "yes"
          ? t("agent.detail.memory.filter.pinned.yes")
          : t("agent.detail.memory.filter.pinned.no"),
  }));

  return (
    <div className="flex flex-col gap-2 border-b border-[var(--color-line)] px-5 py-3">
      <label className="flex h-8 items-center gap-1.5 border border-[var(--color-line)] bg-[var(--color-card)] px-2.5">
        <Search
          className="h-3.5 w-3.5 text-[var(--color-muted)]"
          strokeWidth={1.75}
        />
        <input
          type="search"
          value={filters.q}
          onChange={(e) => onChange({ ...filters, q: e.target.value })}
          placeholder={t("agent.detail.memory.search")}
          aria-label={t("agent.detail.memory.search")}
          className="min-w-0 flex-1 bg-transparent text-[13px] text-[var(--color-ink)] outline-none placeholder:text-[var(--color-muted)]"
        />
      </label>
      <div className="flex flex-wrap items-center gap-1.5">
        <Select<MemoryKind | "any">
          variant="filter"
          ariaLabel={t("agent.detail.memory.filter.kind")}
          icon={<Layers className="h-3.5 w-3.5" strokeWidth={1.75} />}
          triggerLabel={
            filters.kind === "any"
              ? t("agent.detail.memory.filter.kind")
              : t(`agent.detail.memory.kind.${filters.kind}` as const)
          }
          active={filters.kind !== "any"}
          value={filters.kind}
          options={kindOptions}
          onChange={(kind) => onChange({ ...filters, kind })}
        />
        <Select<MemoryState | "any">
          variant="filter"
          ariaLabel={t("agent.detail.memory.filter.state")}
          icon={<CircleDot className="h-3.5 w-3.5" strokeWidth={1.75} />}
          triggerLabel={
            filters.state === "any"
              ? t("agent.detail.memory.filter.state")
              : t(`agent.detail.memory.state.${filters.state}` as const)
          }
          active={filters.state !== "any"}
          value={filters.state}
          options={stateOptions}
          onChange={(state) => onChange({ ...filters, state })}
        />
        <Select<PinnedFilter>
          variant="filter"
          ariaLabel={t("agent.detail.memory.filter.pinned")}
          icon={<Pin className="h-3.5 w-3.5" strokeWidth={1.75} />}
          triggerLabel={
            filters.pinned === "yes"
              ? t("agent.detail.memory.filter.pinned.yes")
              : filters.pinned === "no"
                ? t("agent.detail.memory.filter.pinned.no")
                : t("agent.detail.memory.filter.pinned")
          }
          active={filters.pinned !== "any"}
          value={filters.pinned}
          options={pinnedOptions}
          onChange={(pinned) => onChange({ ...filters, pinned })}
        />
        <div className="flex-1" />
        <button
          type="button"
          role="switch"
          aria-checked={filters.aging}
          aria-label={t("agent.detail.memory.filter.aging")}
          disabled={!agingAvailable && !filters.aging}
          onClick={() => onChange({ ...filters, aging: !filters.aging })}
          className={cn(
            "flex h-7 cursor-pointer items-center gap-1.5 border px-2.5 text-[11px] font-semibold transition-colors duration-150 ease-out",
            filters.aging
              ? "border-[#F59E0B] bg-[#FEF3C7] text-[#92400E]"
              : "border-[#F59E0B66] bg-[#FEF3C766] text-[#92400EAA]",
            !agingAvailable && !filters.aging && "cursor-not-allowed opacity-50",
          )}
        >
          <ClockAlert className="h-3 w-3" strokeWidth={1.75} />
          <span>{t("agent.detail.memory.filter.aging")}</span>
        </button>
      </div>
    </div>
  );
}
