import type { ReactNode } from "react";
import { Check, ChevronDown, ChevronsUpDown } from "lucide-react";
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

/** Visual mode.
 *  - `"default"` — full-width chip styled after the agent-detail Model
 *    picker (`qZPNz` in design.pen). Big rows with leading slot, caption,
 *    and a Check on the active option.
 *  - `"filter"` — compact `h-7` chip for filter bars (Memory page Kind /
 *    State / Pinned). Chrome turns moss-tint when `active` is true; rows
 *    are dense, single-line, highlight the active option via background.
 *
 *  When you reach for a single-select dropdown anywhere in the app, this
 *  is the component — variants cover both the prominent picker and the
 *  filter-row chip without duplicating Dropdown plumbing or listbox a11y. */
export type SelectVariant = "default" | "filter";

type RenderTrigger<V extends string> = (state: {
  selected: SelectOption<V> | null;
  open: boolean;
}) => ReactNode;

/** Generic single-select dropdown. Use `variant="default"` for prominent
 *  pickers (model, kind, state in modals) and `variant="filter"` for the
 *  compact filter-bar chips. Both render the same listbox semantics, the
 *  same open/close primitive (Dropdown), and the same option-row a11y
 *  contract — only the chrome differs. */
export function Select<V extends string>({
  value,
  options,
  onChange,
  ariaLabel,
  className,
  disabled,
  variant = "default",
  // default-variant trigger:
  placeholder,
  renderTrigger,
  width,
  // filter-variant trigger:
  triggerLabel,
  icon,
  active = false,
}: {
  value: V | null;
  options: SelectOption<V>[];
  onChange: (next: V) => void;
  ariaLabel: string;
  className?: string;
  disabled?: boolean;
  variant?: SelectVariant;
  // ── default-variant only ──────────────────────────────────────────
  placeholder?: ReactNode;
  renderTrigger?: RenderTrigger<V>;
  width?: number | string;
  // ── filter-variant only ───────────────────────────────────────────
  /** Trigger label — the caller controls the text (often the selected
   *  option's label, or a category fallback when value is the "any"
   *  sentinel). */
  triggerLabel?: string;
  /** Leading icon for the filter chip. */
  icon?: ReactNode;
  /** Whether the chip should render in its selected / moss-tint state.
   *  Usually `value !== "any"` or `value !== null`. */
  active?: boolean;
}) {
  const selected = options.find((o) => o.value === value) ?? null;
  const isFilter = variant === "filter";

  return (
    <Dropdown
      rootClassName={className}
      placement={isFilter ? "bottom-start" : "bottom-stretch"}
      menuClassName={cn(
        "border border-[var(--color-line)] bg-[var(--color-card)] py-1 shadow-md scroll-thin",
        isFilter ? "min-w-[160px]" : "max-h-[60vh] overflow-y-auto",
      )}
      renderTrigger={({ open, toggle }) =>
        isFilter ? (
          <FilterTrigger
            ariaLabel={ariaLabel}
            disabled={disabled}
            open={open}
            onToggle={toggle}
            icon={icon}
            label={triggerLabel ?? ""}
            active={active}
          />
        ) : (
          <DefaultTrigger
            ariaLabel={ariaLabel}
            disabled={disabled}
            open={open}
            onToggle={toggle}
            width={width}
          >
            {renderTrigger
              ? renderTrigger({ selected, open })
              : defaultTriggerContent({ selected, placeholder })}
          </DefaultTrigger>
        )
      }
    >
      {({ close }) => (
        <ul role="listbox" aria-label={ariaLabel}>
          {options.length === 0 ? (
            <li
              className={cn(
                "text-[12.5px] text-[var(--color-muted)]",
                isFilter ? "px-3 py-1.5" : "px-3 py-2",
              )}
            >
              {placeholder ?? "No options"}
            </li>
          ) : (
            options.map((opt) => {
              const isActive = opt.value === value;
              const onPick = () => {
                close();
                if (opt.value !== value) onChange(opt.value);
              };
              return (
                <li key={opt.value}>
                  {isFilter ? (
                    <FilterRow option={opt} isActive={isActive} onPick={onPick} />
                  ) : (
                    <DefaultRow option={opt} isActive={isActive} onPick={onPick} />
                  )}
                </li>
              );
            })
          )}
        </ul>
      )}
    </Dropdown>
  );
}

