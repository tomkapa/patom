// Prompt diff modal (pencil frame OzwzC, doc/logs_metrics_tab.md §4.5).
//
// Triggered from a prompt-edit marker on the TokenSpendChart or a
// separator row in the TurnsTimeline. Shows the byte-level diff between
// two `agent_prompt_versions` rows side-by-side, with the bottom-right
// "Apply v6" button calling the append-only restore endpoint (a
// successful restore mints a *new* version — v8 byte-identical to v6 —
// rather than rewriting history).
//
// Zero-dependency diff: align lines by index. A v2 of this file can swap
// in a Myers-LCS once we have evidence the alignment actually matters
// for tuning (CLAUDE.md §8 zero-dep bias).

import { useEffect, useMemo, useState } from "react";
import { ArrowRight, Check, ChevronDown, ChevronUp, Copy, Info, Undo2, X } from "lucide-react";
import { Modal, ModalFooter } from "../../molecules/Modal";
import { Button } from "../../atoms/Button";
import { Spinner } from "../../atoms/Spinner";
import { Dropdown } from "../../molecules/Dropdown";
import { MetaRow, MetaCell, MetaDivider } from "../../molecules/MetaRow";
import type { MetricsDeltas, MetricsTotals, PromptVersion } from "../../../types/api";
import { usePromptVersions, useRestorePromptVersion } from "../../../hooks/useAgentLogs";
import { useT } from "../../../i18n";
import { cn } from "../../../lib/utils";
import { formatError } from "../../../lib/errors";

type DiffKind = "context" | "added" | "removed";

type DiffRow = {
  /** 1-indexed line numbers; `null` when the line is absent on this side. */
  leftLine: number | null;
  rightLine: number | null;
  kind: DiffKind;
  /** Raw text on each side; one is empty when `kind` is added/removed. */
  leftText: string;
  rightText: string;
};

/** Align two prompt versions line-by-line and tag each row as
 *  `context | added | removed`. Pure index-based alignment — same length
 *  prefix is contextual, trailing tail on each side is added/removed.
 *  Where lengths overlap but contents differ we render the row as
 *  paired added+removed (one rose on the left, one moss on the right).
 *  This is the cheap "first-cut" diff the slice intentionally ships
 *  (CLAUDE.md §8). */
function diffLines(left: string, right: string): DiffRow[] {
  const leftLines = left.split("\n");
  const rightLines = right.split("\n");
  const out: DiffRow[] = [];
  const max = Math.max(leftLines.length, rightLines.length);
  // §5: the doc's MAX_AGENT_SYSTEM_PROMPT length plus an explicit
  // ceiling keeps this loop bounded for the (degenerate) all-one-line
  // case where the user pasted megabytes.
  const HARD_CAP = 10_000;
  const bound = Math.min(max, HARD_CAP);
  for (let i = 0; i < bound; i += 1) {
    const l = leftLines[i];
    const r = rightLines[i];
    if (l !== undefined && r !== undefined) {
      if (l === r) {
        out.push({
          leftLine: i + 1,
          rightLine: i + 1,
          kind: "context",
          leftText: l,
          rightText: r,
        });
      } else {
        out.push({
          leftLine: i + 1,
          rightLine: i + 1,
          kind: "removed",
          leftText: l,
          rightText: "",
        });
        out.push({
          leftLine: null,
          rightLine: null,
          kind: "added",
          leftText: "",
          rightText: r,
        });
      }
    } else if (l !== undefined) {
      out.push({
        leftLine: i + 1,
        rightLine: null,
        kind: "removed",
        leftText: l,
        rightText: "",
      });
    } else if (r !== undefined) {
      out.push({
        leftLine: null,
        rightLine: i + 1,
        kind: "added",
        leftText: "",
        rightText: r,
      });
    }
  }
  return out;
}

function countChanges(rows: DiffRow[]): { added: number; removed: number } {
  let added = 0;
  let removed = 0;
  for (const r of rows) {
    if (r.kind === "added") added += 1;
    if (r.kind === "removed") removed += 1;
  }
  return { added, removed };
}

