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
import type { PromptVersion } from "../../../types/api";
import { usePromptVersions, useRestorePromptVersion } from "../../../hooks/useAgentLogs";
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
};

export function PromptDiffModal({
  agentId,
  targetVersion,
  open,
  onClose,
}: PromptDiffModalProps) {
  const versionsQuery = usePromptVersions(open ? agentId : null);
  const restore = useRestorePromptVersion();

  // Toolbar toggles. Local state — none of them mutate server data.
  const [hideUnchanged, setHideUnchanged] = useState(false);
  const [showWhitespace, setShowWhitespace] = useState(false);
  const [wrapLines, setWrapLines] = useState(true);
  const [mode, setMode] = useState<"diff" | "unified">("diff");
  const [changeIdx, setChangeIdx] = useState(0);

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
      restore.reset();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, agentId, targetVersion]);

  const versions = versionsQuery.data?.items ?? [];

  // Pick the two versions to diff. The "right" side is `targetVersion`
  // (the one the user clicked), the "left" side is the immediately
  // preceding version, falling back to v1 when the clicked marker is v1
  // itself. If the data isn't loaded yet, both go null.
  const { rightVersion, leftVersion } = useMemo(() => {
    if (!targetVersion || versions.length === 0)
      return { rightVersion: null, leftVersion: null };
    const right =
      versions.find((v) => v.version === targetVersion) ?? versions[0];
    const sorted = [...versions].sort((a, b) => a.version - b.version);
    const idx = sorted.findIndex((v) => v.version === right.version);
    const left = idx > 0 ? sorted[idx - 1] : right;
    return { rightVersion: right, leftVersion: left };
  }, [versions, targetVersion]);

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
  const modelChanged =
    leftVersion?.model !== rightVersion?.model &&
    Boolean(leftVersion || rightVersion);

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
      ariaLabel="Prompt version diff"
    >
      {/* Top bar */}
      <div className="flex items-center justify-between border-b border-[var(--color-line)] px-7 py-5">
        <div className="flex flex-col gap-1.5">
          <div className="font-[var(--font-mono)] text-[10px] tracking-[0.15em] text-[var(--color-muted)] uppercase">
            Prompt version compare
          </div>
          <div className="flex items-center gap-3">
            <span className="font-[var(--font-display)] text-[24px] font-semibold text-[var(--color-muted)]">
              v{leftVersion?.version ?? "—"}
            </span>
            <ArrowRight className="h-[18px] w-[18px] text-[var(--color-muted)]" />
            <span className="border border-[var(--color-moss)] bg-[var(--color-moss-tint)] px-2.5 py-0.5 font-[var(--font-display)] text-[20px] font-semibold text-[var(--color-moss-deep)]">
              v{rightVersion?.version ?? "—"}
            </span>
            <span className="font-[var(--font-body)] text-[13px] text-[var(--color-muted)]">
              · Show system prompt
            </span>
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
            Revert to v{leftVersion?.version ?? "—"}
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
            Copy v{rightVersion?.version ?? "—"}
          </Button>
          <Button variant="primary" size="sm" onClick={onClose}>
            <X className="mr-1.5 h-3 w-3" />
            Close
          </Button>
        </div>
      </div>

      {/* Meta row */}
      <div className="flex items-center gap-6 border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-7 py-3.5">
        <MetaCell label="Edited by" value={editedBy ?? "system seed"} />
        <Divider />
        <MetaCell label="Edited at" value={editedAt} mono />
        <Divider />
        <MetaCell
          label="Changes"
          value={`+${added} lines · -${removed} lines · ${
            modelChanged ? "model changed" : "model unchanged"
          }`}
        />
        <Divider />
        <MetaCell
          label="Applied from"
          value={`v${rightVersion?.version ?? "—"} onward`}
        />
      </div>

      {/* Diff toolbar */}
      <div className="flex items-center justify-between border-b border-[var(--color-line)] px-7 py-2.5">
        <div className="flex items-center gap-2">
          {/* Mode toggle */}
          <div className="flex items-center border border-[var(--color-line)]">
            <ToolbarButton
              active={mode === "diff"}
              onClick={() => setMode("diff")}
            >
              Diff
            </ToolbarButton>
            <ToolbarButton
              active={mode === "unified"}
              onClick={() => setMode("unified")}
            >
              Unified
            </ToolbarButton>
          </div>
          <div className="mx-1 h-5 w-px bg-[var(--color-line)]" />
          <ToolbarCheckbox
            checked={hideUnchanged}
            onChange={setHideUnchanged}
            label="Hide unchanged"
          />
          <ToolbarCheckbox
            checked={showWhitespace}
            onChange={setShowWhitespace}
            label="Show whitespace"
          />
          <ToolbarCheckbox
            checked={wrapLines}
            onChange={setWrapLines}
            label="Wrap lines"
          />
        </div>
        <div className="flex items-center gap-2">
          <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]">
            {totalChanges > 0
              ? `${Math.min(changeIdx + 1, totalChanges)} of ${totalChanges} changes`
              : "0 changes"}
          </span>
          <button
            type="button"
            aria-label="Previous change"
            disabled={totalChanges === 0}
            onClick={() => setChangeIdx((i) => Math.max(0, i - 1))}
            className="flex h-7 w-7 cursor-pointer items-center justify-center border border-[var(--color-line)] bg-[var(--color-card)] text-[var(--color-muted)] hover:text-[var(--color-ink)] disabled:cursor-not-allowed disabled:opacity-40"
          >
            <ChevronUp className="h-3 w-3" />
          </button>
          <button
            type="button"
            aria-label="Next change"
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
          subtitle={`${removed} line${removed === 1 ? "" : "s"} removed`}
          rows={visibleRows}
          side="left"
          wrapLines={wrapLines}
          showWhitespace={showWhitespace}
          loading={versionsQuery.isLoading}
        />
        <DiffSide
          title={`v${rightVersion?.version ?? "—"}`}
          subtitle={`${added} line${added === 1 ? "" : "s"} added`}
          rows={visibleRows}
          side="right"
          wrapLines={wrapLines}
          showWhitespace={showWhitespace}
          loading={versionsQuery.isLoading}
        />
      </div>

      {/* Footer */}
      <ModalFooter
        left={
          <div className="flex items-center gap-3 text-[12px] text-[var(--color-muted)]">
            <Info className="h-3.5 w-3.5" />
            <span>
              Reverting overwrites the active prompt. v
              {rightVersion?.version ?? "—"} history is preserved.
            </span>
          </div>
        }
      >
        {restore.isError ? (
          <span className="text-[11.5px] text-[var(--color-rose)]">
            {formatError(restore.error)}
          </span>
        ) : null}
        <Button variant="ghost" size="sm" onClick={onClose}>
          Cancel
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
          Apply v{leftVersion?.version ?? "—"}
        </Button>
      </ModalFooter>
    </Modal>
  );
}

