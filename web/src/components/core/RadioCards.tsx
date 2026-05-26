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
  name,
  ariaLabel,
}: {
  options: RadioCardOption<V>[];
  value: V;
  onChange: (next: V) => void;
  name: string;
  ariaLabel?: string;
}) {
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
                  active ? "text-[var(--color-moss-deep)]" : "text-[var(--color-muted)]",
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
            <div className="font-[var(--font-mono)] text-[10.5px] leading-tight text-[var(--color-muted)]">
              {opt.description}
            </div>
            <input
              type="radio"
              name={name}
              checked={active}
              onChange={() => onChange(opt.value)}
              className="sr-only"
            />
          </button>
        );
      })}
    </div>
  );
}