export type PromptDiffModalProps = {
  agentId: string;
  /** Version number whose marker was clicked — the modal opens with
   *  this as the "new" side and the immediately preceding version as
   *  the "old" side. */
  targetVersion: number | null;
  open: boolean;
  onClose: () => void;
  /** Page-scoped metrics totals + deltas (current window vs compare).
   *  Drives the "since vN" KPI strip below the meta row. Optional — when
   *  unavailable, the strip renders muted placeholders. */
  metricsTotals?: MetricsTotals;
  metricsDeltas?: MetricsDeltas;
  /** Human-readable window string for the KPI strip caption (e.g.
   *  `"last 24h"`). Already localised by the page. Falls back to the
   *  24h label when omitted. */
  windowLabel?: string;
};

export function PromptDiffModal({
  agentId,
  targetVersion,
  open,
  onClose,
  metricsTotals,
  metricsDeltas,
  windowLabel,
}: PromptDiffModalProps) {
  const { t } = useT();
  const versionsQuery = usePromptVersions(open ? agentId : null);
  const restore = useRestorePromptVersion();
  const effectiveWindowLabel =
    windowLabel ?? t("agent.detail.logs.scope.windowLabel.24h");

  // Toolbar toggles. Local state — none of them mutate server data.
  const [hideUnchanged, setHideUnchanged] = useState(false);
  const [showWhitespace, setShowWhitespace] = useState(false);
  const [wrapLines, setWrapLines] = useState(true);
  const [mode, setMode] = useState<"diff" | "unified">("diff");
  const [changeIdx, setChangeIdx] = useState(0);
  /** User-picked "left" (compared-against) version. Defaults to the
   *  immediately preceding version; resettable when the modal reopens. */
  const [leftPickedVersion, setLeftPickedVersion] = useState<number | null>(null);

  // Reset state when the modal re-opens on a different target. `restore`
  // identity is not stable across renders (react-query mints a new
  // mutation object), so we intentionally exclude it from the dep array
  // — the reset only needs to fire on open/agentId/targetVersion
  // transitions. eslint-disable-next-line keeps the comment honest.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(() => {
    if (open) {
      setHideUnchanged(false);
      setShowWhitespace(false);
      setWrapLines(true);
      setMode("diff");
      setChangeIdx(0);
      setLeftPickedVersion(null);
      restore.reset();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, agentId, targetVersion]);

  const versions = versionsQuery.data?.items ?? [];

  // Pick the two versions to diff. The "right" side is `targetVersion`
  // (the one the user clicked), the "left" side defaults to the
  // immediately preceding version but can be overridden via the version
  // pills in the header or the "Compare against" footer dropdown. If
  // the data isn't loaded yet, both go null.
  const { rightVersion, leftVersion } = useMemo(() => {
    if (!targetVersion || versions.length === 0)
      return { rightVersion: null, leftVersion: null };
    const right =
      versions.find((v) => v.version === targetVersion) ?? versions[0];
    const sorted = [...versions].sort((a, b) => a.version - b.version);
    const idx = sorted.findIndex((v) => v.version === right.version);
    const picked =
      leftPickedVersion != null
        ? sorted.find((v) => v.version === leftPickedVersion)
        : undefined;
    const fallback = idx > 0 ? sorted[idx - 1] : right;
    return { rightVersion: right, leftVersion: picked ?? fallback };
  }, [versions, targetVersion, leftPickedVersion]);

  const rows = useMemo(() => {
    if (!leftVersion || !rightVersion) return [];
    return diffLines(leftVersion.system_prompt, rightVersion.system_prompt);
  }, [leftVersion, rightVersion]);

  const visibleRows = useMemo(
    () => (hideUnchanged ? rows.filter((r) => r.kind !== "context") : rows),
    [rows, hideUnchanged],
  );

  const { added, removed } = useMemo(() => countChanges(rows), [rows]);
  const totalChanges = added + removed;

  const handleApply = async () => {
    if (!leftVersion || leftVersion.version === rightVersion?.version) return;
    try {
      await restore.mutateAsync({
        agentId,
        version: leftVersion.version,
      });
      onClose();
    } catch {
      // Error rendered below the footer; the modal stays open so the
      // user can retry.
    }
  };

  const editedBy = rightVersion?.edited_by ?? "system";
  const editedAt = rightVersion?.created_at ?? "—";

  return (
    <Modal
      open={open}
      onClose={onClose}
      width={1280}
      ariaLabel={t("agent.detail.logs.diff.aria")}
    >
      {/* Top bar */}
      <div className="flex items-center justify-between border-b border-[var(--color-line)] px-7 py-5">
        <div className="flex flex-col gap-1.5">
          <div className="font-[var(--font-mono)] text-[10px] tracking-[0.15em] text-[var(--color-muted)] uppercase">
            {t("agent.detail.logs.diff.eyebrow")}
          </div>
          <div className="flex items-center gap-3">
            <span className="font-[var(--font-display)] text-[24px] font-semibold text-[var(--color-muted)]">
              v{leftVersion?.version ?? "—"}
            </span>
            <ArrowRight className="h-[18px] w-[18px] text-[var(--color-muted)]" />
            <span className="border border-[var(--color-moss)] bg-[var(--color-moss-tint)] px-2.5 py-0.5 font-[var(--font-display)] text-[20px] font-semibold text-[var(--color-moss-deep)]">
              v{rightVersion?.version ?? "—"}
            </span>
            <VersionPills
              versions={versions}
              targetVersion={rightVersion?.version ?? null}
              leftVersion={leftVersion?.version ?? null}
              onPickLeft={setLeftPickedVersion}
            />
          </div>
        </div>
        <div className="flex items-center gap-2">
          <Button
            variant="ghost"
            size="sm"
            onClick={handleApply}
            disabled={
              !leftVersion ||
              leftVersion.version === rightVersion?.version ||
              restore.isPending
            }
            loading={restore.isPending}
            className="border border-[var(--color-line)]"
          >
            <Undo2 className="mr-1.5 h-3 w-3" />
            {t("agent.detail.logs.diff.revert", {
              version: leftVersion?.version ?? "—",
            })}
          </Button>
          <Button
            variant="ghost"
            size="sm"
            onClick={() => {
              if (rightVersion) {
                void navigator.clipboard
                  ?.writeText(rightVersion.system_prompt)
                  .catch(() => {
                    /* no-op — clipboard rejection is non-fatal */
                  });
              }
            }}
            className="border border-[var(--color-line)]"
          >
            <Copy className="mr-1.5 h-3 w-3" />
            {t("agent.detail.logs.diff.copyRight", {
              version: rightVersion?.version ?? "—",
            })}
          </Button>
          <Button variant="primary" size="sm" onClick={onClose}>
            <X className="mr-1.5 h-3 w-3" />
            {t("agent.detail.logs.diff.close")}
          </Button>
        </div>
      </div>

      {/* Meta row */}
      <MetaRow>
        <MetaCell
          label={t("agent.detail.logs.diff.meta.editedBy")}
          value={
            editedBy ?? t("agent.detail.logs.diff.meta.editedBy.fallback")
          }
        />
        <MetaDivider />
        <MetaCell
          label={t("agent.detail.logs.diff.meta.editedAt")}
          value={editedAt}
          mono
        />
        <MetaDivider />
        <MetaCell
          label={t("agent.detail.logs.diff.meta.changes")}
          value={t("agent.detail.logs.diff.meta.changes.value", {
            added,
            removed,
          })}
        />
        <MetaDivider />
        <MetaCell
          label={t("agent.detail.logs.diff.meta.appliedFrom")}
          value={t("agent.detail.logs.diff.meta.appliedFrom.value", {
            version: rightVersion?.version ?? "—",
          })}
        />
      </MetaRow>

      {/* "Since vN (vs vN-1)" KPI strip */}
      <SinceStrip
        rightVersion={rightVersion?.version ?? null}
        leftVersion={leftVersion?.version ?? null}
        windowLabel={effectiveWindowLabel}
        totals={metricsTotals}
        deltas={metricsDeltas}
      />

      {/* Diff toolbar */}
      <div className="flex items-center justify-between border-b border-[var(--color-line)] px-7 py-2.5">
        <div className="flex items-center gap-2">
          {/* Mode toggle */}
          <div className="flex items-center border border-[var(--color-line)]">
            <ToolbarButton
              active={mode === "diff"}
              onClick={() => setMode("diff")}
            >
              {t("agent.detail.logs.diff.toolbar.diff")}
            </ToolbarButton>
            <ToolbarButton
              active={mode === "unified"}
              onClick={() => setMode("unified")}
            >
              {t("agent.detail.logs.diff.toolbar.unified")}
            </ToolbarButton>
          </div>
          <div className="mx-1 h-5 w-px bg-[var(--color-line)]" />
          <ToolbarCheckbox
            checked={hideUnchanged}
            onChange={setHideUnchanged}
            label={t("agent.detail.logs.diff.toolbar.hideUnchanged")}
          />
          <ToolbarCheckbox
            checked={showWhitespace}
            onChange={setShowWhitespace}
            label={t("agent.detail.logs.diff.toolbar.showWhitespace")}
          />
          <ToolbarCheckbox
            checked={wrapLines}
            onChange={setWrapLines}
            label={t("agent.detail.logs.diff.toolbar.wrapLines")}
          />
        </div>
        <div className="flex items-center gap-2">
          <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]">
            {totalChanges > 0
              ? t("agent.detail.logs.diff.toolbar.changesCount", {
                  shown: Math.min(changeIdx + 1, totalChanges),
                  total: totalChanges,
                })
              : t("agent.detail.logs.diff.toolbar.noChanges")}
          </span>
          <button
            type="button"
            aria-label={t("agent.detail.logs.diff.toolbar.prev")}
            disabled={totalChanges === 0}
            onClick={() => setChangeIdx((i) => Math.max(0, i - 1))}
            className="flex h-7 w-7 cursor-pointer items-center justify-center border border-[var(--color-line)] bg-[var(--color-card)] text-[var(--color-muted)] hover:text-[var(--color-ink)] disabled:cursor-not-allowed disabled:opacity-40"
          >
            <ChevronUp className="h-3 w-3" />
          </button>
          <button
            type="button"
            aria-label={t("agent.detail.logs.diff.toolbar.next")}
            disabled={totalChanges === 0}
            onClick={() =>
              setChangeIdx((i) => Math.min(totalChanges - 1, i + 1))
            }
            className="flex h-7 w-7 cursor-pointer items-center justify-center border border-[var(--color-line)] bg-[var(--color-card)] text-[var(--color-muted)] hover:text-[var(--color-ink)] disabled:cursor-not-allowed disabled:opacity-40"
          >
            <ChevronDown className="h-3 w-3" />
          </button>
        </div>
      </div>

      {/* Diff body */}
      <div className="grid grid-cols-2 bg-[var(--color-card)]">
        <DiffSide
          title={`v${leftVersion?.version ?? "—"}`}
          subtitle={t(
            removed === 1
              ? "agent.detail.logs.diff.side.removed.one"
              : "agent.detail.logs.diff.side.removed.many",
            { n: removed },
          )}
          copyAriaLabel={t("agent.detail.logs.diff.side.copy.aria", {
            version: leftVersion?.version ?? "—",
          })}
          rows={visibleRows}
          side="left"
          wrapLines={wrapLines}
          showWhitespace={showWhitespace}
          loading={versionsQuery.isLoading}
          onCopy={
            leftVersion
              ? () => {
                  void navigator.clipboard
                    ?.writeText(leftVersion.system_prompt)
                    .catch(() => {});
                }
              : undefined
          }
        />
        <DiffSide
          title={`v${rightVersion?.version ?? "—"}`}
          subtitle={t(
            added === 1
              ? "agent.detail.logs.diff.side.added.one"
              : "agent.detail.logs.diff.side.added.many",
            { n: added },
          )}
          copyAriaLabel={t("agent.detail.logs.diff.side.copy.aria", {
            version: rightVersion?.version ?? "—",
          })}
          rows={visibleRows}
          side="right"
          wrapLines={wrapLines}
          showWhitespace={showWhitespace}
          loading={versionsQuery.isLoading}
          onCopy={
            rightVersion
              ? () => {
                  void navigator.clipboard
                    ?.writeText(rightVersion.system_prompt)
                    .catch(() => {});
                }
              : undefined
          }
        />
      </div>

      {/* Footer */}
      <ModalFooter
        left={
          <div className="flex items-center gap-3 text-[12px] text-[var(--color-muted)]">
            <Info className="h-3.5 w-3.5" />
            <span>
              {t("agent.detail.logs.diff.footer.info", {
                next: (rightVersion?.version ?? 0) + 1,
                left: leftVersion?.version ?? "—",
                right: rightVersion?.version ?? "—",
              })}
            </span>
          </div>
        }
      >
        <CompareAgainstDropdown
          versions={versions}
          targetVersion={rightVersion?.version ?? null}
          leftVersion={leftVersion?.version ?? null}
          onPickLeft={setLeftPickedVersion}
        />
        {restore.isError ? (
          <span className="text-[11.5px] text-[var(--color-rose)]">
            {formatError(restore.error)}
          </span>
        ) : null}
        <Button variant="ghost" size="sm" onClick={onClose}>
          {t("agent.detail.logs.diff.cancel")}
        </Button>
        <Button
          variant="moss"
          size="sm"
          onClick={handleApply}
          loading={restore.isPending}
          disabled={
            !leftVersion ||
            leftVersion.version === rightVersion?.version ||
            restore.isPending
          }
        >
          {restore.isSuccess ? (
            <Check className="mr-1.5 h-3 w-3" />
          ) : (
            <Undo2 className="mr-1.5 h-3 w-3" />
          )}
          {t("agent.detail.logs.diff.apply", {
            version: leftVersion?.version ?? "—",
          })}
        </Button>
      </ModalFooter>
    </Modal>
  );
}