function MetaCell({
  label,
  value,
  mono,
}: {
  label: string;
  value: string;
  mono?: boolean;
}) {
  return (
    <div className="flex flex-col gap-0.5">
      <div className="font-[var(--font-mono)] text-[10px] tracking-[0.12em] text-[var(--color-muted)] uppercase">
        {label}
      </div>
      <div
        className={cn(
          "text-[12px] font-medium text-[var(--color-ink)]",
          mono && "font-[var(--font-mono)]",
        )}
      >
        {value}
      </div>
    </div>
  );
}

function Divider() {
  return <div className="h-[30px] w-px bg-[var(--color-line)]" />;
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
}: {
  title: string;
  subtitle: string;
  rows: DiffRow[];
  side: "left" | "right";
  wrapLines: boolean;
  showWhitespace: boolean;
  loading: boolean;
}) {
  return (
    <div
      className={cn(
        side === "left" && "border-r border-[var(--color-line)]",
      )}
    >
      <div className="flex items-center justify-between border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-4 py-2.5">
        <div className="font-[var(--font-mono)] text-[12px] font-semibold text-[var(--color-ink)]">
          {title}
        </div>
        <div className="font-[var(--font-mono)] text-[10px] text-[var(--color-muted)]">
          {subtitle}
        </div>
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
