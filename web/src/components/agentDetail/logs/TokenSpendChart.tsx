import { useMemo, useRef, useState } from "react";
import { SectionCard } from "../../molecules/SectionCard";
import { KpiTile } from "../../molecules/KpiTile";
import { EmptyState } from "../../molecules/EmptyState";
import { useT } from "../../../i18n";
import type {
  MetricsBucket,
  MetricsDeltas,
  MetricsTotals,
  PromptEditMarker,
} from "../../../types/api";

const PADDING = { top: 24, right: 20, bottom: 28, left: 48 } as const;
const CHART_HEIGHT = 220;
/** Approx pixel width of a marker label (`↑ v99 edited`) at the rendered
 *  font size. Two markers closer than this collide visually, so we merge
 *  them into a single grouped marker. */
const MARKER_LABEL_PX = 64;
/** Stacked-bar colour by kind. Mirrors the `--color-moss*` family already
 *  used elsewhere; reflection lands on the tinted variant so the rare,
 *  internal turn type reads visually subordinate to the user-facing one. */
const KIND_COLOR = {
  normal: "var(--color-moss)",
  reflection: "var(--color-moss-tint)",
  resolution: "var(--color-ink)",
} as const;

/** Zero-dep stacked-bar chart. The pencil frame (`NJOCg`) drives the
 *  layout — bars by time bucket, dashed vertical markers at every
 *  prompt-edit, mono 3-group caption (totals · p50/p95 · failures with
 *  delta) at the bottom. */
export function TokenSpendChart({
  buckets,
  totals,
  deltas,
  promptEdits,
  loading,
  onBucketClick,
  onMarkerClick,
}: {
  buckets: MetricsBucket[];
  totals: MetricsTotals;
  deltas: MetricsDeltas;
  promptEdits: PromptEditMarker[];
  loading: boolean;
  /** Click → filter the timeline below to this bucket. */
  onBucketClick?: (start: string) => void;
  /** Click a `↑ v7 edited` dashed marker → open the prompt-diff modal
   *  for that version (doc/logs_metrics_tab.md §5.2 + slice 3). */
  onMarkerClick?: (version: number) => void;
}) {
  const { t } = useT();
  const [hovered, setHovered] = useState<number | null>(null);
  const [pointer, setPointer] = useState<{ x: number; y: number } | null>(null);
  const chartRef = useRef<HTMLDivElement | null>(null);

  const { width, bars, yMax, xForTime } = useGeometry(buckets);
  const markerGroups = useMemo(
    () => groupMarkers(promptEdits, xForTime),
    [promptEdits, xForTime],
  );

  return (
    <SectionCard
      header={<ChartHero totals={totals} deltas={deltas} />}
    >
      <div className="px-8 py-5">
        {buckets.length === 0 && !loading ? (
          <EmptyState
            title={t("agent.detail.logs.chart.empty.title")}
            description={t("agent.detail.logs.chart.empty.body")}
          />
        ) : (
          <>
            <div ref={chartRef} className="relative">
              <svg
                role="img"
                aria-label={t("agent.detail.logs.chart.aria")}
                viewBox={`0 0 ${width} ${CHART_HEIGHT}`}
                className="w-full"
                preserveAspectRatio="none"
                onMouseMove={(e) => {
                  const host = chartRef.current;
                  if (!host) return;
                  const rect = host.getBoundingClientRect();
                  setPointer({
                    x: e.clientX - rect.left,
                    y: e.clientY - rect.top,
                  });
                }}
                onMouseLeave={() => {
                  setPointer(null);
                  setHovered(null);
                }}
              >
                <YAxis yMax={yMax} />
                {bars.map((bar, i) => (
                  <BucketBar
                    key={bar.bucket.start}
                    bar={bar}
                    yMax={yMax}
                    highlighted={hovered === i}
                    onEnter={() => setHovered(i)}
                    onLeave={() => setHovered(null)}
                    onClick={() => onBucketClick?.(bar.bucket.start)}
                  />
                ))}
                {markerGroups.map((g) => (
                  <EditMarker
                    key={g.versions[0]}
                    x={g.x}
                    versions={g.versions}
                    onClick={
                      onMarkerClick
                        ? () => onMarkerClick(g.versions[g.versions.length - 1]!)
                        : undefined
                    }
                  />
                ))}
              </svg>
              {hovered != null && bars[hovered] && pointer ? (
                <Tooltip bar={bars[hovered]} pointer={pointer} />
              ) : null}
            </div>
            <CaptionRow totals={totals} deltas={deltas} />
          </>
        )}
      </div>
    </SectionCard>
  );
}