function VersionPills({
  versions,
  targetVersion,
  leftVersion,
  onPickLeft,
}: {
  versions: PromptVersion[];
  targetVersion: number | null;
  leftVersion: number | null;
  onPickLeft: (v: number) => void;
}) {
  // Show up to three pills: the two versions immediately before
  // `targetVersion`, plus `targetVersion` itself (highlighted, click is
  // a no-op since the right side is fixed).
  if (!targetVersion) return null;
  const sorted = [...versions].sort((a, b) => a.version - b.version);
  const targetIdx = sorted.findIndex((v) => v.version === targetVersion);
  if (targetIdx < 0) return null;
  const slice = sorted.slice(Math.max(0, targetIdx - 2), targetIdx + 1);
  return (
    <div className="flex items-center gap-2 pl-3">
      {slice.map((v) => {
        const isTarget = v.version === targetVersion;
        const isLeft = v.version === leftVersion;
        let pillTone: string;
        if (isTarget) {
          pillTone = "cursor-default border-[var(--color-ink)] bg-[var(--color-ink)] text-[var(--color-card)]";
        } else if (isLeft) {
          pillTone = "cursor-pointer border-[var(--color-moss)] bg-[var(--color-moss-tint)] text-[var(--color-moss-deep)]";
        } else {
          pillTone = "cursor-pointer border-[var(--color-line)] bg-[var(--color-card)] text-[var(--color-muted)] hover:text-[var(--color-ink)]";
        }
        return (
          <button
            key={v.version}
            type="button"
            aria-pressed={isLeft || isTarget}
            disabled={isTarget}
            onClick={() => onPickLeft(v.version)}
            className={cn(
              "border px-2.5 py-1 font-[var(--font-mono)] text-[12px] font-semibold transition-colors duration-150 ease-out",
              pillTone,
            )}
          >
            v{v.version}
          </button>
        );
      })}
    </div>
  );
}

