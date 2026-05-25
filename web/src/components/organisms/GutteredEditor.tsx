import { useEffect, useMemo, useRef } from "react";
import type { ReactNode } from "react";
import { cn } from "../../lib/utils";

/** Generic dark-header + gutter-numbered textarea panel. The footer is
 *  free-form so callers can render stats (lines / tokens / chars) or
 *  per-domain affordances without baking them into the editor. */
export function GutteredEditor({
  value,
  onChange,
  header,
  footer,
  placeholder,
  readOnly,
  ariaLabel,
  className,
}: {
  value: string;
  onChange: (next: string) => void;
  header: { icon?: ReactNode; title: ReactNode; right?: ReactNode };
  footer?: ReactNode;
  placeholder?: string;
  readOnly?: boolean;
  ariaLabel: string;
  className?: string;
}) {
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const gutterRef = useRef<HTMLDivElement>(null);

  const lineCount = useMemo(
    () => Math.max(1, value.split("\n").length),
    [value],
  );
  // Render at least enough numbers to fill the visible gutter even when
  // the prompt is short — the design shows numbers continuing into empty
  // space.
  const gutterContent = useMemo(() => {
    const lines: string[] = [];
    for (let i = 1; i <= Math.max(lineCount, 26); i++) lines.push(String(i));
    return lines.join("\n");
  }, [lineCount]);

  // Keep gutter scroll in lockstep with the textarea.
  useEffect(() => {
    const ta = textareaRef.current;
    const g = gutterRef.current;
    if (!ta || !g) return;
    const sync = () => {
      g.scrollTop = ta.scrollTop;
    };
    ta.addEventListener("scroll", sync, { passive: true });
    return () => ta.removeEventListener("scroll", sync);
  }, []);

  return (
    <div
      className={cn(
        "flex h-full min-h-0 flex-col border border-[var(--color-line-strong)] bg-[var(--color-card)]",
        className,
      )}
    >
      <div className="flex h-10 shrink-0 items-center justify-between gap-3 bg-[var(--color-ink)] px-4">
        <div className="flex items-center gap-2 text-[#FFFFFFCC]">
          {header.icon ? (
            <span className="flex h-3.5 w-3.5 items-center justify-center text-[#FFFFFF66]">
              {header.icon}
            </span>
          ) : null}
          <span className="font-[var(--font-mono)] text-[13px]">
            {header.title}
          </span>
        </div>
        {header.right ? (
          <div className="flex items-center gap-2">{header.right}</div>
        ) : null}
      </div>

      <div className="flex min-h-0 flex-1 overflow-hidden">
        <div
          ref={gutterRef}
          aria-hidden="true"
          className="scroll-thin shrink-0 overflow-hidden border-r border-[var(--color-line)] bg-[var(--color-paper-2)] px-2 py-5 pl-4 text-right font-[var(--font-mono)] text-[13px] whitespace-pre text-[var(--color-muted-2)] tabular-nums select-none"
          style={{ lineHeight: 1.7, width: 52 }}
        >
          {gutterContent}
        </div>
        <textarea
          ref={textareaRef}
          aria-label={ariaLabel}
          value={value}
          readOnly={readOnly}
          onChange={(e) => onChange(e.target.value)}
          placeholder={placeholder}
          spellCheck={false}
          className="scroll-thin min-h-0 flex-1 resize-none bg-transparent px-6 py-5 font-[var(--font-mono)] text-[13px] text-[var(--color-ink)] outline-none placeholder:text-[var(--color-muted-2)]"
          style={{ lineHeight: 1.7 }}
        />
      </div>

      {footer !== undefined ? (
        <div className="flex h-9 shrink-0 items-center justify-between gap-3 border-t border-[var(--color-line)] bg-[var(--color-paper-2)] px-4">
          {footer}
        </div>
      ) : null}
    </div>
  );
}