type Bar = {
  bucket: MetricsBucket;
  x: number;
  total: number;
  width: number;
};

function useGeometry(buckets: MetricsBucket[]) {
  return useMemo(() => {
    const width = 800;
    const usable = width - PADDING.left - PADDING.right;
    const slotW = buckets.length > 0 ? usable / buckets.length : usable;
    const barW = Math.max(8, slotW * 0.7);
    let yMax = 0;
    const bars: Bar[] = buckets.map((b, i) => {
      const total = b.by_kind.normal + b.by_kind.reflection + b.by_kind.resolution;
      if (total > yMax) yMax = total;
      const x = PADDING.left + slotW * i + (slotW - barW) / 2;
      return { bucket: b, x, total, width: barW };
    });
    if (yMax === 0) yMax = 1;
    const firstStart = buckets[0]?.start;
    const lastStart = buckets[buckets.length - 1]?.start;
    const tFirst = firstStart ? new Date(firstStart).getTime() : 0;
    const tLast = lastStart ? new Date(lastStart).getTime() : 0;
    const tSpan = Math.max(1, tLast - tFirst);

    const xForTime = (iso: string): number | null => {
      if (!firstStart || !lastStart) return null;
      const t = new Date(iso).getTime();
      if (t < tFirst || t > tLast) return null;
      return PADDING.left + ((t - tFirst) / tSpan) * usable;
    };
    return { width, bars, yMax, xForTime };
  }, [buckets]);
}

function YAxis({ yMax }: { yMax: number }) {
  const yChart = CHART_HEIGHT - PADDING.bottom;
  const yTop = PADDING.top;
  const ticks = [0, 0.5, 1] as const;
  return (
    <g>
      {ticks.map((t) => {
        const y = yTop + (yChart - yTop) * (1 - t);
        return (
          <g key={t}>
            <line
              x1={PADDING.left}
              x2={PADDING.left + 800 - PADDING.left - PADDING.right}
              y1={y}
              y2={y}
              stroke="var(--color-line)"
              strokeWidth={0.5}
            />
            <text
              x={PADDING.left - 6}
              y={y + 3}
              textAnchor="end"
              fontSize="9"
              fill="var(--color-muted-foreground)"
              fontFamily="var(--font-mono)"
            >
              {formatTokens(Math.round(yMax * t))}
            </text>
          </g>
        );
      })}
    </g>
  );
}

function BucketBar({
  bar,
  yMax,
  highlighted,
  onEnter,
  onLeave,
  onClick,
}: {
  bar: Bar;
  yMax: number;
  highlighted: boolean;
  onEnter: () => void;
  onLeave: () => void;
  onClick: () => void;
}) {
  const yChart = CHART_HEIGHT - PADDING.bottom;
  const yTop = PADDING.top;
  const scale = (yChart - yTop) / yMax;
  const heights = {
    normal: bar.bucket.by_kind.normal * scale,
    reflection: bar.bucket.by_kind.reflection * scale,
    resolution: bar.bucket.by_kind.resolution * scale,
  };
  // Stack bottom-up: normal, reflection, resolution.
  let cursor = yChart;
  const segs: { key: keyof typeof heights; y: number; h: number }[] = [];
  for (const key of ["normal", "reflection", "resolution"] as const) {
    const h = heights[key];
    if (h <= 0) continue;
    cursor -= h;
    segs.push({ key, y: cursor, h });
  }
  return (
    <g
      onMouseEnter={onEnter}
      onMouseLeave={onLeave}
      onClick={onClick}
      style={{ cursor: "pointer", opacity: highlighted ? 0.85 : 1 }}
    >
      {segs.map((s) => (
        <rect
          key={s.key}
          x={bar.x}
          y={s.y}
          width={bar.width}
          height={s.h}
          fill={KIND_COLOR[s.key]}
        />
      ))}
    </g>
  );
}

