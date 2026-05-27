import { useState, type KeyboardEvent } from "react";
import { X } from "lucide-react";
import { cn } from "../../lib/utils";

/** RFC-ish check: matches the BE's `Email::try_from` shape (must
 *  contain `@`, neither side empty, ≤320 chars). */
function isValidEmail(raw: string): boolean {
  const t = raw.trim();
  if (t.length < 3 || t.length > 320) return false;
  const at = t.indexOf("@");
  if (at <= 0 || at >= t.length - 1) return false;
  return true;
}

export type EmailChip = { value: string; invalid: boolean };

export function EmailChipsInput({
  chips,
  onChange,
  placeholder = "name@company.com",
  helperRight,
  ariaLabel = "Emails",
}: {
  chips: EmailChip[];
  onChange: (next: EmailChip[]) => void;
  placeholder?: string;
  helperRight?: string;
  ariaLabel?: string;
}) {
  const [draft, setDraft] = useState("");

  const commit = (raw: string) => {
    const value = raw.trim().replace(/,$/, "");
    if (!value) return;
    if (chips.some((c) => c.value === value)) {
      setDraft("");
      return;
    }
    onChange([...chips, { value, invalid: !isValidEmail(value) }]);
    setDraft("");
  };

  const onKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter" || e.key === ",") {
      e.preventDefault();
      commit(draft);
    } else if (e.key === "Backspace" && !draft && chips.length > 0) {
      onChange(chips.slice(0, -1));
    }
  };

  const onPaste = (e: React.ClipboardEvent<HTMLInputElement>) => {
    const text = e.clipboardData.getData("text");
    if (!/[,\s]/.test(text)) return;
    e.preventDefault();
    const out = [...chips];
    for (const piece of text.split(/[,\s]+/)) {
      const value = piece.trim();
      if (!value || out.some((c) => c.value === value)) continue;
      out.push({ value, invalid: !isValidEmail(value) });
    }
    onChange(out);
  };

  return (
    <div>
      {helperRight ? (
        <div className="mb-1.5 flex items-center justify-end font-[var(--font-mono)] text-[10.5px] tracking-[0.06em] text-[var(--color-muted-foreground)] uppercase">
          {helperRight}
        </div>
      ) : null}
      <div
        className="flex flex-wrap items-center gap-1.5 border border-[var(--color-line)] bg-[var(--color-card)] px-2 py-2 focus-within:ring-1 focus-within:ring-[var(--color-moss)]"
        role="group"
        aria-label={ariaLabel}
      >
        {chips.map((c, idx) => (
          <span
            key={`${c.value}-${idx}`}
            className={cn(
              "inline-flex items-center gap-1 border px-2 py-0.5 font-[var(--font-mono)] text-[12px]",
              c.invalid
                ? "border-[var(--color-rose)] bg-[var(--color-rose-soft)] text-[var(--color-rose)]"
                : "border-[var(--color-moss-soft)] bg-[var(--color-moss-soft)] text-[var(--color-moss-deep)]",
            )}
          >
            {c.value}
            <button
              type="button"
              onClick={() =>
                onChange(chips.filter((_, i) => i !== idx))
              }
              aria-label={`Remove ${c.value}`}
              className="cursor-pointer p-0.5 opacity-70 hover:opacity-100"
            >
              <X className="h-3 w-3" />
            </button>
          </span>
        ))}
        <input
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={onKey}
          onPaste={onPaste}
          onBlur={() => draft && commit(draft)}
          placeholder={chips.length === 0 ? placeholder : ""}
          className="min-w-[12ch] flex-1 bg-transparent px-1 py-1 font-[var(--font-mono)] text-[12px] text-[var(--color-ink)] outline-none placeholder:text-[var(--color-fg-muted)]"
        />
      </div>
    </div>
  );
}
