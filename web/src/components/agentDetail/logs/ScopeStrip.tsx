import type { ReactNode } from "react";
import { ChevronDown } from "lucide-react";
import { Dropdown } from "../../molecules/Dropdown";
import { useT } from "../../../i18n";
import { cn } from "../../../lib/utils";
import type {
  LogsCompareMode,
  LogsKindFilter,
  LogsTimeRange,
} from "../../../types/api";

/** Filter strip on the Logs & metrics page (pencil frame `POiQ7`).
 *  Three label-above-value pills (RANGE / KIND / COMPARE), a divider,
 *  and a read-only `BUCKET` chip derived from the active range. Right
 *  side carries the live-refresh indicator. Stateless — every change
 *  bubbles back to the page so react-query keys stay the source of
 *  truth. */
export function ScopeStrip({
  range,
  onRangeChange,
  kind,
  onKindChange,
  compare,
  onCompareChange,
  updatedAt,
  bucketLabel,
}: {
  range: LogsTimeRange;
  onRangeChange: (next: LogsTimeRange) => void;
  kind: LogsKindFilter;
  onKindChange: (next: LogsKindFilter) => void;
  compare: LogsCompareMode;
  onCompareChange: (next: LogsCompareMode) => void;
  updatedAt: Date | null;
  /** Bucket size returned by the metrics endpoint (e.g. `"3h"`). Falls
   *  back to a static map keyed on `range` until the first fetch
   *  succeeds so the chip never goes blank. */
  bucketLabel?: string;
}) {
  const { t } = useT();
  const rangeLabel = t(RANGE_KEYS[range]);
  const kindLabel = t(KIND_KEYS[kind]);
  const compareLabel = t(COMPARE_KEYS[compare]);

  return (
    <div className="flex flex-wrap items-center gap-2.5 border-b border-[var(--color-line)] bg-[var(--color-card)] px-8 py-3.5">
      <FilterPill
        label={t("agent.detail.logs.scope.range")}
        value={rangeLabel}
        ariaLabel={t("agent.detail.logs.scope.aria.range")}
        active={range !== "24h"}
        options={[
          { value: "1h", label: t("agent.detail.logs.scope.range.1h") },
          { value: "24h", label: t("agent.detail.logs.scope.range.24h") },
          { value: "7d", label: t("agent.detail.logs.scope.range.7d") },
          { value: "30d", label: t("agent.detail.logs.scope.range.30d") },
        ]}
        onChange={(v) => onRangeChange(v as LogsTimeRange)}
        current={range}
      />
      <FilterPill
        label={t("agent.detail.logs.scope.kind")}
        value={kindLabel}
        ariaLabel={t("agent.detail.logs.scope.aria.kind")}
        active={kind !== "all"}
        options={[
          { value: "all", label: t("agent.detail.logs.scope.kind.all") },
          { value: "normal", label: t("agent.detail.logs.scope.kind.normal") },
          {
            value: "reflection",
            label: t("agent.detail.logs.scope.kind.reflection"),
          },
          {
            value: "resolution",
            label: t("agent.detail.logs.scope.kind.resolution"),
          },
          {
            value: "compaction",
            label: t("agent.detail.logs.scope.kind.compaction"),
          },
        ]}
        onChange={(v) => onKindChange(v as LogsKindFilter)}
        current={kind}
      />
      <FilterPill
        label={t("agent.detail.logs.scope.compare")}
        value={compareLabel}
        ariaLabel={t("agent.detail.logs.scope.aria.compare")}
        active={compare !== "none"}
        options={[
          {
            value: "prev_window",
            label: t("agent.detail.logs.scope.compare.prev"),
          },
          { value: "none", label: t("agent.detail.logs.scope.compare.none") },
        ]}
        onChange={(v) => onCompareChange(v as LogsCompareMode)}
        current={compare}
      />
      <div className="mx-1 h-7 w-px bg-[var(--color-line)]" aria-hidden />
      <BucketChip
        label={t("agent.detail.logs.scope.bucket")}
        value={bucketLabel ?? FALLBACK_BUCKET[range]}
      />
      <div className="ml-auto flex items-center gap-2">
        <span
          className="h-1.5 w-1.5 rounded-full bg-[var(--color-moss)]"
          aria-hidden
        />
        <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)]">
          {updatedAt
            ? t("agent.detail.logs.scope.updated", { n: secondsAgo(updatedAt) })
            : t("agent.detail.logs.scope.updating")}
        </span>
      </div>
    </div>
  );
}