function EditMarker({
  x,
  versions,
  onClick,
}: {
  x: number;
  versions: number[];
  onClick?: () => void;
}) {
  const { t } = useT();
  const yTop = PADDING.top - 6;
  const yBottom = CHART_HEIGHT - PADDING.bottom;
  const cursor = onClick ? "cursor-pointer" : undefined;
  const first = versions[0]!;
  const last = versions[versions.length - 1]!;
  const grouped = versions.length > 1;
  const label = grouped
    ? t("agent.detail.logs.chart.marker.labelMulti", {
        first,
        last,
        count: versions.length,
      })
    : t("agent.detail.logs.chart.marker.label", { version: first });
  const aria = onClick
    ? grouped
      ? t("agent.detail.logs.chart.marker.ariaMulti", {
          first,
          last,
          count: versions.length,
        })
      : t("agent.detail.logs.chart.marker.aria", { version: first })
    : undefined;
  return (
    <g
      className={cursor}
      onClick={onClick}
      role={onClick ? "button" : undefined}
      aria-label={aria}
    >
      <line
        x1={x}
        x2={x}
        y1={yTop}
        y2={yBottom}
        stroke="var(--color-ink)"
        strokeWidth={1}
        strokeDasharray="3 3"
      />
      <text
        x={x + 4}
        y={yTop + 4}
        fontSize="10"
        fill="var(--color-ink)"
        fontFamily="var(--font-mono)"
      >
        {label}
      </text>
    </g>
  );
}

type MarkerGroup = { x: number; versions: number[] };

/** Merge prompt-edit markers whose labels would visually collide
 *  (closer than `MARKER_LABEL_PX` apart on the x-axis). Versions
 *  within a group are kept in ascending order so the label can read
 *  `v{first}-v{last}` and clicks resolve to the latest edit. */
function groupMarkers(
  edits: PromptEditMarker[],
  xForTime: (iso: string) => number | null,
): MarkerGroup[] {
  const positioned = edits
    .map((m) => ({ version: m.version, x: xForTime(m.created_at) }))
    .filter((m): m is { version: number; x: number } => m.x != null)
    .sort((a, b) => a.x - b.x);
  const groups: MarkerGroup[] = [];
  for (const m of positioned) {
    const last = groups[groups.length - 1];
    if (last && m.x - last.x < MARKER_LABEL_PX) {
      last.versions.push(m.version);
    } else {
      groups.push({ x: m.x, versions: [m.version] });
    }
  }
  for (const g of groups) g.versions.sort((a, b) => a - b);
  return groups;
}

function ChartHero({
  totals,
  deltas,
}: {
  totals: MetricsTotals;
  deltas: MetricsDeltas;
}) {
  const { t } = useT();
  return (
    <div className="flex flex-wrap items-end justify-between gap-4 border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-8 py-4">
      <div className="flex flex-col gap-1.5">
        <span className="font-[var(--font-mono)] text-[10px] font-medium tracking-[0.15em] uppercase text-[var(--color-muted-foreground)]">
          {t("agent.detail.logs.chart.eyebrow")}
        </span>
        <div className="flex items-baseline gap-3">
          <span className="font-[var(--font-display)] text-[32px] leading-none font-bold text-[var(--color-ink)]">
            {formatTokens(totals.tokens)}
          </span>
          <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)]">
            {t("agent.detail.logs.chart.tokens")}
          </span>
          {renderDeltaPill(deltas.tokens)}
        </div>
      </div>
      <div className="flex items-center gap-4">
        <LegendSwatch
          color="var(--color-moss)"
          label={t("agent.detail.logs.chart.legend.normal")}
        />
        <LegendSwatch
          color="var(--color-moss-tint)"
          label={t("agent.detail.logs.chart.legend.reflection")}
        />
        <LegendSwatch
          color="var(--color-ink)"
          label={t("agent.detail.logs.chart.legend.resolution")}
        />
      </div>
    </div>
  );
}