function DefaultTrigger({
  ariaLabel,
  disabled,
  open,
  onToggle,
  width,
  children,
}: {
  ariaLabel: string;
  disabled?: boolean;
  open: boolean;
  onToggle: () => void;
  width?: number | string;
  children: ReactNode;
}) {
  return (
    <div style={width !== undefined ? { width } : undefined}>
      <button
        type="button"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={ariaLabel}
        disabled={disabled}
        onClick={onToggle}
        className="flex w-full cursor-pointer items-center justify-between gap-2 border border-[var(--color-line-strong)] bg-[var(--color-card)] px-3 py-2 text-left outline-none transition-colors duration-150 ease-out hover:bg-[var(--color-paper-2)] focus-visible:ring-1 focus-visible:ring-[var(--color-ink)] disabled:cursor-not-allowed disabled:opacity-50"
      >
        {children}
      </button>
    </div>
  );
}

function FilterTrigger({
  ariaLabel,
  disabled,
  open,
  onToggle,
  icon,
  label,
  active,
}: {
  ariaLabel: string;
  disabled?: boolean;
  open: boolean;
  onToggle: () => void;
  icon?: ReactNode;
  label: string;
  active: boolean;
}) {
  return (
    <button
      type="button"
      aria-haspopup="listbox"
      aria-expanded={open}
      aria-label={ariaLabel}
      disabled={disabled}
      onClick={onToggle}
      className={cn(
        "flex h-7 cursor-pointer items-center gap-1.5 border px-2.5 text-[12px] outline-none transition-colors duration-150 ease-out focus-visible:ring-1 focus-visible:ring-[var(--color-ink)] disabled:cursor-not-allowed disabled:opacity-50",
        active
          ? "border-[var(--color-moss)] bg-[var(--color-moss-tint)] text-[var(--color-moss-deep)]"
          : "border-[var(--color-line)] bg-[var(--color-card)] text-[var(--color-muted)] hover:text-[var(--color-ink)]",
      )}
    >
      {icon ? (
        <span
          className={cn(
            "shrink-0",
            active ? "text-[var(--color-moss)]" : "text-[var(--color-muted-2)]",
          )}
        >
          {icon}
        </span>
      ) : null}
      <span>{label}</span>
      <ChevronDown
        className="h-3 w-3 text-[var(--color-muted)]"
        strokeWidth={1.75}
      />
    </button>
  );
}

function DefaultRow<V extends string>({
  option,
  isActive,
  onPick,
}: {
  option: SelectOption<V>;
  isActive: boolean;
  onPick: () => void;
}) {
  return (
    <button
      type="button"
      role="option"
      aria-selected={isActive}
      onClick={onPick}
      className={cn(
        "flex w-full cursor-pointer items-center gap-2.5 px-3 py-2 text-left transition-colors duration-100 ease-out hover:bg-[var(--color-paper-2)]",
        isActive && "bg-[var(--color-moss-tint)]",
      )}
    >
      {option.leading ? (
        <span className="flex h-4 w-4 shrink-0 items-center justify-center text-[var(--color-moss)]">
          {option.leading}
        </span>
      ) : null}
      <span className="min-w-0 flex-1">
        <span className="block truncate font-[var(--font-mono)] text-[13px] text-[var(--color-ink)]">
          {option.label}
        </span>
        {option.caption ? (
          <span className="block truncate font-[var(--font-mono)] text-[10.5px] tracking-[0.08em] text-[var(--color-muted)] uppercase">
            {option.caption}
          </span>
        ) : null}
      </span>
      {isActive ? (
        <Check className="h-3.5 w-3.5 shrink-0 text-[var(--color-moss)]" />
      ) : null}
    </button>
  );
}

function FilterRow<V extends string>({
  option,
  isActive,
  onPick,
}: {
  option: SelectOption<V>;
  isActive: boolean;
  onPick: () => void;
}) {
  return (
    <button
      type="button"
      role="option"
      aria-selected={isActive}
      onClick={onPick}
      className={cn(
        "flex w-full cursor-pointer items-center justify-between gap-2 px-3 py-1.5 text-left text-[12.5px] transition-colors duration-100 ease-out hover:bg-[var(--color-paper-2)]",
        isActive && "bg-[var(--color-moss-tint)] text-[var(--color-moss-deep)]",
      )}
    >
      <span>{option.label}</span>
    </button>
  );
}

function defaultTriggerContent<V extends string>({
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
