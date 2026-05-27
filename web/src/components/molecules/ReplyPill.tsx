import { ArrowRight, MessageSquare } from "lucide-react";
import { Monogram } from "../atoms/Monogram";
import { dedupeById } from "../../lib/utils";

export function ReplyPill({
  replyCount,
  participants,
  meta,
  onView,
}: {
  replyCount: number;
  participants: { id: string; name: string }[];
  meta?: string;
  onView?: () => void;
}) {
  return (
    <div className="mt-2 flex flex-wrap items-center gap-3 border border-[var(--color-line)] bg-[var(--color-paper-2)] px-2.5 py-1.5">
      <button
        onClick={onView}
        className="inline-flex items-center gap-1.5 font-[var(--font-mono)] text-[11.5px] font-semibold text-[var(--color-moss-deep)] hover:text-[var(--color-moss)] transition-colors"
      >
        <MessageSquare className="h-3 w-3" />
        {replyCount} {replyCount === 1 ? "reply" : "replies"} in thread
      </button>
      <span className="flex -space-x-1">
        {dedupeById(participants)
          .slice(0, 3)
          .map((p) => (
            <Monogram
              key={p.id}
              name={p.name}
              id={p.id}
              size={18}
              className="ring-1 ring-[var(--color-paper-2)]"
            />
          ))}
      </span>
      {meta && (
        <span className="font-[var(--font-mono)] text-[10.5px] text-[var(--color-muted-foreground)]">
          {meta}
        </span>
      )}
      <button
        onClick={onView}
        className="ml-auto inline-flex items-center gap-1 font-[var(--font-mono)] text-[10.5px] font-medium text-[var(--color-muted-foreground)] hover:text-[var(--color-ink)] transition-colors"
      >
        View thread <ArrowRight className="h-3 w-3" />
      </button>
    </div>
  );
}
