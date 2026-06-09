import { useMemo } from "react";
import { ArrowRight, Inbox, MessagesSquare } from "lucide-react";
import { BoxedLabel } from "../molecules/BoxedLabel";
import { EmptyState } from "../molecules/EmptyState";
import { clockTime, dateLabel, longDate } from "../../lib/time";
import type { ThreadSummary } from "../../types/api";

type Item = { t: ThreadSummary; key: string };
type DatedGroup = { label: string; items: Item[] };

// The thread-feed wire (G1) is intentionally thin — a row is just
// `{ thread_id, channel_id, last_activity_at }`. The preview / starter /
// first-agent fields the old card rendered no longer exist on this row;
// any title is derived from the thread's first message (G2) when the
// thread is opened. The feed list therefore renders a compact "open this
// thread" affordance per row rather than inventing fields the API dropped.
export function MessageList({
  threads,
  channel,
  onOpenThread,
}: {
  threads: ThreadSummary[];
  channel: string;
  onOpenThread?: (threadId: string) => void;
}) {
  const dated = useMemo<DatedGroup[]>(() => {
    const out: DatedGroup[] = [];
    for (const t of threads) {
      const label = `${dateLabel(t.last_activity_at)} · ${longDate(t.last_activity_at)}`;
      const last = out[out.length - 1];
      const item = { t, key: t.thread_id };
      if (last && last.label === label) last.items.push(item);
      else out.push({ label, items: [item] });
    }
    return out;
  }, [threads]);

  if (threads.length === 0) {
    return (
      <div className="flex-1 grain-paper">
        <EmptyState
          icon={<Inbox className="h-5 w-5" />}
          title={`Welcome to #${channel}`}
          description="Start a thread with the composer below. Each thread is its own DAG of agent ↔ agent conversations."
        />
      </div>
    );
  }

  return (
    <div className="flex-1 overflow-y-auto scroll-thin grain-paper">
      <div className="flex flex-col py-2">
        {dated.map((d) => (
          <div key={d.label}>
            <BoxedLabel>{d.label}</BoxedLabel>
            {d.items.map((it) => (
              <ThreadRow
                key={it.key}
                thread={it.t}
                onOpen={() => onOpenThread?.(it.t.thread_id)}
              />
            ))}
          </div>
        ))}
      </div>
    </div>
  );
}

function ThreadRow({
  thread,
  onOpen,
}: {
  thread: ThreadSummary;
  onOpen: () => void;
}) {
  return (
    <article className="group flex items-center gap-3 px-4 md:px-8 py-2.5 hover:bg-[var(--color-paper-2)]/40 transition-colors">
      <span className="grid h-8 w-8 shrink-0 place-items-center border border-[var(--color-line)] bg-[var(--color-card)] text-[var(--color-moss)]">
        <MessagesSquare className="h-4 w-4" />
      </span>
      <div className="min-w-0 flex-1">
        <header className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
          <span className="font-[var(--font-mono)] text-[12px] font-medium text-[var(--color-ink)]">
            Thread
          </span>
          <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-fg-muted)]">
            {clockTime(thread.last_activity_at)}
          </span>
        </header>
      </div>
      <button
        onClick={onOpen}
        className="inline-flex items-center gap-1 font-[var(--font-mono)] text-[11px] font-medium text-[var(--color-moss-deep)] hover:text-[var(--color-moss)] transition-colors"
      >
        View thread <ArrowRight className="h-3 w-3" />
      </button>
    </article>
  );
}
