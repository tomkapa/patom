import { memo } from "react";
import { Check, MoreHorizontal, Smile } from "lucide-react";
import { Button } from "../atoms/Button";
import { Monogram } from "../atoms/Monogram";
import { Markdown } from "../molecules/Markdown";
import { ReplyPill } from "../molecules/ReplyPill";
import { cn } from "../../lib/utils";
import { renderMentions } from "../../lib/mentions";
import { clockTime } from "../../lib/time";
import type { AgentRef } from "../../types/api";

export type BubbleSender =
  | { kind: "human"; name: string; id?: string; avatarUrl?: string | null }
  | { kind: "agent"; agent: AgentRef };

type MessageBubbleProps = {
  sender: BubbleSender;
  body?: string;
  ts: string;
  fresh?: boolean;
  replyPill?: {
    count: number;
    participants: { id: string; name: string }[];
    meta?: string;
    onView?: () => void;
  };
};

export const MessageBubble = memo(function MessageBubble({
  sender,
  body,
  ts,
  fresh = false,
  replyPill,
}: MessageBubbleProps) {
  const text = body ?? "";
  const isHuman = sender.kind === "human";
  const senderName = isHuman ? sender.name : sender.agent.name;
  const senderId = isHuman ? sender.id ?? "user" : sender.agent.id;
  const senderAvatar = isHuman ? sender.avatarUrl ?? null : null;

  return (
    <article
      className={cn(
        "group relative flex gap-3 px-4 md:px-8 py-2 hover:bg-[var(--color-paper-2)]/40 transition-colors",
        fresh && "bubble-in",
      )}
    >
      <div className="pt-[3px]">
        <Monogram
          name={senderName}
          id={senderId}
          tone={isHuman ? undefined : "moss"}
          size={32}
          avatarUrl={senderAvatar}
        />
      </div>
      <div className="min-w-0 flex-1">
        <header className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
          <span className="font-[var(--font-display)] text-[14px] font-bold tracking-tight text-[var(--color-ink)]">
            {senderName}
          </span>
          {!isHuman && (
            <span className="border border-[var(--color-moss)] px-1 font-[var(--font-mono)] text-[9.5px] font-bold uppercase tracking-[0.14em] text-[var(--color-moss)]">
              AGENT
            </span>
          )}
          <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-fg-muted)]">
            {clockTime(ts)}
          </span>
        </header>

        <div className="mt-1 space-y-1.5">
          {text &&
            (isHuman ? (
              <p className="font-[var(--font-sans)] text-[14px] leading-[1.55] text-[var(--color-ink)] whitespace-pre-wrap">
                {renderMentions(text)}
              </p>
            ) : (
              <Markdown text={text} />
            ))}

          {replyPill && (
            <ReplyPill
              replyCount={replyPill.count}
              participants={replyPill.participants}
              meta={replyPill.meta}
              onView={replyPill.onView}
            />
          )}
        </div>
      </div>

      <div className="absolute right-6 top-1.5 hidden items-center gap-0.5 border border-[var(--color-line)] bg-[var(--color-card)] p-0.5 shadow-[0_1px_3px_rgba(26,43,30,0.1)] group-hover:flex">
        <Button variant="ghost" size="xs" iconOnly aria-label="Smile">
          <Smile className="h-3.5 w-3.5" />
        </Button>
        <Button variant="ghost" size="xs" iconOnly aria-label="Acknowledge">
          <Check className="h-3.5 w-3.5" />
        </Button>
        <Button variant="ghost" size="xs" iconOnly aria-label="More">
          <MoreHorizontal className="h-3.5 w-3.5" />
        </Button>
      </div>
    </article>
  );
});
