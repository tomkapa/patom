import { useEffect, useMemo, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { ChatLayout } from "../components/templates/ChatLayout";
import { ChannelHeader } from "../components/organisms/ChannelHeader";
import { Composer } from "../components/organisms/Composer";
import { MessageList } from "../components/organisms/MessageList";
import { Sidebar } from "../components/organisms/Sidebar";
import { ThreadPanel } from "../components/organisms/ThreadPanel";
import { OrgSwitcher } from "../components/organisms/OrgSwitcher";
import { useAgents } from "../hooks/useAgents";
import { useActiveOrg } from "../hooks/useMe";
import { useAuthStore } from "../stores/authStore";
import { useThreads } from "../hooks/useThreads";
import { useThreadStream } from "../hooks/useThreadStream";
import { useSubmitPrompt } from "../hooks/useSubmitPrompt";
import { useThreadView } from "../hooks/useThreadView";
import { useTurnDetail } from "../hooks/useAgentLogs";
import { useThreadStore } from "../stores/threadStore";
import {
  DEMO_AGENTS,
  DEMO_HISTORY,
  DEMO_HUMAN_POSTER,
  DEMO_REPLIES,
  DEMO_THREADS,
  DEMO_USER,
} from "../lib/demo";
import { decodeBody } from "../lib/chatBody";
import type { Bubble, Poster, RootMessage } from "../lib/foldHistory";
import { uuidv7 } from "../lib/utils";
import { prefixMention } from "../lib/mentions";
import { useIsWide } from "../hooks/useMediaQuery";
import { useT } from "../i18n";

const CHANNEL = "general";

/** Force demo fixtures via `?demo=1` (or empty backend). */
function isDemoMode(): boolean {
  if (typeof window === "undefined") return false;
  return new URLSearchParams(window.location.search).get("demo") === "1";
}

export function ChatView() {
  const forcedDemo = isDemoMode();
  const [selectedRoot, setSelectedRoot] = useState<string | null>(
    forcedDemo ? DEMO_THREADS[0]!.root_request_id : null,
  );
  // When set, the channel feed is filtered to threads where this agent is
  // the human's first recipient (`first_agent.id === selectedAgentId`).
  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);
  // On wide screens the thread panel is a default-visible side column; on
  // compact screens it's a drill-in overlay that stays closed until the
  // reader opens a thread (otherwise it would cover the feed on load).
  const isWide = useIsWide();
  const [showPanel, setShowPanel] = useState(isWide);

  // Deep-link from the memory pane: `/?turn=<request_id>` opens the thread
  // that owns the turn and scrolls the matching bubble into view. We
  // resolve turn → root_request_id via the turn-detail endpoint (the
  // memory rows / events only carry the turn id, not the root).
  const [searchParams, setSearchParams] = useSearchParams();
  const focusTurnId = searchParams.get("turn");
  const turnDetailQ = useTurnDetail(forcedDemo ? null : focusTurnId);
  useEffect(() => {
    const rootId = turnDetailQ.data?.turn.root_request_id;
    if (!rootId) return;
    setSelectedRoot(rootId);
    setShowPanel(true);
  }, [turnDetailQ.data?.turn.root_request_id]);
  const clearFocusTurn = () => {
    if (!focusTurnId) return;
    const next = new URLSearchParams(searchParams);
    next.delete("turn");
    setSearchParams(next, { replace: true });
  };

  const agentsQ = useAgents();
  const threadsQ = useThreads();
  const submit = useSubmitPrompt();
  const me = useAuthStore((s) => s.me);
  const activeOrg = useActiveOrg();
  const addPending = useThreadStore((s) => s.addPending);
  const attachRequestId = useThreadStore((s) => s.attachRequestId);
  const removePending = useThreadStore((s) => s.removePending);

  // Demo fixtures are opt-in via `?demo=1`. An empty backend renders the
  // real (empty) UI — never fall back to fixtures, or replies get silently
  // dropped by the `if (isDemo) return;` guards below.
  const isDemo = forcedDemo;
  const agents = isDemo ? DEMO_AGENTS : (agentsQ.data ?? []);
  const threads = isDemo ? DEMO_THREADS : (threadsQ.data ?? []);
  const poster = isDemo
    ? { ...DEMO_HUMAN_POSTER, avatar_url: null }
    : {
        name: me?.user.display_name ?? me?.user.email ?? DEMO_USER.name,
        id: me?.user.id ?? DEMO_USER.id,
        avatar_url: me?.user.avatar_url ?? null,
      };

  // SSE stream + view selector are skipped in demo mode by passing null.
  const liveRootId = isDemo ? null : selectedRoot;
  useThreadStream(liveRootId);
  const view = useThreadView(liveRootId, agents, poster);
  const demoView = useMemo(
    () => (isDemo ? buildDemoView(poster) : null),
    [isDemo, poster],
  );
  const bubbles = isDemo ? (demoView?.bubbles ?? []) : view.bubbles;
  const rootMessage = isDemo ? demoView?.rootMessage : view.rootMessage;
  const showThinking = isDemo ? false : view.showThinking;

  const defaultAgent = useMemo(
    () => agents.find((a) => a.is_default) ?? agents[0],
    [agents],
  );

  const visibleThreads = useMemo(
    () =>
      selectedAgentId
        ? threads.filter((t) => t.first_agent.id === selectedAgentId)
        : threads,
    [threads, selectedAgentId],
  );

  const selectedAgent = useMemo(
    () =>
      selectedAgentId
        ? (agents.find((a) => a.id === selectedAgentId) ?? null)
        : null,
    [agents, selectedAgentId],
  );

  const selectedThread = useMemo(
    () => threads.find((t) => t.root_request_id === selectedRoot) ?? null,
    [threads, selectedRoot],
  );

  const onSubmit = async (input: { content: string; agent_id?: string }) => {
    if (isDemo) return;
    const agent_id = selectedAgentId ?? input.agent_id ?? defaultAgent?.id;
    if (!agent_id) return;
    const res = await submit.mutateAsync({
      content: input.content,
      agent_id,
    });
    setSelectedRoot(res.request_id);
  };

  const { t } = useT();

  const onThreadReply = async (input: { content: string }) => {
    if (isDemo || !selectedThread) return;
    const root = selectedThread.root_request_id;
    // Auto-prefix the receiver's @handle so the optimistic bubble matches
    // what the fold renders for the persisted row.
    const text = prefixMention(input.content, selectedThread.first_agent.name);
    const idempotency_key = uuidv7();
    addPending(root, {
      idempotency_key,
      text,
      ts: new Date().toISOString(),
    });
    try {
      const res = await submit.mutateAsync({
        content: text,
        session_id: selectedThread.root_session_id,
        idempotency_key,
      });
      // Stamps the request_id so the persisted echo can dedupe this entry.
      attachRequestId(root, idempotency_key, res.request_id);
    } catch (e) {
      // Withdraw the optimistic bubble; the user can retry.
      removePending(root, idempotency_key);
      throw e;
    }
  };

  // Auto-resume is now server-driven: the OAuth callback enqueues the
  // synthetic continuation prompt itself, so every channel adapter
  // (web, Slack, future Lark) gets the resume for free. The card
  // flipping to its "connected" state is still driven by the
  // useMcpServers poll; the agent's next response arrives via the
  // existing thread stream.

  // Feed label: agent DMs read `dm/<name>`, the channel reads bare
  // `general` (the `#` prefix is added only where a header wants it).
  const channelLabel = selectedAgent ? `dm/${selectedAgent.name}` : CHANNEL;

  return (
    <ChatLayout
      title={selectedAgent ? channelLabel : `#${CHANNEL}`}
      sidebar={
        <Sidebar
          workspace={activeOrg?.name ?? "Patom"}
          threads={threads}
          agents={agents}
          selectedChannel={selectedAgentId ? "" : CHANNEL}
          selectedAgentId={selectedAgentId}
          onSelectChannel={() => {
            setSelectedAgentId(null);
            setSelectedRoot(null);
          }}
          onSelectAgent={(id) => {
            setSelectedAgentId(id);
            setSelectedRoot(null);
          }}
          orgSwitcher={me ? <OrgSwitcher /> : undefined}
        />
      }
      main={
        <>
          <ChannelHeader channel={channelLabel} agents={agents} />
          <MessageList
            threads={visibleThreads}
            channel={channelLabel}
            userName={poster.name}
            humanPoster={poster}
            onOpenThread={(rootId) => {
              setSelectedRoot(rootId);
              setShowPanel(true);
            }}
          />
          <Composer
            agents={agents}
            mode={selectedAgent ? "dm" : "channel"}
            dmAgent={selectedAgent ?? undefined}
            channel={CHANNEL}
            pending={submit.isPending}
            disabled={agentsQ.isLoading && !isDemo}
            onSubmit={onSubmit}
          />
        </>
      }
      panelOpen={showPanel}
      onPanelClose={() => setShowPanel(false)}
      panel={
        <ThreadPanel
          channel={CHANNEL}
          thread={selectedThread}
          agents={agents}
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
        key: `h:${m.session_id}:${m.seq}`,
        request_id: `demo:${m.session_id}:${m.seq}`,
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
      key: `h:${m.session_id}:${m.seq}`,
      request_id: `demo:${m.session_id}:${m.seq}`,
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
