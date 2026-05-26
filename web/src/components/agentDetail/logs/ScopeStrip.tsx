import { Select } from "../../molecules/Select";
import type {
  LogsCompareMode,
  LogsKindFilter,
  LogsTimeRange,
} from "../../../types/api";

/** Three controls + a "last updated" caption. Mirrors the strip in pencil
 *  frame `NJOCg`. Stateless: every selector is a controlled input that
 *  bubbles back to the page so the URL / react-query keys stay the source
 *  of truth. */
export function ScopeStrip({
  range,
  onRangeChange,
  kind,
  onKindChange,
  compare,
  onCompareChange,
  updatedAt,
}: {
  range: LogsTimeRange;
  onRangeChange: (next: LogsTimeRange) => void;
  kind: LogsKindFilter;
  onKindChange: (next: LogsKindFilter) => void;
  compare: LogsCompareMode;
  onCompareChange: (next: LogsCompareMode) => void;
  /** Wall-clock time the last successful fetch returned. Rendered as
   *  "updated 12s ago" mono caption — refreshed by the parent so the
   *  ticker doesn't re-render every neighbour. */
  updatedAt: Date | null;
}) {
  return (
    <div className="flex flex-wrap items-center gap-3 border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-8 py-3">
      <Select<LogsTimeRange>
        variant="filter"
        value={range}
        ariaLabel="Time range"
        triggerLabel={LABELS_RANGE[range]}
        onChange={onRangeChange}
        active
        options={[
          { value: "1h", label: "Last 1h" },
          { value: "24h", label: "Last 24h" },
          { value: "7d", label: "Last 7d" },
          { value: "30d", label: "Last 30d" },
        ]}
      />
      <Select<LogsKindFilter>
        variant="filter"
        value={kind}
        ariaLabel="Kind filter"
        triggerLabel={`Kind: ${LABELS_KIND[kind]}`}
        onChange={onKindChange}
        active={kind !== "all"}
        options={[
          { value: "all", label: "All" },
          { value: "normal", label: "Normal" },
          { value: "reflection", label: "Reflection" },
          { value: "resolution", label: "Resolution" },
        ]}
      />
      <Select<LogsCompareMode>
        variant="filter"
        value={compare}
        ariaLabel="Compare window"
        triggerLabel={`Compare: ${LABELS_COMPARE[compare]}`}
        onChange={onCompareChange}
        active={compare !== "none"}
        options={[
          { value: "prev_window", label: "Previous window" },
          { value: "none", label: "None" },
        ]}
      />
      <div className="ml-auto font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]">
        {updatedAt
          ? `updated ${secondsAgo(updatedAt)}s ago`
          : "updating…"}
      </div>
    </div>
  );
}

const LABELS_RANGE: Record<LogsTimeRange, string> = {
  "1h": "Last 1h",
  "24h": "Last 24h",
  "7d": "Last 7d",
  "30d": "Last 30d",
};

const LABELS_KIND: Record<LogsKindFilter, string> = {
  all: "all",
  normal: "normal",
  reflection: "reflection",
  resolution: "resolution",
};

const LABELS_COMPARE: Record<LogsCompareMode, string> = {
  prev_window: "prev window",
  none: "none",
};

function secondsAgo(t: Date): number {
  const ms = Date.now() - t.getTime();
  if (ms < 0) return 0;
  return Math.floor(ms / 1000);
}