function SinceStrip({
  rightVersion,
  leftVersion,
  windowLabel,
  totals,
  deltas,
}: {
  rightVersion: number | null;
  leftVersion: number | null;
  windowLabel: string;
  totals?: MetricsTotals;
  deltas?: MetricsDeltas;
}) {
  const { t } = useT();
  if (!rightVersion || !leftVersion) return null;
  const tokensDelta = deltas?.tokens ?? null;
  const tokensBase =
    totals && tokensDelta != null ? totals.tokens - tokensDelta : null;
  const latencyDelta = deltas?.latency_p95_ms ?? null;
  const latencyBase =
    totals && latencyDelta != null
      ? totals.latency_p95_ms - latencyDelta
      : null;
  const failuresDelta = deltas?.failure_count ?? null;
  return (
    <div className="flex items-center gap-6 border-b border-[var(--color-line)] bg-[var(--color-card)] px-7 py-4">
      <span className="font-[var(--font-mono)] text-[10px] font-semibold tracking-[0.12em] uppercase text-[var(--color-muted)]">
        {t("agent.detail.logs.diff.since.label", {
          right: rightVersion,
          left: leftVersion,
          window: windowLabel,
        })}
      </span>
      <SinceCell
        label={t("agent.detail.logs.diff.since.cost")}
        delta={tokensDelta}
        base={tokensBase}
        format="pct"
      />
      <SinceCell
        label={t("agent.detail.logs.diff.since.latency")}
        delta={latencyDelta}
        base={latencyBase}
        format="pct"
      />
      <SinceCell
        label={t("agent.detail.logs.diff.since.failures")}
        delta={failuresDelta}
        format="absolute"
      />
    </div>
  );
}