function LegendSwatch({ color, label }: { color: string; label: string }) {
  return (
    <span className="flex items-center gap-1.5">
      <span
        className="h-2.5 w-2.5"
        style={{ backgroundColor: color }}
        aria-hidden
      />
      <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)]">
        {label}
      </span>
    </span>
  );
}

function renderDeltaPill(d: number | null) {
  if (d == null) return null;
  if (d === 0) return null;
  const positive = d > 0;
  const arrow = positive ? "▲" : "▼";
  const cls = positive
    ? "border-[var(--color-rose)] bg-[var(--color-rose-soft)] text-[var(--color-rose)]"
    : "border-[var(--color-moss)] bg-[var(--color-moss-tint)] text-[var(--color-moss-deep)]";
  return (
    <span
      className={`inline-flex items-center gap-1 border px-2 py-0.5 font-[var(--font-mono)] text-[11px] ${cls}`}
    >
      {arrow} {Math.abs(d).toLocaleString()}
    </span>
  );
}

function CaptionRow({
  totals,
  deltas,
}: {
  totals: MetricsTotals;
  deltas: MetricsDeltas;
}) {
  // Four KPI tiles below the chart — matches pencil frame NJOCg. The
  // values restate the chart's headline metrics so a glance answers the
  // four-question loop (doc §1) without parsing the bars.
  const { t } = useT();
  return (
    <div className="mt-4 grid grid-cols-4 gap-3">
      <KpiTile
        label={t("agent.detail.logs.chart.kpi.tokens")}
        value={formatTokens(totals.tokens)}
        delta={renderDeltaInline(deltas.tokens)}
      />
      <KpiTile
        label={t("agent.detail.logs.chart.kpi.turns")}
        value={totals.turns.toLocaleString()}
      />
      <KpiTile
        label={t("agent.detail.logs.chart.kpi.latency")}
        value={`${formatMs(totals.latency_p50_ms)} · ${formatMs(totals.latency_p95_ms)}`}
        delta={renderDeltaInline(deltas.latency_p95_ms)}
      />
      <KpiTile
        label={t("agent.detail.logs.chart.kpi.failed")}
        value={String(totals.failure_count)}
        delta={renderDeltaInline(deltas.failure_count)}
        tone={totals.failure_count > 0 ? "rose" : undefined}
      />
    </div>
  );
}

function Tooltip({
  bar,
  pointer,
}: {
  bar: Bar;
  pointer: { x: number; y: number };
}) {
  const { t } = useT();
  // Offset from cursor so the tooltip doesn't sit under the mouse and
  // immediately trigger mouseleave. Positioned via inline style because
  // the coordinates change every mousemove — Tailwind classes can't.
  return (
    <div
      className="pointer-events-none absolute z-10 inline-flex flex-col rounded border border-[var(--color-line)] bg-[var(--color-card)] px-2 py-1 font-[var(--font-mono)] text-[11px] text-[var(--color-ink)] shadow"
      style={{
        left: pointer.x + 12,
        top: pointer.y + 12,
      }}
    >
      <span>{new Date(bar.bucket.start).toLocaleString()}</span>
      <span>
        {t("agent.detail.logs.chart.tooltip.total", {
          tokens: formatTokens(bar.total),
        })}
      </span>
      <span>
        {t("agent.detail.logs.chart.tooltip.byKind", {
          normal: bar.bucket.by_kind.normal,
          reflection: bar.bucket.by_kind.reflection,
          resolution: bar.bucket.by_kind.resolution,
        })}
      </span>
    </div>
  );
}

function renderDeltaInline(d: number | null) {
  if (d == null) return null;
  if (d === 0) return <span className="text-[var(--color-muted-foreground)]">▬ 0</span>;
  const arrow = d > 0 ? "▲" : "▼";
  const tone = d > 0 ? "text-[var(--color-rose)]" : "text-[var(--color-moss)]";
  return (
    <span className={tone}>
      {arrow} {Math.abs(d).toLocaleString()}
    </span>
  );
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return String(n);
}

function formatMs(n: number): string {
  if (n >= 1000) return `${(n / 1000).toFixed(1)}s`;
  return `${n}ms`;
}
