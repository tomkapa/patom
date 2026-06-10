import { useMemo } from "react";
import { ArrowRight, Inbox, MessageSquareText, Sparkles } from "lucide-react";
import { BoxedLabel } from "../molecules/BoxedLabel";
import { EmptyState } from "../molecules/EmptyState";
import { Monogram } from "../atoms/Monogram";
import { renderMentions } from "../../lib/mentions";
import { clockTime, dateLabel, longDate } from "../../lib/time";
import { useT } from "../../i18n";
import type { Mentionable, ThreadSummary } from "../../types/api";

/** First-contact orientation, shown in place of the generic empty state
 *  when the system channel (#general) has no threads yet — i.e. right
 *  after onboarding. The CTA does NOT post; it drops a prefilled message
 *  to the Recruiter into the composer so the user reviews and sends. */
export type WelcomeContext = { userName: string; onIntro: () => void };

type Item = { t: ThreadSummary; key: string };
type DatedGroup = { label: string; items: Item[] };

/** Slack-style timeline: each thread renders as its root posted message —
 *  author, snippet, time — with a reply affordance. The G1 wire carries the
 *  root summary + reply count so no per-thread feed fetch is needed. */
export function MessageList({
  threads,
  roster,
  channel,
  welcome,
  onOpenThread,
}: {
  threads: ThreadSummary[];
  /** Names for mention highlighting + agent author resolution. */
  roster: Mentionable[];
  channel: string;
  /** When present and the timeline is empty, show the first-contact
   *  orientation instead of the generic empty state. Null elsewhere. */
  welcome?: WelcomeContext | null;
  onOpenThread?: (threadId: string) => void;
}) {
  const dated = useMemo<DatedGroup[]>(() => {
    const out: DatedGroup[] = [];
    // Timeline order: oldest day first, like a chat log (G1 is
    // newest-activity-first for the sidebar's benefit).
    const ordered = [...threads].sort((a, b) =>
      (a.root?.created_at ?? a.last_activity_at) <
      (b.root?.created_at ?? b.last_activity_at)
        ? -1
        : 1,
    );
    for (const t of ordered) {
      const ts = t.root?.created_at ?? t.last_activity_at;
      const label = `${dateLabel(ts)} · ${longDate(ts)}`;
      const last = out[out.length - 1];
      const item = { t, key: t.thread_id };
      if (last && last.label === label) last.items.push(item);
      else out.push({ label, items: [item] });
    }
    return out;
  }, [threads]);

  // Built once per roster change rather than per row: the mention name list
  // and an agent-by-id lookup the root-author resolution needs.
  const names = useMemo(() => roster.map((m) => m.name), [roster]);
  const agentsById = useMemo(
    () => new Map(roster.filter((m) => m.kind === "agent").map((m) => [m.id, m])),
    [roster],
  );

  if (threads.length === 0) {
    return (
      <div className="flex-1 grain-paper">
        {welcome ? (
          <WelcomeCard welcome={welcome} />
        ) : (
          <EmptyState
            icon={<Inbox className="h-5 w-5" />}
            title={`Welcome to #${channel}`}
            description="Say something below — tag an agent or a colleague with @, or just post."
          />
        )}
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
                names={names}
                agentsById={agentsById}
                onOpen={() => onOpenThread?.(it.t.thread_id)}
              />
            ))}
          </div>
        ))}
      </div>
    </div>
  );
}

/** Rendered as a system-style notice in the empty system channel: a
 *  greeting + a single CTA that prefills the composer (no auto-post). */
