import { useId, type InputHTMLAttributes, type ReactNode } from "react";
import { Check } from "lucide-react";
import { cn } from "../../lib/utils";

type Props = {
  checked: boolean;
  onChange: (next: boolean) => void;
  label?: ReactNode;
  className?: string;
} & Omit<InputHTMLAttributes<HTMLInputElement>, "checked" | "onChange" | "type" | "className">;

export function Checkbox({
  checked,
  onChange,
  label,
  className,
  id,
  disabled,
  ...rest
}: Props) {
  const generatedId = useId();
  const boxId = id ?? generatedId;
  return (
    <label
      htmlFor={boxId}
      className={cn(
        "inline-flex items-center gap-2",
        disabled ? "cursor-not-allowed opacity-60" : "cursor-pointer",
        className,
      )}
    >
      <span
        className={cn(
          "flex h-[14px] w-[14px] shrink-0 items-center justify-center border transition-colors duration-150 ease-out",
          checked
            ? "border-[var(--color-moss)] bg-[var(--color-moss)]"
            : "border-[var(--color-line)] bg-[var(--color-card)]",
        )}
      >
        <input
          id={boxId}
          type="checkbox"
          checked={checked}
          onChange={(e) => onChange(e.target.checked)}
          disabled={disabled}
          className="absolute h-0 w-0 opacity-0"
          {...rest}
        />
        <Check
          className={cn(
            "h-2.5 w-2.5 text-white transition-[opacity,transform] duration-150 ease-out",
            checked ? "scale-100 opacity-100" : "scale-50 opacity-0",
          )}
          strokeWidth={3}
          aria-hidden="true"
        />
      </span>
      {label ? (
        <span className="text-[12px] text-[var(--color-ink)]">{label}</span>
      ) : null}
    </label>
  );
}
