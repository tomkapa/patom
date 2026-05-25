import type { MemoryKind, MemoryRow, MemoryState } from "../../../types/api";

/** Mirrors `MAX_MEMORIES_PER_AGENT` in `src/memory/limits.rs`. The
 *  quota strip's denominator must match the backend cap or the
 *  "eviction starts at 90%" caption lies. */
export const MAX_MEMORIES_PER_AGENT = 1024;

/** Mirrors `MATURATION_WINDOW` in `src/memory/limits.rs` — 7 days. A
 *  memory older than this that is still `tentative` is what the
 *  filter chip flags as "aging". */
const MATURATION_WINDOW_MS = 7 * 24 * 60 * 60 * 1000;

/** Canonical kind order for the grouped list. Identity (`self`) first
 *  because it's the agent's most stable layer; `open` last because
 *  those are known unknowns and tend to be noise. */
export const KIND_ORDER: readonly MemoryKind[] = [
  "self",
  "other",
  "collaborator",
  "procedure",
  "open",
] as const;

export type PinnedFilter = "any" | "yes" | "no";

export type MemoryFilters = {
  q: string;
  kind: MemoryKind | "any";
  state: MemoryState | "any";
  pinned: PinnedFilter;
  aging: boolean;
};

export const EMPTY_FILTERS: MemoryFilters = {
  q: "",
  kind: "any",
  state: "any",
  pinned: "any",
  aging: false,
};

/** A `tentative` row OR a row whose state is still `tentative` and
 *  whose creation predates the maturation window — i.e. one the
 *  librarian should have promoted by now. The check is intentionally
 *  client-side: the backend does not surface an "aging" flag. */
export function isAging(row: MemoryRow, nowMs: number): boolean {
  if (row.state !== "tentative") return false;
  const createdMs = Date.parse(row.created_at);
  if (Number.isNaN(createdMs)) return true;
  return nowMs - createdMs >= MATURATION_WINDOW_MS;
}

export function isAnyTentative(rows: MemoryRow[]): boolean {
  return rows.some((r) => r.state === "tentative");
}

export function applyFilters(
  rows: MemoryRow[],
  filters: MemoryFilters,
  nowMs: number,
): MemoryRow[] {
  const needle = filters.q.trim().toLowerCase();
  return rows.filter((r) => {
    if (filters.kind !== "any" && r.kind !== filters.kind) return false;
    if (filters.state !== "any" && r.state !== filters.state) return false;
    if (filters.pinned === "yes" && !r.pinned) return false;
    if (filters.pinned === "no" && r.pinned) return false;
    if (filters.aging && !(r.state === "tentative" || isAging(r, nowMs)))
      return false;
    if (needle && !r.content.toLowerCase().includes(needle)) return false;
    return true;
  });
}

/** Group rows by kind in `KIND_ORDER`, dropping empty buckets. The
 *  caller renders one header per non-empty group. */
export function groupByKind(rows: MemoryRow[]): {
  kind: MemoryKind;
  rows: MemoryRow[];
}[] {
  const buckets = new Map<MemoryKind, MemoryRow[]>();
  for (const r of rows) {
    const bucket = buckets.get(r.kind) ?? [];
    bucket.push(r);
    buckets.set(r.kind, bucket);
  }
  return KIND_ORDER.filter((k) => (buckets.get(k)?.length ?? 0) > 0).map(
    (k) => ({ kind: k, rows: buckets.get(k) ?? [] }),
  );
}

export function quotaPercent(used: number): number {
  return Math.round((used / MAX_MEMORIES_PER_AGENT) * 100);
}
