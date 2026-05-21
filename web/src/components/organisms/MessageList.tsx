import { useMemo } from "react";
import { Inbox } from "lucide-react";
import { BoxedLabel } from "../molecules/BoxedLabel";
import { EmptyState } from "../molecules/EmptyState";
import { MessageBubble } from "./MessageBubble";
import { dateLabel, longDate } from "../../lib/time";
import { prefixMention } from "../../lib/mentions";
import type { ThreadSummary } from "../../types/api";

type Item = { t: ThreadSummary; key: string };
type DatedGroup = { label: string; items: Item[] };

export function MessageList({
  threads,
  channel,
  userName,
  humanPoster,
  onOpenThread,
}: {
  threads: ThreadSummary[];
  channel: string;
  userName: string;
  humanPoster?: { name: string; id: string; avatar_url?: string | null };
  onOpenThread?: (rootId: string) => void;
}) {
  const dated = useMemo<DatedGroup[]>(() => {
    const out: DatedGroup[] = [];
    for (const t of threads) {
      const label = `${dateLabel(t.created_at)} · ${longDate(t.created_at)}`;
      const last = out[out.length - 1];
      const item = { t, key: t.root_request_id };
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

  const human = humanPoster ?? { name: userName, id: "user", avatar_url: null };

  return (
    <div className="flex-1 overflow-y-auto scroll-thin grain-paper">
      <div className="flex flex-col py-2">
        {dated.map((d) => (
          <div key={d.label}>
            <BoxedLabel>{d.label}</BoxedLabel>
            {d.items.map((it) => {
              const text = prefixMention(it.t.preview, it.t.first_agent.name);
              return (
                <MessageBubble
                  key={it.key}
                  sender={{
                    kind: "human",
                    name: human.name,
                    id: human.id,
                    avatarUrl: human.avatar_url ?? null,
                  }}
                  body={text}
                  ts={it.t.created_at}
                  replyPill={
                    it.t.reply_count > 0
                      ? {
                          count: it.t.reply_count,
                          participants: [it.t.first_agent],
                          meta: it.t.first_agent.name,
                          onView: () => onOpenThread?.(it.t.root_request_id),
                        }
                      : undefined
                  }
                />
              );
            })}
          </div>
        ))}
      </div>
    </div>
  );
}