function SinceCell({
  label,
  delta,
  base,
  format,
}: {
  label: string;
  delta: number | null;
  base?: number | null;
  format: "pct" | "absolute";
}) {
  const value = (() => {
    if (delta == null) return "—";
    if (format === "absolute") {
      const sign = delta > 0 ? "+" : "";
      return `${sign}${delta.toLocaleString()}`;
    }
    if (base == null || base === 0) return "—";
    const pct = (delta / base) * 100;
    const sign = pct > 0 ? "+" : "";
    return `${sign}${pct.toFixed(1)}%`;
  })();
  const positive = delta != null && delta > 0;
  let tone: string;
  if (delta == null || delta === 0) {
    tone = "border-[var(--color-line)] bg-[var(--color-card)] text-[var(--color-muted)]";
  } else if (positive) {
    tone = "border-[var(--color-rose)] bg-[var(--color-rose-soft)] text-[var(--color-rose)]";
  } else {
    tone = "border-[var(--color-moss)] bg-[var(--color-moss-tint)] text-[var(--color-moss-deep)]";
  }
  return (
    <div className="flex items-center gap-2.5">
      <span className="font-[var(--font-body)] text-[12px] text-[var(--color-muted)]">
        {label}
      </span>
      <span className="font-[var(--font-mono)] text-[14px] font-semibold text-[var(--color-ink)]">
        {value}
      </span>
      {delta != null && delta !== 0 ? (
        <span
          className={cn(
            "inline-flex items-center gap-1 border px-2 py-0.5 font-[var(--font-mono)] text-[11px]",
            tone,
          )}
        >
          {positive ? "▲" : "▼"} {Math.abs(delta).toLocaleString()}
        </span>
      ) : null}
    </div>
  );
}

