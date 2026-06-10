import { useT } from "../../i18n";
import { describePrompt } from "./promptStats";

/** Footer row for a {@link GutteredEditor} that edits prompt-like text:
 *  line / token / char counts on the left, the mono font hint on the
 *  right. Shared by the agent system-prompt editor and the workspace
 *  organization-rule editor so the two read identically. Owns the
 *  `describePrompt` call so callers just hand it the current text. */
export function PromptStatsFooter({ value }: { value: string }) {
  const { t } = useT();
  const stats = describePrompt(value);
  return (
    <>
      <div className="flex items-center gap-5 font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)]">
        <span>{t("agent.detail.general.prompt.lines", { n: stats.lines })}</span>
        <span>
          {t("agent.detail.general.prompt.tokens", {
            n: stats.tokens.toLocaleString(),
          })}
        </span>
        <span>
          {t("agent.detail.general.prompt.chars", {
            n: stats.chars.toLocaleString(),
          })}
        </span>
      </div>
      <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)]">
        {t("agent.detail.general.prompt.fontHint")}
      </span>
    </>
  );
}
