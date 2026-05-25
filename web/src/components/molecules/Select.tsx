import type { ReactNode } from "react";
import { Check, ChevronsUpDown } from "lucide-react";
import { Dropdown } from "./Dropdown";
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
 *  ChevronsUpDown affordance); callers can override via `renderTrigger`.
 *  Built on the `Dropdown` primitive so all open/close behaviour stays
 *  in lockstep with the other dropdowns in the app. */
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
  const selected = options.find((o) => o.value === value) ?? null;

  return (
    <Dropdown
      rootClassName={className}
      menuClassName="max-h-[60vh] overflow-y-auto border border-[var(--color-line)] bg-[var(--color-card)] py-1 shadow-md scroll-thin"
      renderTrigger={({ open, toggle }) => (
        <div style={width !== undefined ? { width } : undefined}>
          <button
            type="button"
            aria-haspopup="listbox"
            aria-expanded={open}
            aria-label={ariaLabel}
            disabled={disabled}
            onClick={toggle}
            className="flex w-full items-center justify-between gap-2 border border-[var(--color-line-strong)] bg-[var(--color-card)] px-3 py-2 text-left outline-none transition-colors hover:bg-[var(--color-paper-2)] focus-visible:ring-1 focus-visible:ring-[var(--color-ink)] disabled:cursor-not-allowed disabled:opacity-50"
          >
            {renderTrigger
              ? renderTrigger({ selected, open })
              : defaultTrigger({ selected, placeholder })}
          </button>
        </div>
      )}
    >
      {({ close }) => (
        <ul role="listbox" aria-label={ariaLabel}>
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
                    onClick={() => {
                      close();
                      if (opt.value !== value) onChange(opt.value);
                    }}
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
      )}
    </Dropdown>
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