function CompareAgainstDropdown({
  versions,
  targetVersion,
  leftVersion,
  onPickLeft,
}: {
  versions: PromptVersion[];
  targetVersion: number | null;
  leftVersion: number | null;
  onPickLeft: (v: number) => void;
}) {
  const { t } = useT();
  const candidates = versions.filter(
    (v) => v.version !== targetVersion,
  );
  return (
    <Dropdown
      placement="bottom-start"
      menuClassName="min-w-[160px] border border-[var(--color-line)] bg-[var(--color-card)] shadow-md"
      renderTrigger={({ open, toggle }) => (
        <button
          type="button"
          onClick={toggle}
          aria-haspopup="listbox"
          aria-expanded={open}
          className="flex cursor-pointer items-center gap-2 border border-[var(--color-line)] bg-[var(--color-card)] px-3 py-1.5 font-[var(--font-body)] text-[12px] text-[var(--color-ink)] outline-none transition-colors duration-150 ease-out hover:bg-[var(--color-paper-2)] focus-visible:ring-1 focus-visible:ring-[var(--color-ink)]"
        >
          <span className="font-[var(--font-mono)] text-[10px] tracking-[0.12em] uppercase text-[var(--color-muted)]">
            {t("agent.detail.logs.diff.compareAgainst.label")}
          </span>
          <span className="font-[var(--font-mono)] text-[12px] font-semibold text-[var(--color-ink)]">
            v{leftVersion ?? "—"}
          </span>
          <ChevronDown className="h-3 w-3 text-[var(--color-muted)]" />
        </button>
      )}
    >
      {({ close }) => (
        <ul
          role="listbox"
          aria-label={t("agent.detail.logs.diff.compareAgainst.aria")}
        >
          {candidates.length === 0 ? (
            <li className="px-3 py-2 text-[12px] text-[var(--color-muted)]">
              {t("agent.detail.logs.diff.compareAgainst.empty")}
            </li>
          ) : (
            candidates.map((v) => {
              const isActive = v.version === leftVersion;
              return (
                <li key={v.version}>
                  <button
                    type="button"
                    role="option"
                    aria-selected={isActive}
                    onClick={() => {
                      close();
                      if (!isActive) onPickLeft(v.version);
                    }}
                    className={cn(
                      "flex w-full cursor-pointer items-center justify-between gap-2 px-3 py-1.5 text-left text-[12.5px] transition-colors duration-100 ease-out hover:bg-[var(--color-paper-2)]",
                      isActive && "bg-[var(--color-moss-tint)] text-[var(--color-moss-deep)]",
                    )}
                  >
                    <span>v{v.version}</span>
                    {isActive ? (
                      <Check className="h-3 w-3 text-[var(--color-moss)]" />
                    ) : null}
                  </button>
                </li>
              );
            })
          )}
        </ul>
      )}
    </Dropdown>
  );
}

