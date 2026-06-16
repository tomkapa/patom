import { useEffect, useMemo, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { ChatLayout } from "../components/templates/ChatLayout";
import { ChannelHeader } from "../components/organisms/ChannelHeader";
import { Composer, type ComposerSubmit } from "../components/organisms/Composer";
import { MessageList, type WelcomeContext } from "../components/organisms/MessageList";
import { Sidebar, dmKey } from "../components/organisms/Sidebar";
import { ThreadPanel } from "../components/organisms/ThreadPanel";
import { ChannelDialog } from "../components/organisms/ChannelDialog";
import { matchMentions } from "../components/molecules/MentionInput";
import { useChannels } from "../hooks/useChannels";
import { useActiveOrg } from "../hooks/useMe";
import { useAuthStore } from "../stores/authStore";
import { useThreads } from "../hooks/useThreads";
import { useThreadStream } from "../hooks/useThreadStream";
import { useSubmitPrompt } from "../hooks/useSubmitPrompt";
import { useThreadView } from "../hooks/useThreadView";
import { useTurnDetail } from "../hooks/useAgentLogs";
import { useThreadStore } from "../stores/threadStore";
import { useRoster, tagRef } from "../hooks/useRoster";
import {
  DEMO_HISTORY,
  DEMO_HUMAN_POSTER,
  DEMO_REPLIES,
  DEMO_ROSTER,
  DEMO_THREADS,
  DEMO_USER,
} from "../lib/demo";
import { decodeBody } from "../lib/chatBody";
import { ApiError } from "../lib/errors";
import { track } from "../lib/analytics";
import type { Bubble, Poster, RootMessage } from "../lib/foldHistory";
import { uuidv7 } from "../lib/utils";
import { useIsWide } from "../hooks/useMediaQuery";
import { useT } from "../i18n";
import type { Attachment, Channel, Mentionable } from "../types/api";

/** Demo mode has no backend; show a single read-only #general so the feed
 *  renders. Real channels come from `useChannels`. */
const DEMO_CHANNELS: Channel[] = [
  {
    id: "general",
    name: "general",
    system: true,
    can_manage: false,
    created_at: new Date(0).toISOString(),
    archived_at: null,
  },
];

/** Force demo fixtures via `?demo=1` (or empty backend). */
function isDemoMode(): boolean {
  if (typeof window === "undefined") return false;
  return new URLSearchParams(window.location.search).get("demo") === "1";
}

export function ChatView() {
  const forcedDemo = isDemoMode();
  const [selectedRoot, setSelectedRoot] = useState<string | null>(
    forcedDemo ? DEMO_THREADS[0]!.thread_id : null,
  );
  // When set, the view shows the 1:1 conversation with this colleague —
  // human or agent, both are DM counterparts (Slack parity). Mutually
  // exclusive with `selectedChannelId`.
  const [selectedDm, setSelectedDm] = useState<Mentionable | null>(null);
  // The selected channel. `null` until channels load; an effect defaults it
  // to the first channel (#general).
  const [selectedChannelId, setSelectedChannelId] = useState<string | null>(
    forcedDemo ? DEMO_CHANNELS[0]!.id : null,
  );
  // Create / manage channel dialog. `manageChannel` is set for the manage
  // variant; `null` + `dialogOpen` is the create variant.
  const [dialogOpen, setDialogOpen] = useState(false);
  const [manageChannel, setManageChannel] = useState<Channel | null>(null);
  // On wide screens the thread panel is a default-visible side column; on
  // compact screens it's a drill-in overlay that stays closed until the
  // reader opens a thread (otherwise it would cover the feed on load).
  const isWide = useIsWide();
  const [showPanel, setShowPanel] = useState(isWide);
  // Inline composer error — surfaces a 429 (over monthly spend budget) from
  // POST /prompts instead of swallowing it as a generic mutation rejection.
  const [composerError, setComposerError] = useState<string | null>(null);
  // Text dropped into the composer from outside (the welcome CTA). The
  // bumped nonce lets the same text be re-filled on a repeat click; the
  // user reviews and sends — nothing posts on their behalf.
  const [prefill, setPrefill] = useState<{ text: string; nonce: number } | null>(
    null,
  );

  // Deep-link from the memory pane: `/?turn=<request_id>` opens the thread
  // that owns the turn and scrolls the matching bubble into view. The
  // turn-detail wire carries the thread id directly (threads are keyed by
  // `thread_id`, not request ids).
  const [searchParams, setSearchParams] = useSearchParams();
  const focusTurnId = searchParams.get("turn");
  const turnDetailQ = useTurnDetail(forcedDemo ? null : focusTurnId);
  useEffect(() => {
    const threadId = turnDetailQ.data?.turn.thread_id;
    if (!threadId) return;
    setSelectedRoot(threadId);
    setShowPanel(true);
  }, [turnDetailQ.data?.turn.thread_id]);
  const clearFocusTurn = () => {
    if (!focusTurnId) return;
    const next = new URLSearchParams(searchParams);
    next.delete("turn");
    setSearchParams(next, { replace: true });
  };

  const channelsQ = useChannels();
  const channels = forcedDemo ? DEMO_CHANNELS : (channelsQ.data ?? []);
  const isDemo = forcedDemo;

  // The system #general enrolls every org member, so its roster IS the
  // workspace's humans — the source for the DM sidebar and DM composers.
  const general = channels.find((c) => c.system) ?? channels[0] ?? null;
  const selectedChannel =
    channels.find((c) => c.id === selectedChannelId) ?? null;

  // Roster of the open context: the channel's members (or, in DM mode, the
  // whole workspace via #general) plus every agent (org-global). This scopes
  // the composer, mentions, header counts, and name resolution — NOT the DM
  // sidebar, which is workspace-wide (see `dmSource` below).
  const rosterChannelId = isDemo
    ? null
    : selectedDm
      ? (general?.id ?? null)
      : (selectedChannelId ?? general?.id ?? null);
  const liveRoster = useRoster(rosterChannelId);
  const me = useAuthStore((s) => s.me);
  const contextRoster = isDemo ? DEMO_ROSTER : liveRoster.roster;
  const agents = useMemo(
    () => contextRoster.filter((m) => m.kind === "agent"),
    [contextRoster],
  );
  const humans = useMemo(
    () => contextRoster.filter((m) => m.kind === "human"),
    [contextRoster],
  );

  // The DM sidebar is workspace-wide and must NOT narrow to the open channel,
  // so it sources its roster from #general directly — selecting a channel
  // leaves the people you can DM unchanged.
  const dmLiveRoster = useRoster(isDemo ? null : (general?.id ?? null));
  const dmSource = isDemo ? DEMO_ROSTER : dmLiveRoster.roster;
  // DM sidebar: everyone except the viewer (you don't DM yourself).
  const dmRoster = useMemo(
    () => [
      ...dmSource.filter((m) => m.kind === "human" && m.id !== me?.user.id),
      ...dmSource.filter((m) => m.kind === "agent"),
    ],
    [dmSource, me?.user.id],
  );

  // DM mode reads the pair's conversation; channel mode reads the channel.
  const threadsQ = useThreads(
    isDemo ? null : selectedDm ? null : selectedChannelId,
    isDemo || !selectedDm ? null : tagRef(selectedDm),
  );
  const submit = useSubmitPrompt();
  const activeOrg = useActiveOrg();
  const addPending = useThreadStore((s) => s.addPending);
  const attachOutcome = useThreadStore((s) => s.attachOutcome);
  const removePending = useThreadStore((s) => s.removePending);

  const threads = isDemo ? DEMO_THREADS : (threadsQ.data ?? []);

  // Default the channel selection to the first channel (#general) once the
  // list loads, unless the reader is already in a channel or a DM.
  useEffect(() => {
    if (selectedChannelId || selectedDm) return;
    const first = channels[0];
    if (first) setSelectedChannelId(first.id);
  }, [channels, selectedChannelId, selectedDm]);
  const poster: Poster = isDemo
    ? { ...DEMO_HUMAN_POSTER, avatar_url: null }
    : {
        name: me?.user.display_name ?? me?.user.email ?? DEMO_USER.name,
        id: me?.user.id ?? DEMO_USER.id,
        avatar_url: me?.user.avatar_url ?? null,
      };

  // SSE stream + view selector are skipped in demo mode by passing null.
  const liveRootId = isDemo ? null : selectedRoot;
  useThreadStream(liveRootId);
  const view = useThreadView(liveRootId, contextRoster, poster);
  const demoView = useMemo(
    () => (isDemo ? buildDemoView(poster) : null),
    [isDemo, poster],
  );
  const bubbles = isDemo ? (demoView?.bubbles ?? []) : view.bubbles;
  const rootMessage = isDemo ? demoView?.rootMessage : view.rootMessage;
  const showThinking = isDemo ? false : view.showThinking;

  const selectedThread = useMemo(
    () => threads.find((t) => t.thread_id === selectedRoot) ?? null,
    [threads, selectedRoot],
  );

  const channelName = selectedChannel?.name ?? "general";

  const { t } = useT();

  // First-contact orientation: in the system channel (#general) with an
  // empty timeline — i.e. right after onboarding — offer the team intro.
  // The CTA prefills a message to the Recruiter; it does not auto-post.
  const welcome: WelcomeContext | null =
    !isDemo && !selectedDm && selectedChannel?.system === true
      ? {
          userName:
            me?.user.display_name ?? me?.user.email ?? "there",
          onIntro: () =>
            setPrefill((p) => ({
              text: t("chat.welcome.prefill"),
              nonce: (p?.nonce ?? 0) + 1,
            })),
        }
      : null;

  // Engagement signal — one event each time a thread is opened (sidebar
  // click, deep-link, or just-sent root). Demo mode is excluded.
  useEffect(() => {
    if (isDemo || !selectedRoot) return;
    track("thread_opened");
  }, [selectedRoot, isDemo]);

  // A 429 from POST /prompts means the workspace is over its monthly budget —
  // surface it inline rather than as a generic failure. Returns true if handled.
  const handleBudgetExceeded = (e: unknown): boolean => {
    if (e instanceof ApiError && e.status === 429) {
      setComposerError(t("chat.error.billing_exceeded"));
      // Monetization signal — the workspace hit its monthly spend cap.
      track("budget_warning_shown");
      return true;
    }
    // 402 from POST /prompts means the workspace is out of free credit (#154) —
    // surface the top-up / bring-your-own-key prompt inline.
    if (e instanceof ApiError && e.status === 402) {
      setComposerError(t("chat.error.out_of_credit"));
      track("out_of_credit_shown");
      return true;
    }
    return false;
  };

  // Post to the channel timeline (or start a DM thread). Tags are whoever
  // was @-mentioned — agents among them get invoked; none is required.
  const onSubmit = async (input: ComposerSubmit) => {
    if (isDemo) return;
    setComposerError(null);
    try {
      const res = await submit.mutateAsync({
        content: input.content,
        tags: input.tags.map(tagRef),
        attachments: input.attachments,
        // In channel mode the new thread is stamped with the channel; a DM
        // names its counterpart so the BE files it as the pair's thread.
        ...(selectedDm
          ? { counterpart: tagRef(selectedDm) }
          : { channel_id: selectedChannelId ?? undefined }),
      });
      setSelectedRoot(res.thread_id);
      if (!isWide) setShowPanel(true);
    } catch (e) {
      if (handleBudgetExceeded(e)) return;
      throw e;
    }
  };

  // Reply inside the open thread. The text goes exactly as typed — no
  // implicit @-prefix; the backend routes DM replies to the counterpart and
  // leaves untagged channel replies as plain posts.
  const onThreadReply = async (input: {
    content: string;
    attachments?: Attachment[];
  }) => {
    if (isDemo || !selectedThread) return;
    const threadId = selectedThread.thread_id;
    const idempotency_key = uuidv7();
    addPending(threadId, {
      idempotency_key,
      text: input.content,
      attachments: input.attachments,
      ts: new Date().toISOString(),
    });
    setComposerError(null);
    try {
      const res = await submit.mutateAsync({
        content: input.content,
        tags: matchMentions(input.content, contextRoster).map(tagRef),
        thread_id: threadId,
        attachments: input.attachments,
        idempotency_key,
      });
      // Stamp the outcome so the persisted echo can dedupe this entry and
      // the "thinking…" placeholder knows whether anyone will reply.
      attachOutcome(threadId, idempotency_key, {
        request_id: res.request_id,
        triggered: res.triggered_agent_ids.length > 0,
      });
    } catch (e) {
      // Withdraw the optimistic bubble; the user can retry.
      removePending(threadId, idempotency_key);
      if (handleBudgetExceeded(e)) return;
      throw e;
    }
  };

  // Feed label: DMs read `dm/<name>`, a channel reads its bare name
  // (the `#` prefix is added only where a header wants it).
  const channelLabel = selectedDm ? `dm/${selectedDm.name}` : channelName;

  return (
    <>
    <ChatLayout
      title={selectedDm ? channelLabel : `#${channelName}`}
      sidebar={
        <Sidebar
          workspace={activeOrg?.name ?? "Patom"}
          channels={channels}
          dms={dmRoster}
          selectedChannelId={selectedDm ? null : selectedChannelId}
          selectedDmKey={selectedDm ? dmKey(selectedDm) : null}
          onSelectChannel={(id) => {
            setSelectedDm(null);
            setSelectedChannelId(id);
            setSelectedRoot(null);
          }}
          onSelectDm={(m) => {
            setSelectedDm(m);
            setSelectedRoot(null);
          }}
          onAddChannel={() => {
            setManageChannel(null);
            setDialogOpen(true);
          }}
          onManageChannel={(c) => {
            setManageChannel(c);
            setDialogOpen(true);
          }}
        />
      }
      main={
        <>
          <ChannelHeader
            channel={channelLabel}
            memberCount={humans.length}
            agentCount={agents.length}
          />
          <MessageList
            threads={threads}
            roster={contextRoster}
            channel={channelLabel}
            welcome={welcome}
            onOpenThread={(rootId) => {
              setSelectedRoot(rootId);
              setShowPanel(true);
            }}
          />
          {composerError ? (
            <div
              role="alert"
              className="mx-3 mt-2 flex items-center justify-between gap-2 border border-[var(--color-rose)] bg-[var(--color-rose-soft)] px-3 py-2 text-[12px] text-[var(--color-rose)]"
            >
              <span className="min-w-0">{composerError}</span>
              <button
                type="button"
                onClick={() => setComposerError(null)}
                aria-label="Dismiss"
                className="shrink-0 font-[var(--font-mono)] text-[11px] uppercase tracking-[0.06em] hover:opacity-70"
              >
                ✕
              </button>
            </div>
          ) : null}
          <Composer
            roster={contextRoster}
            mode={selectedDm ? "dm" : "channel"}
            dmCounterpart={selectedDm ?? undefined}
            channel={channelName}
            pending={submit.isPending}
            disabled={liveRoster.isLoading && !isDemo}
            prefill={prefill}
            onSubmit={onSubmit}
          />
        </>
      }
      panelOpen={showPanel}
      onPanelClose={() => setShowPanel(false)}
      panel={
        <ThreadPanel
          channel={channelName}
          resizable={isWide}
          thread={selectedThread}
          roster={contextRoster}
          bubbles={bubbles}
          rootMessage={rootMessage}
          showThinking={showThinking}
          pending={submit.isPending}
          focusRequestId={focusTurnId}
          onFocusConsumed={clearFocusTurn}
          onReply={onThreadReply}
          onClose={() => setShowPanel(false)}
        />
      }
    />
    {!isDemo && (
      <ChannelDialog
        open={dialogOpen}
        channel={manageChannel ?? undefined}
        onClose={() => setDialogOpen(false)}
        onArchived={(id) => {
          // Deselect an archived channel; the default-select effect falls back
          // to the first remaining channel.
          if (selectedChannelId === id) setSelectedChannelId(null);
        }}
      />
    )}
    </>
  );
}

/**
 * Demo-mode view synthesis. The fixtures use the legacy `content: string`
 * shape and don't carry send_message tool calls, so they bypass `foldHistory`
 * entirely — each demo reply maps to one bubble whose `key` matches
 * `DEMO_REPLY_META` (so reasoning / tool / token decorations attach).
 */
function buildDemoView(poster: Poster): {
  bubbles: Bubble[];
  rootMessage: RootMessage | undefined;
} {
  const first = DEMO_HISTORY[0];
  const rootMessage: RootMessage | undefined = first
    ? {
        name: poster.name,
        id: poster.id,
        avatar_url: poster.avatar_url,
        ts: first.created_at,
        text: decodeBody(first.body).text,
      }
    : undefined;

  const bubbles: Bubble[] = DEMO_REPLIES.map((m) => {
    const text = decodeBody(m.body).text;
    if (m.sender.kind === "agent") {
      return {
        kind: "agent",
        key: `h:${m.seq}`,
        request_id: `demo:${m.seq}`,
        client_key: null,
        agent_id: m.sender.agent_id,
        agent_name: null,
        human_name: null,
        human_id: null,
        human_avatar_url: null,
        ts: m.created_at,
        text,
        reasoning: "",
        tool_calls: [],
        wire_requests: [],
        phase: "persisted",
      };
    }
    return {
      kind: "human",
      key: `h:${m.seq}`,
      request_id: `demo:${m.seq}`,
      client_key: null,
      agent_id: null,
      agent_name: null,
      human_name: poster.name,
      human_id: poster.id,
      human_avatar_url: poster.avatar_url,
      ts: m.created_at,
      text,
      reasoning: "",
      tool_calls: [],
      wire_requests: [],
      phase: "persisted",
    };
  });

  return { bubbles, rootMessage };
}
