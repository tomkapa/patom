import { useCallback, useRef, useState } from "react";
import type { ReactNode } from "react";
import { Check, ChevronsUpDown } from "lucide-react";
import { useDismissable } from "../../hooks/useDismissable";
import { cn } from "../../lib/utils";

export type SelectOption<V extends string> = {
  value: V;
  label: ReactNode;
  caption?: ReactNode;
  /** Leading icon/avatar rendered before the label in both the
   *  default trigger chip and each menu row. */
  leading?: ReactNode;
};

type RenderTrigger<V extends string> = (state: {
  selected: SelectOption<V> | null;
  open: boolean;
}) => ReactNode;

/** Generic single-select dropdown. The default trigger matches the
 *  agent-detail "Model" chip in design.pen `qZPNz` (border, mono label,
 *  ChevronsUpDown affordance); callers can override via `renderTrigger`. */
export function Select<V extends string>({
  value,
  options,
  onChange,
  placeholder,
  renderTrigger,
  width,
  disabled,
  ariaLabel,
  className,
}: {
  value: V | null;
  options: SelectOption<V>[];
  onChange: (next: V) => void;
  placeholder?: ReactNode;
  renderTrigger?: RenderTrigger<V>;
  width?: number | string;
  disabled?: boolean;
  ariaLabel: string;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const close = useCallback(() => setOpen(false), []);
  useDismissable(rootRef, open, close);

  const selected = options.find((o) => o.value === value) ?? null;

  const onPick = (v: V) => {
    setOpen(false);
    if (v !== value) onChange(v);
  };

  const trigger = renderTrigger
    ? renderTrigger({ selected, open })
    : defaultTrigger({ selected, placeholder });

  return (
    <div
      ref={rootRef}
      className={cn("relative", className)}
      style={width !== undefined ? { width } : undefined}
    >
      <button
        type="button"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={ariaLabel}
        disabled={disabled}
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center justify-between gap-2 border border-[var(--color-line-strong)] bg-[var(--color-card)] px-3 py-2 text-left outline-none transition-colors hover:bg-[var(--color-paper-2)] focus-visible:ring-1 focus-visible:ring-[var(--color-ink)] disabled:cursor-not-allowed disabled:opacity-50"
      >
        {trigger}
      </button>

      {open ? (
        <ul
          role="listbox"
          aria-label={ariaLabel}
          className="scroll-thin absolute top-full right-0 left-0 z-20 mt-1 max-h-[60vh] overflow-y-auto border border-[var(--color-line)] bg-[var(--color-card)] py-1 shadow-md"
        >
          {options.length === 0 ? (
            <li className="px-3 py-2 text-[12.5px] text-[var(--color-muted)]">
              {placeholder ?? "No options"}
            </li>
          ) : (
            options.map((opt) => {
              const isActive = opt.value === value;
              return (
                <li key={opt.value}>
                  <button
                    type="button"
                    role="option"
                    aria-selected={isActive}
                    onClick={() => onPick(opt.value)}
                    className={cn(
                      "flex w-full items-center gap-2.5 px-3 py-2 text-left hover:bg-[var(--color-paper-2)]",
                      isActive && "bg-[var(--color-moss-tint)]",
                    )}
                  >
                    {opt.leading ? (
                      <span className="flex h-4 w-4 shrink-0 items-center justify-center text-[var(--color-moss)]">
                        {opt.leading}
                      </span>
                    ) : null}
                    <span className="min-w-0 flex-1">
                      <span className="block truncate font-[var(--font-mono)] text-[13px] text-[var(--color-ink)]">
                        {opt.label}
                      </span>
                      {opt.caption ? (
                        <span className="block truncate font-[var(--font-mono)] text-[10.5px] tracking-[0.08em] text-[var(--color-muted)] uppercase">
                          {opt.caption}
                        </span>
                      ) : null}
                    </span>
                    {isActive ? (
                      <Check className="h-3.5 w-3.5 shrink-0 text-[var(--color-moss)]" />
                    ) : null}
                  </button>
                </li>
              );
            })
          )}
        </ul>
      ) : null}
    </div>
  );
}

function defaultTrigger<V extends string>({
  selected,
  placeholder,
}: {
  selected: SelectOption<V> | null;
  placeholder?: ReactNode;
}) {
  return (
    <>
      <span className="flex min-w-0 items-center gap-2.5">
        {selected?.leading ? (
          <span className="flex h-3.5 w-3.5 shrink-0 items-center justify-center text-[var(--color-moss)]">
            {selected.leading}
          </span>
        ) : null}
        <span className="truncate font-[var(--font-mono)] text-[13px] text-[var(--color-ink)]">
          {selected?.label ?? placeholder ?? "Select…"}
        </span>
      </span>
      <ChevronsUpDown
        className="h-3.5 w-3.5 shrink-0 text-[var(--color-muted-2)]"
        strokeWidth={1.75}
      />
    </>
  );
}