type PillOption = { value: string; label: ReactNode };

function FilterPill({
  label,
  value,
  ariaLabel,
  active,
  options,
  current,
  onChange,
}: {
  label: string;
  value: string;
  ariaLabel: string;
  active: boolean;
  options: PillOption[];
  current: string;
  onChange: (next: string) => void;
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
            "flex cursor-pointer items-center gap-2 border px-3 py-1.5 text-left outline-none transition-colors duration-150 ease-out focus-visible:ring-1 focus-visible:ring-[var(--color-ink)]",
            active
              ? "border-[var(--color-moss)] bg-[var(--color-moss-tint)]"
              : "border-[var(--color-line)] bg-[var(--color-card)] hover:bg-[var(--color-paper-2)]",
          )}
        >
          <span className="flex flex-col items-start gap-0.5 leading-tight">
            <span className="font-[var(--font-mono)] text-[9px] font-medium tracking-[0.12em] uppercase text-[var(--color-muted-foreground)]">
              {label}
            </span>
            <span
              className={cn(
                "font-[var(--font-body)] text-[13px] font-medium",
                active ? "text-[var(--color-moss-deep)]" : "text-[var(--color-ink)]",
              )}
            >
              {value}
            </span>
          </span>
          <ChevronDown
            className={cn(
              "h-3 w-3",
              active ? "text-[var(--color-moss)]" : "text-[var(--color-muted-foreground)]",
            )}
            strokeWidth={1.75}
          />
        </button>
      )}
    >
      {({ close }) => (
        <ul role="listbox" aria-label={ariaLabel}>
          {options.map((opt) => {
            const isActive = opt.value === current;
            return (
              <li key={opt.value}>
                <button
                  type="button"
                  role="option"
                  aria-selected={isActive}
                  onClick={() => {
                    close();
                    if (!isActive) onChange(opt.value);
                  }}
                  className={cn(
                    "flex w-full cursor-pointer items-center justify-between gap-2 px-3 py-1.5 text-left text-[12.5px] transition-colors duration-100 ease-out hover:bg-[var(--color-paper-2)]",
                    isActive && "bg-[var(--color-moss-tint)] text-[var(--color-moss-deep)]",
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

function BucketChip({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center gap-2 border border-[var(--color-line)] bg-[var(--color-card)] px-3 py-1.5">
      <span className="font-[var(--font-mono)] text-[9px] font-medium tracking-[0.12em] uppercase text-[var(--color-muted-foreground)]">
        {label}
      </span>
      <span className="font-[var(--font-body)] text-[13px] font-medium text-[var(--color-ink)]">
        {value}
      </span>
    </div>
  );
}

const RANGE_KEYS = {
  "1h": "agent.detail.logs.scope.range.1h",
  "24h": "agent.detail.logs.scope.range.24h",
  "7d": "agent.detail.logs.scope.range.7d",
  "30d": "agent.detail.logs.scope.range.30d",
} as const;

const KIND_KEYS = {
  all: "agent.detail.logs.scope.kind.all",
  normal: "agent.detail.logs.scope.kind.normal",
  reflection: "agent.detail.logs.scope.kind.reflection",
  resolution: "agent.detail.logs.scope.kind.resolution",
  compaction: "agent.detail.logs.scope.kind.compaction",
} as const;

const COMPARE_KEYS = {
  prev_window: "agent.detail.logs.scope.compare.prev",
  none: "agent.detail.logs.scope.compare.none",
} as const;

const FALLBACK_BUCKET: Record<LogsTimeRange, string> = {
  "1h": "5m",
  "24h": "1h",
  "7d": "6h",
  "30d": "1d",
};

function secondsAgo(t: Date): number {
  const ms = Date.now() - t.getTime();
  if (ms < 0) return 0;
  return Math.floor(ms / 1000);
}