function ToolbarButton({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "cursor-pointer px-2.5 py-1.5 font-[var(--font-body)] text-[11px]",
        active
          ? "bg-[var(--color-ink)] text-[var(--color-paper)]"
          : "bg-[var(--color-card)] text-[var(--color-muted)] hover:text-[var(--color-ink)]",
      )}
    >
      {children}
    </button>
  );
}

function ToolbarCheckbox({
  checked,
  onChange,
  label,
}: {
  checked: boolean;
  onChange: (next: boolean) => void;
  label: string;
}) {
  return (
    <label
      className={cn(
        "flex cursor-pointer items-center gap-1.5 border border-[var(--color-line)] px-2.5 py-1.5",
        checked
          ? "border-[var(--color-moss)] bg-[var(--color-moss-tint)]"
          : "bg-[var(--color-card)]",
      )}
    >
      <span
        className={cn(
          "flex h-[11px] w-[11px] items-center justify-center border",
          checked
            ? "border-[var(--color-moss)] bg-[var(--color-moss)]"
            : "border-[var(--color-ink)] bg-[var(--color-card)]",
        )}
        aria-hidden
      >
        {checked ? <Check className="h-2 w-2 text-white" /> : null}
      </span>
      <input
        type="checkbox"
        checked={checked}
        onChange={(e) => onChange(e.target.checked)}
        className="sr-only"
      />
      <span className="font-[var(--font-body)] text-[11px] text-[var(--color-ink)]">
        {label}
      </span>
    </label>
  );
}