function WelcomeCard({ welcome }: { welcome: WelcomeContext }) {
  const { t } = useT();
  return (
    <div className="flex h-full items-center justify-center px-4 py-10">
      <div className="flex w-full max-w-[440px] flex-col gap-4 border border-[var(--color-line-strong)] bg-[var(--color-card)] p-6 shadow-sm">
        <div className="flex items-center gap-2.5">
          <span className="flex h-8 w-8 items-center justify-center bg-[var(--color-moss)] text-white">
            <Sparkles className="h-4 w-4" />
          </span>
          <span className="font-[var(--font-mono)] text-[10px] uppercase tracking-[0.16em] text-[var(--color-moss)]">
            Patom
          </span>
        </div>
        <h2 className="font-[var(--font-display)] text-[19px] font-bold leading-tight text-[var(--color-ink)]">
          {t("chat.welcome.title", { name: welcome.userName })}
        </h2>
        <p className="text-[13.5px] leading-[1.55] text-[var(--color-fg-secondary)]">
          {t("chat.welcome.body")}
        </p>
        <button
          type="button"
          onClick={welcome.onIntro}
          data-testid="welcome-intro-cta"
          className="inline-flex items-center justify-center gap-2 self-start bg-[var(--color-moss)] px-4 py-2.5 text-[14px] font-semibold text-white transition-colors hover:bg-[var(--color-moss-deep)]"
        >
          {t("chat.welcome.cta")}
          <ArrowRight className="h-3.5 w-3.5" />
        </button>
      </div>
    </div>
  );
}

/** Resolve the root author's display name + avatar from the wire (humans
 *  are profile-enriched server-side; agents resolve via the roster map). */
function rootAuthor(
  thread: ThreadSummary,
  agentsById: ReadonlyMap<string, Mentionable>,
): { name: string; id: string; avatarUrl: string | null; isAgent: boolean } {
  const root = thread.root;
  if (!root) return { name: "…", id: thread.thread_id, avatarUrl: null, isAgent: false };
  const sender = root.sender;
  if (sender.kind === "agent") {
    const agent = agentsById.get(sender.agent_id);
    return {
      name: agent?.name ?? "agent",
      id: sender.agent_id,
      avatarUrl: agent?.avatar_url ?? null,
      isAgent: true,
    };
  }
  if (sender.kind === "human") {
    return {
      name: root.sender_display_name ?? "member",
      id: sender.user_id,
      avatarUrl: root.sender_avatar_url,
      isAgent: false,
    };
  }
  return { name: "system", id: thread.thread_id, avatarUrl: null, isAgent: false };
}

function ThreadRow({
  thread,
  names,
  agentsById,
  onOpen,
}: {
  thread: ThreadSummary;
  names: string[];
  agentsById: ReadonlyMap<string, Mentionable>;
  onOpen: () => void;
}) {
  const author = rootAuthor(thread, agentsById);
  return (
    <article
      onClick={onOpen}
      className="group flex cursor-pointer gap-3 px-4 md:px-8 py-2.5 hover:bg-[var(--color-paper-2)]/40 transition-colors"
    >
      <Monogram
        name={author.name}
        id={author.id}
        size={32}
        tone={author.isAgent ? "moss" : "user"}
        avatarUrl={author.avatarUrl}
      />
      <div className="min-w-0 flex-1">
        <header className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
          <span className="font-[var(--font-display)] text-[13.5px] font-bold text-[var(--color-ink)]">
            {author.name}
          </span>
          {author.isAgent && (
            <span className="border border-[var(--color-moss)] px-1 font-[var(--font-mono)] text-[9px] font-bold uppercase tracking-[0.14em] text-[var(--color-moss)]">
              AGENT
            </span>
          )}
          <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-fg-muted)]">
            {clockTime(thread.root?.created_at ?? thread.last_activity_at)}
          </span>
        </header>
        <p className="mt-0.5 truncate text-[13.5px] leading-[1.5] text-[var(--color-ink)]">
          {thread.root ? renderMentions(thread.root.snippet, names) : "…"}
        </p>
        <button
          onClick={(e) => {
            e.stopPropagation();
            onOpen();
          }}
          className="mt-0.5 inline-flex items-center gap-1 font-[var(--font-mono)] text-[11px] font-medium text-[var(--color-moss-deep)] hover:text-[var(--color-moss)] transition-colors"
        >
          <MessageSquareText className="h-3 w-3" />
          {thread.reply_count > 0
            ? `${thread.reply_count} ${thread.reply_count === 1 ? "reply" : "replies"}`
            : "Reply in thread"}
        </button>
      </div>
    </article>
  );
}
