import { Cpu } from "lucide-react";
import { Select } from "../molecules/Select";
import type { SelectOption } from "../molecules/Select";
import type { ModelEntry } from "../../types/api";

const WORKSPACE_DEFAULT = "__default__";

/** Left-side label + caption, right-side model picker chip. Specific to
 *  the agent General tab copy — the generic widget lives in
 *  `molecules/Select`. */
export function ModelPickerRow({
  models,
  value,
  onChange,
  label,
  caption,
  inheritLabel,
  ariaLabel,
}: {
  models: ModelEntry[];
  /** Pinned model id, or `null` to inherit the workspace default. */
  value: string | null;
  onChange: (next: string | null) => void;
  label: string;
  caption: string;
  inheritLabel: string;
  ariaLabel: string;
}) {
  const options: SelectOption<string>[] = [
    {
      value: WORKSPACE_DEFAULT,
      label: inheritLabel,
      caption: "default",
      leading: <Cpu className="h-3.5 w-3.5" strokeWidth={1.75} />,
    },
    ...models.map((m) => ({
      value: m.id,
      label: m.id,
      caption: m.provider,
      leading: <Cpu className="h-3.5 w-3.5" strokeWidth={1.75} />,
    })),
  ];

  return (
    <div className="flex flex-col items-start gap-3 md:flex-row md:items-center md:justify-between md:gap-6">
      <div className="flex min-w-0 flex-col gap-1">
        <div className="text-[14px] font-medium text-[var(--color-ink)]">
          {label}
        </div>
        <div className="max-w-[42ch] text-[13px] text-[var(--color-muted-foreground)]">
          {caption}
        </div>
      </div>
      <Select
        ariaLabel={ariaLabel}
        value={value ?? WORKSPACE_DEFAULT}
        options={options}
        onChange={(next) =>
          onChange(next === WORKSPACE_DEFAULT ? null : next)
        }
        width={280}
      />
    </div>
  );
}
