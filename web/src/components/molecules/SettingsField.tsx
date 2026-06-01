import type { ReactNode } from "react";

/** A settings-form row: label + optional helper on the left, control on the
 *  right. Shared by the settings pages (General, Billing) so the two-column
 *  grid doesn't drift between them. */
export function SettingsField({
  label,
  helper,
  children,
}: {
  label: string;
  helper?: string;
  children: ReactNode;
}) {
  return (
    <div className="grid grid-cols-1 gap-6 md:grid-cols-[280px_1fr] md:gap-8">
      <div>
        <div className="text-[13px] font-semibold text-[var(--color-ink)]">
          {label}
        </div>
        {helper ? (
          <div className="mt-1 text-[12px] leading-snug text-[var(--color-muted-foreground)]">
            {helper}
          </div>
        ) : null}
      </div>
      <div className="min-w-0">{children}</div>
    </div>
  );
}
