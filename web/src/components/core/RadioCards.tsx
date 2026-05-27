import type { LucideIcon } from "lucide-react";
import { Check } from "lucide-react";
import { cn } from "../../lib/utils";

export type RadioCardOption<V extends string> = {
  value: V;
  label: string;
  description: string;
  icon: LucideIcon;
};

export function RadioCards<V extends string>({
  options,
  value,
  onChange,
  ariaLabel,
}: {
  options: RadioCardOption<V>[];
  value: V;
  onChange: (next: V) => void;
  /** Form name. Unused now that we render a single ARIA radiogroup
   *  (a nested <input type="radio"> inside a <button role="radio">
   *  is invalid markup), but kept on the API so callers don't have
   *  to drop the prop on the floor. */
  name?: string;
  ariaLabel?: string;
}) {
  // ARIA pattern: one <button role="radio"> per option inside one
  // <div role="radiogroup">. No nested form control — the button is
  // the single interactive element, with `aria-checked` reflecting
  // state and keyboard activation (Enter / Space) handled by the
  // browser's native button affordance.
  return (
    <div role="radiogroup" aria-label={ariaLabel} className="grid grid-cols-3 gap-2">
      {options.map((opt) => {
        const active = opt.value === value;
        const Icon = opt.icon;
        return (
          <button
            key={opt.value}
            type="button"
            role="radio"
            aria-checked={active}
            tabIndex={active ? 0 : -1}
            onClick={() => onChange(opt.value)}
            className={cn(
              "relative flex cursor-pointer flex-col gap-1.5 border px-3 py-3 text-left transition-colors",
              active
                ? "border-[var(--color-moss)] bg-[var(--color-moss-tint)]"
                : "border-[var(--color-line)] bg-[var(--color-card)] hover:bg-[var(--color-paper-2)]",
            )}
          >
            <div className="flex items-center gap-1.5 text-[13px] font-semibold text-[var(--color-ink)]">
              <Icon
                className={cn(
                  "h-3.5 w-3.5",
                  active ? "text-[var(--color-moss-deep)]" : "text-[var(--color-muted-foreground)]",
                )}
                strokeWidth={1.75}
              />
              {opt.label}
              {active ? (
                <Check
                  className="ml-auto h-3.5 w-3.5 text-[var(--color-moss-deep)]"
                  strokeWidth={2}
                />
              ) : null}
            </div>
            <div className="font-[var(--font-mono)] text-[10.5px] leading-tight text-[var(--color-muted-foreground)]">
              {opt.description}
            </div>
          </button>
        );
      })}
    </div>
  );
}
