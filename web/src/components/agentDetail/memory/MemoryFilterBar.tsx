import type { ReactNode } from "react";
import {
  ChevronDown,
  ClockAlert,
  CircleDot,
  Layers,
  Pin,
  Search,
} from "lucide-react";
import { Dropdown } from "../../molecules/Dropdown";
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

type ChipOption<V extends string> = { value: V; label: string };

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

  const kindOptions: ChipOption<MemoryKind | "any">[] = KIND_VALUES.map(
    (v) => ({
      value: v,
      label:
        v === "any"
          ? t("agent.detail.memory.filter.any")
          : t(`agent.detail.memory.kind.${v}` as const),
    }),
  );
  const stateOptions: ChipOption<MemoryState | "any">[] = STATE_VALUES.map(
    (v) => ({
      value: v,
      label:
        v === "any"
          ? t("agent.detail.memory.filter.any")
          : t(`agent.detail.memory.state.${v}` as const),
    }),
  );
  const pinnedOptions: ChipOption<PinnedFilter>[] = PINNED_VALUES.map((v) => ({
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
        <FilterChip
          label={
            filters.kind === "any"
              ? t("agent.detail.memory.filter.kind")
              : t(`agent.detail.memory.kind.${filters.kind}` as const)
          }
          icon={<Layers className="h-3.5 w-3.5" strokeWidth={1.75} />}
          options={kindOptions}
          value={filters.kind}
          active={filters.kind !== "any"}
          ariaLabel={t("agent.detail.memory.filter.kind")}
          onChange={(v) => onChange({ ...filters, kind: v })}
        />
        <FilterChip
          label={
            filters.state === "any"
              ? t("agent.detail.memory.filter.state")
              : t(`agent.detail.memory.state.${filters.state}` as const)
          }
          icon={<CircleDot className="h-3.5 w-3.5" strokeWidth={1.75} />}
          options={stateOptions}
          value={filters.state}
          active={filters.state !== "any"}
          ariaLabel={t("agent.detail.memory.filter.state")}
          onChange={(v) => onChange({ ...filters, state: v })}
        />
        <FilterChip
          label={
            filters.pinned === "yes"
              ? t("agent.detail.memory.filter.pinned.yes")
              : filters.pinned === "no"
                ? t("agent.detail.memory.filter.pinned.no")
                : t("agent.detail.memory.filter.pinned")
          }
          icon={<Pin className="h-3.5 w-3.5" strokeWidth={1.75} />}
          options={pinnedOptions}
          value={filters.pinned}
          active={filters.pinned !== "any"}
          ariaLabel={t("agent.detail.memory.filter.pinned")}
          onChange={(v) => onChange({ ...filters, pinned: v })}
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
            "flex h-7 items-center gap-1.5 border px-2.5 text-[11px] font-semibold transition-colors",
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

function FilterChip<V extends string>({
  label,
  icon,
  options,
  value,
  active,
  ariaLabel,
  onChange,
}: {
  label: string;
  icon: ReactNode;
  options: ChipOption<V>[];
  value: V;
  active: boolean;
  ariaLabel: string;
  onChange: (next: V) => void;
}) {
  return (
    <Dropdown
      placement="bottom-start"
      menuClassName="min-w-[160px] border border-[var(--color-line)] bg-[var(--color-card)] py-1 shadow-md"
      renderTrigger={({ open, toggle }) => (
        <button
          type="button"
          aria-haspopup="listbox"
          aria-expanded={open}
          aria-label={ariaLabel}
          onClick={toggle}
          className={cn(
            "flex h-7 items-center gap-1.5 border px-2.5 text-[12px] outline-none focus-visible:ring-1 focus-visible:ring-[var(--color-ink)]",
            active
              ? "border-[var(--color-moss)] bg-[var(--color-moss-tint)] text-[var(--color-moss-deep)]"
              : "border-[var(--color-line)] bg-[var(--color-card)] text-[var(--color-muted)] hover:text-[var(--color-ink)]",
          )}
        >
          <span
            className={cn(
              "shrink-0",
              active
                ? "text-[var(--color-moss)]"
                : "text-[var(--color-muted-2)]",
            )}
          >
            {icon}
          </span>
          <span>{label}</span>
          <ChevronDown
            className="h-3 w-3 text-[var(--color-muted)]"
            strokeWidth={1.75}
          />
        </button>
      )}
    >
      {({ close }) => (
        <ul role="listbox" aria-label={ariaLabel}>
          {options.map((opt) => {
            const isActive = opt.value === value;
            return (
              <li key={opt.value}>
                <button
                  type="button"
                  role="option"
                  aria-selected={isActive}
                  onClick={() => {
                    close();
                    if (opt.value !== value) onChange(opt.value);
                  }}
                  className={cn(
                    "flex w-full items-center justify-between gap-2 px-3 py-1.5 text-left text-[12.5px] hover:bg-[var(--color-paper-2)]",
                    isActive &&
                      "bg-[var(--color-moss-tint)] text-[var(--color-moss-deep)]",
                  )}
                >
                  <span>{opt.label}</span>
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </Dropdown>
  );
}
