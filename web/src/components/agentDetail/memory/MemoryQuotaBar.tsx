import { ProgressBar } from "../../atoms/ProgressBar";
import { useT } from "../../../i18n";
import {
  MAX_MEMORIES_PER_AGENT,
  quotaPercent,
} from "./memoryFilterState";

export function MemoryQuotaBar({ used }: { used: number }) {
  const { t } = useT();
  const pct = quotaPercent(used);
  return (
    <div className="flex flex-col gap-1.5 border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-2.5">
      <div className="flex items-center justify-between">
        <span className="font-[var(--font-mono)] text-[10px] tracking-[0.15em] text-[var(--color-muted-foreground)] uppercase">
          {t("agent.detail.memory.quota.eyebrow")}
        </span>
        <span className="font-[var(--font-mono)] text-[11px] font-semibold text-[var(--color-ink)]">
          {t("agent.detail.memory.quota.count", {
            used,
            max: MAX_MEMORIES_PER_AGENT,
          })}
        </span>
      </div>
      <ProgressBar
        value={used}
        max={MAX_MEMORIES_PER_AGENT}
        ariaLabel={t("agent.detail.memory.quota.eyebrow")}
      />
      <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)]">
        {t("agent.detail.memory.quota.caption", { pct })}
      </span>
    </div>
  );
}
