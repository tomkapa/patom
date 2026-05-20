import { useMemo, useState } from "react";
import { ChatLayout } from "../components/templates/ChatLayout";
import { ChannelHeader } from "../components/organisms/ChannelHeader";
import { Composer } from "../components/organisms/Composer";
import { MessageList } from "../components/organisms/MessageList";
import { Sidebar } from "../components/organisms/Sidebar";
import { ThreadPanel } from "../components/organisms/ThreadPanel";
import { MenuRail } from "../components/organisms/MenuRail";
import { OrgSwitcher } from "../components/organisms/OrgSwitcher";
import { useAgents } from "../hooks/useAgents";
import { useActiveOrg } from "../hooks/useMe";
import { useAuthStore } from "../stores/authStore";
import { useThreads } from "../hooks/useThreads";
import { useThreadStream } from "../hooks/useThreadStream";
import { useSubmitPrompt } from "../hooks/useSubmitPrompt";
import { useThreadView } from "../hooks/useThreadView";
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
import type { Bubble, RootMessage } from "../lib/foldHistory";
import { uuidv7 } from "../lib/utils";
import { prefixMention } from "../lib/mentions";
import { useT } from "../i18n";
import type { McpWireRequest } from "../types/api";

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
  const [showPanel, setShowPanel] = useState(true);

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
  const poster = isDemo ? DEMO_HUMAN_POSTER : DEMO_USER;

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

  // Auto-resume after the inline wire-MCP card flips to wired. The
  // card guards against double-firing (initiated-in-session +
  // sessionStorage marker), so we just dispatch a reply on the same
  // thread and let the agent's next turn pick up the new capability.
  const onWireConnected = (req: McpWireRequest) => {
    void onThreadReply({
      content: t("thread.wireRequest.resumePrompt", { name: req.display_name }),
    });
  };

  return (
    <ChatLayout
      rail={<MenuRail />}
      sidebar={
        <Sidebar
          workspace={activeOrg?.name ?? "Relay"}
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
          <ChannelHeader
            channel={selectedAgent ? `dm/${selectedAgent.name}` : CHANNEL}
            agents={agents}
          />
          <MessageList
            threads={visibleThreads}
            channel={selectedAgent ? `dm/${selectedAgent.name}` : CHANNEL}
            userName={me?.user.display_name ?? me?.user.email ?? DEMO_USER.name}
            humanPoster={isDemo ? DEMO_HUMAN_POSTER : undefined}
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
      panel={
        showPanel ? (
          <ThreadPanel
            channel={CHANNEL}
            thread={selectedThread}
            agents={agents}
            bubbles={bubbles}
            rootMessage={rootMessage}
            showThinking={showThinking}
            pending={submit.isPending}
            onReply={onThreadReply}
            onWireConnected={onWireConnected}
            onClose={() => setShowPanel(false)}
          />
        ) : null
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
function buildDemoView(poster: { name: string; id: string }): {
  bubbles: Bubble[];
  rootMessage: RootMessage | undefined;
} {
  const first = DEMO_HISTORY[0];
  const rootMessage: RootMessage | undefined = first
    ? {
        name: poster.name,
        id: poster.id,
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