function DiffSide({
  title,
  subtitle,
  rows,
  side,
  wrapLines,
  showWhitespace,
  loading,
  onCopy,
  copyAriaLabel,
}: {
  title: string;
  subtitle: string;
  rows: DiffRow[];
  side: "left" | "right";
  wrapLines: boolean;
  showWhitespace: boolean;
  loading: boolean;
  onCopy?: () => void;
  copyAriaLabel?: string;
}) {
  return (
    <div
      className={cn(
        side === "left" && "border-r border-[var(--color-line)]",
      )}
    >
      <div className="flex items-center justify-between border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-4 py-2.5">
        <div className="flex items-baseline gap-3">
          <div className="font-[var(--font-mono)] text-[12px] font-semibold text-[var(--color-ink)]">
            {title}
          </div>
          <div className="font-[var(--font-mono)] text-[10px] text-[var(--color-muted)]">
            {subtitle}
          </div>
        </div>
        {onCopy ? (
          <button
            type="button"
            onClick={onCopy}
            aria-label={copyAriaLabel ?? title}
            className="flex h-6 w-6 cursor-pointer items-center justify-center text-[var(--color-muted)] hover:text-[var(--color-ink)]"
          >
            <Copy className="h-3 w-3" />
          </button>
        ) : null}
      </div>
      <div className="max-h-[60vh] min-h-[320px] overflow-auto">
        {loading ? (
          <div className="flex h-32 items-center justify-center">
            <Spinner />
          </div>
        ) : (
          <ol className="font-[var(--font-mono)] text-[12px] leading-[18px]">
            {rows.map((row, idx) => (
              <DiffLine
                key={`${side}-${idx}`}
                row={row}
                side={side}
                wrapLines={wrapLines}
                showWhitespace={showWhitespace}
              />
            ))}
          </ol>
        )}
      </div>
    </div>
  );
}

function DiffLine({
  row,
  side,
  wrapLines,
  showWhitespace,
}: {
  row: DiffRow;
  side: "left" | "right";
  wrapLines: boolean;
  showWhitespace: boolean;
}) {
  const text = side === "left" ? row.leftText : row.rightText;
  const lineNum = side === "left" ? row.leftLine : row.rightLine;
  const isContext = row.kind === "context";
  const isSidePresent =
    (side === "left" && row.kind !== "added") ||
    (side === "right" && row.kind !== "removed");

  let background = "";
  let marker = "";
  if (isSidePresent && !isContext) {
    if (side === "left") {
      background = "bg-[var(--color-rose-soft)]";
      marker = "-";
    } else {
      background = "bg-[var(--color-moss-tint)]";
      marker = "+";
    }
  } else if (!isSidePresent) {
    background = "bg-[var(--color-paper-2)]";
  }

  const rendered = showWhitespace
    ? text
        .replaceAll(" ", "·")
        .replaceAll("\t", "→   ")
    : text;

  return (
    <li className={cn("grid", background)} style={{ gridTemplateColumns: "48px 22px 1fr" }}>
      <span className="border-r border-[var(--color-line)] bg-[var(--color-card)] py-1.5 text-center text-[10.5px] text-[var(--color-muted)]">
        {lineNum ?? ""}
      </span>
      <span
        aria-hidden
        className={cn(
          "py-1.5 text-center text-[10.5px]",
          marker === "-" && "text-[var(--color-rose)]",
          marker === "+" && "text-[var(--color-moss-deep)]",
          marker === "" && "text-[var(--color-muted-2)]",
        )}
      >
        {marker}
      </span>
      <pre
        className={cn(
          "px-3 py-1.5 text-[12px] text-[var(--color-ink)]",
          wrapLines ? "whitespace-pre-wrap break-words" : "whitespace-pre",
        )}
      >
        {isSidePresent ? rendered : ""}
      </pre>
    </li>
  );
}

export type { PromptVersion };
