import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  AtSign,
  Bell,
  ChevronDown,
  ChevronRight,
  Paperclip,
  Send,
  Smile,
  X,
} from "lucide-react";
import { Link } from "react-router-dom";
import { useT } from "../../i18n";
import { Button } from "../atoms/Button";
import { Monogram } from "../atoms/Monogram";
import { Markdown } from "../molecules/Markdown";
import { ToolCallLine } from "../molecules/ToolCallLine";
import { WireMcpRequestCard } from "./WireMcpRequestCard";
import { MentionInput } from "../molecules/MentionInput";
import { clockTime, formatMs } from "../../lib/time";
import { cn, insertAtCaret } from "../../lib/utils";
import { useResizableWidth } from "../../hooks/useResizableWidth";
import type {
  Agent,
  ThreadSummary,
  ToolCallEntry,
} from "../../types/api";
import type { Bubble, RootMessage } from "../../lib/foldHistory";
import { DEMO_REPLY_META } from "../../lib/demo";
import { renderMentions } from "../../lib/mentions";

const THREAD_PANEL_WIDTH_KEY = "patom.threadPanel.width";
const THREAD_PANEL_DEFAULT_WIDTH = 360;
const THREAD_PANEL_MIN_WIDTH = 320;
/** Upper bound: never let the panel eat more than ~half the viewport. */
const THREAD_PANEL_MAX_FRACTION = 0.5;

/**
 * Pure renderer. Takes the merged `Bubble[]` from `useThreadView` and the
 * `rootMessage` for the panel header. Holds no merge logic, no dedup, no
 * reconciliation — that all lives in the selector.
 */
export function ThreadPanel({
  channel,
  thread,
  agents,
  bubbles,
  rootMessage,
  showThinking,
  pending,
  focusRequestId,
  onFocusConsumed,
  onReply,
  onClose,
  resizable = false,
}: {
  channel: string;
  thread: ThreadSummary | null;
  agents: Agent[];
  bubbles: Bubble[];
  rootMessage?: RootMessage;
  /** When inline (≥ lg), allow drag-to-resize from the left edge. In the
   *  compact overlay drawer the width is owned by the drawer, so leave off. */
  resizable?: boolean;
  /** Whether to render the "thinking…" placeholder. The selector decides;
   *  this component just paints. */
  showThinking?: boolean;
  /** Composer "Send" spinner — `/prompts` mutation in flight. */
  pending?: boolean;
  /** Deep-link target from `/?turn=<id>`: when a matching bubble is in
   *  `bubbles`, scroll it into view and pulse a highlight. Cleared via
   *  `onFocusConsumed` once handled so a later scroll isn't yanked back. */
  focusRequestId?: string | null;
  onFocusConsumed?: () => void;
  onReply?: (input: { content: string }) => void;
  onClose?: () => void;
}) {
  const { width, dragging, panelRef, handleProps } = useResizableWidth({
    storageKey: THREAD_PANEL_WIDTH_KEY,
    defaultWidth: THREAD_PANEL_DEFAULT_WIDTH,
    minWidth: THREAD_PANEL_MIN_WIDTH,
    maxFraction: THREAD_PANEL_MAX_FRACTION,
  });

  const [reply, setReply] = useState("");
  const replyRef = useRef<HTMLTextAreaElement | null>(null);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const trimmed = reply.trim();
  const sendReply = () => {
    if (!trimmed || pending || !thread) return;
    onReply?.({ content: trimmed });
    setReply("");
    // Pin to bottom after the optimistic bubble has had a chance to render.
    // Two RAFs cover both React commit and the bubble's measured layout.
    requestAnimationFrame(() => {
      requestAnimationFrame(() => {
        const el = scrollRef.current;
        if (el) el.scrollTop = el.scrollHeight;
      });
    });
  };
  const insertAt = () => insertAtCaret(replyRef, reply, setReply, "@");

  // Deep-link scroll: once a bubble with `focusRequestId` lands in the
  // rendered list, scroll the matching <article> into view and hold the
  // highlight on it for a beat. The URL param is cleared immediately via
  // `onFocusConsumed` so a follow-up sidebar click isn't yanked back to
  // the deep-link target; the local highlight outlives the URL clear.
  const lastFocusedRef = useRef<string | null>(null);
  const [highlightId, setHighlightId] = useState<string | null>(null);
  useEffect(() => {
    if (!focusRequestId) return;
    if (lastFocusedRef.current === focusRequestId) return;
    const present = bubbles.some((b) => b.request_id === focusRequestId);
    if (!present) return;
    const el = scrollRef.current?.querySelector<HTMLElement>(
      `[data-request-id="${CSS.escape(focusRequestId)}"]`,
    );
    if (!el) return;
    lastFocusedRef.current = focusRequestId;
    setHighlightId(focusRequestId);
    el.scrollIntoView({ behavior: "smooth", block: "center" });
    onFocusConsumed?.();
    const fadeId = focusRequestId;
    const timer = window.setTimeout(() => {
      setHighlightId((cur) => (cur === fadeId ? null : cur));
    }, 2500);
    return () => window.clearTimeout(timer);
  }, [focusRequestId, bubbles, onFocusConsumed]);

  // Follow the tail only if the reader is already near the bottom — never
  // yank a user who scrolled up to re-read older replies.
  const lastSignature = useRef<string>("");
  const signature = useMemo(() => {
    const tail = bubbles[bubbles.length - 1];
    return `${bubbles.length}|${tail?.key ?? ""}|${tail?.text.length ?? 0}|${
      showThinking ? 1 : 0
    }`;
  }, [bubbles, showThinking]);
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    if (signature === lastSignature.current) return;
    const distanceFromBottom =
      el.scrollHeight - el.scrollTop - el.clientHeight;
    const wasAtBottom = distanceFromBottom < 120;
    lastSignature.current = signature;
    if (wasAtBottom) el.scrollTop = el.scrollHeight;
  }, [signature]);

  return (
    <aside
      ref={panelRef}
      className={cn(
        "relative flex h-full flex-col border-l border-[var(--color-line)] bg-[var(--color-paper)]",
        resizable ? "shrink-0" : "w-full",
        dragging && "select-none",
      )}
      style={resizable ? { width } : undefined}
      aria-label="Thread side panel"
    >
      {resizable && (
        <div
          {...handleProps}
          role="separator"
          aria-orientation="vertical"
          aria-label="Resize thread panel"
          className="group absolute left-0 top-0 z-10 h-full w-1.5 -translate-x-1/2 cursor-col-resize touch-none"
        >
          <span
            className={cn(
              "absolute inset-y-0 left-1/2 w-px -translate-x-1/2 bg-[var(--color-moss)] transition-opacity",
              dragging ? "opacity-100" : "opacity-0 group-hover:opacity-100",
            )}
          />
        </div>
      )}
      <header className="flex items-center justify-between gap-2 border-b border-[var(--color-line)] px-5 py-3">
        <div>
          <div className="font-[var(--font-mono)] text-[10px] uppercase tracking-[0.18em] text-[var(--color-muted-foreground)]">
            Thread
          </div>
          <div className="mt-0.5 font-[var(--font-display)] text-[16px] font-bold text-[var(--color-ink)]">
            Replies in <span className="font-[var(--font-mono)]">#{channel}</span>
          </div>
        </div>
        <Button variant="ghost" size="sm" iconOnly aria-label="Close" onClick={onClose}>
          <X className="h-4 w-4" />
        </Button>
      </header>

      <div ref={scrollRef} className="flex-1 overflow-y-auto scroll-thin">
        {/* Root post */}
        {rootMessage && (
          <article className="flex gap-3 border-b border-[var(--color-line)] px-5 py-4">
            <Monogram
            name={rootMessage.name}
            id={rootMessage.id}
            size={28}
            avatarUrl={rootMessage.avatar_url}
          />
            <div className="min-w-0 flex-1">
              <div className="flex items-baseline gap-2">
                <span className="font-[var(--font-display)] text-[13.5px] font-bold text-[var(--color-ink)]">
                  {rootMessage.name}
                </span>
                <span className="font-[var(--font-mono)] text-[11px] text-[var(--color-fg-muted)]">
                  {clockTime(rootMessage.ts)}
                </span>
              </div>
              <p className="mt-0.5 text-[13.5px] leading-[1.5] text-[var(--color-ink)]">
                {renderMentions(rootMessage.text, agents.map((a) => a.name))}
              </p>
            </div>
          </article>
        )}

        {/* Replies count bar */}
        <div className="flex items-center justify-between border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-1.5">
          <span className="font-[var(--font-mono)] text-[10px] uppercase tracking-[0.18em] text-[var(--color-muted-foreground)]">
            {bubbles.length} {bubbles.length === 1 ? "reply" : "replies"}
          </span>
          <Button variant="ghost" size="xs" iconOnly aria-label="Notifications">
            <Bell className="h-3.5 w-3.5" />
          </Button>
        </div>

        <div className="flex flex-col">
          {bubbles.length === 0 && !showThinking && (
            <p className="px-5 py-6 font-[var(--font-mono)] text-[12px] text-[var(--color-fg-muted)]">
              No replies yet.
            </p>
          )}
          {bubbles.map((b) => {
            const focused = highlightId === b.request_id;
            return b.kind === "human" ? (
              <HumanReplyCard key={b.key} bubble={b} agents={agents} focused={focused} />
            ) : (
              <AgentReplyCard
                key={b.key}
                bubble={b}
                agents={agents}
                thread={thread}
                focused={focused}
              />
            );
          })}
          {showThinking && <ThinkingCard />}
        </div>
      </div>

      {/* Reply composer */}
      <form
        onSubmit={(e) => {
          e.preventDefault();
          sendReply();
        }}
        className="border-t border-[var(--color-line)] bg-[var(--color-paper)] p-3"
      >
        <div
          className={cn(
            "border border-[var(--color-line-strong)] bg-[var(--color-card)] focus-within:ring-2 focus-within:ring-[var(--color-moss)]/15",
            !thread && "opacity-60",
          )}
        >
          <MentionInput
            value={reply}
            onChange={setReply}
            agents={agents}
            mode="thread"
            placeholder="Reply… (use @ to mention an agent)"
            onSubmit={sendReply}
            disabled={!thread || pending}
            textRef={replyRef}
            rows={1}
            maxHeight={140}
          />
          <div className="flex items-center gap-1 border-t border-[var(--color-line)] px-2 py-1">
            <Button type="button" variant="ghost" size="xs" iconOnly aria-label="Mention" onClick={insertAt}>
              <AtSign className="h-3.5 w-3.5" />
            </Button>
            <Button type="button" variant="ghost" size="xs" iconOnly aria-label="Attach">
              <Paperclip className="h-3.5 w-3.5" />
            </Button>
            <Button type="button" variant="ghost" size="xs" iconOnly aria-label="Emoji">
              <Smile className="h-3.5 w-3.5" />
            </Button>
            <Button
              type="submit"
              variant="moss"
              size="sm"
              loading={pending}
              disabled={!trimmed || pending || !thread}
              className="ml-auto"
            >
              {pending ? "sending" : (
                <>
                  Send <Send className="h-3 w-3" strokeWidth={2.5} />
                </>
              )}
            </Button>
          </div>
        </div>
      </form>
    </aside>
  );
}

function HumanReplyCard({
  bubble,
  agents,
  focused,
}: {
  bubble: Bubble;
  agents: Agent[];
  focused?: boolean;
}) {
  const name = bubble.human_name ?? "you";
  const agentNames = agents.map((a) => a.name);
  return (
    <article
      data-request-id={bubble.request_id}
      className={cn(
        "flex gap-3 border-b border-[var(--color-line)] px-5 py-4 transition-colors",
        focused && "bg-[var(--color-moss-tint)]",
      )}
    >
      <Monogram
        name={name}
        id={bubble.human_id ?? name}
        size={22}
        tone="user"
        avatarUrl={bubble.human_avatar_url}
      />
      <div className="min-w-0 flex-1">
        <header className="flex items-baseline gap-2">
          <span className="font-[var(--font-display)] text-[13px] font-bold text-[var(--color-ink)]">
            {name}
          </span>
          <span className="ml-auto font-[var(--font-mono)] text-[11px] text-[var(--color-fg-muted)]">
            {clockTime(bubble.ts)}
          </span>
        </header>
        <p className="mt-0.5 text-[13.5px] leading-[1.5] text-[var(--color-ink)]">
          {renderMentions(bubble.text, agentNames)}
        </p>
      </div>
    </article>
  );
}

/** Terminal failure on a streaming agent bubble. The budget-exceeded case
 *  gets a dedicated, actionable message + link; every other failure shows the
 *  raw reason so nothing is swallowed. */
function FailureBanner({ bubble }: { bubble: Bubble }) {
  const { t } = useT();
  const isBudget = bubble.error_code === "budget_exceeded";
  return (
    <div
      role="alert"
      className="mt-2 flex items-start gap-2 border border-[var(--color-rose)] bg-[var(--color-rose-soft)] px-3 py-2 text-[12px] text-[var(--color-rose)]"
    >
      <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" strokeWidth={1.75} />
      <div className="min-w-0">
        {isBudget ? (
          <>
            <span>{t("chat.error.budget_exceeded")}</span>{" "}
            <Link to="/settings/billing" className="font-medium underline">
              {t("settings.nav.billing")}
            </Link>
          </>
        ) : (
          <span>{bubble.error}</span>
        )}
      </div>
    </div>
  );
}

function AgentReplyCard({
  bubble,
  agents,
  thread,
  focused,
}: {
  bubble: Bubble;
  agents: Agent[];
  /** Thread context — passed to each wire-MCP card so the OAuth start
   *  flow can populate `resume_ctx` for the server-side auto-continue. */
  thread: ThreadSummary | null;
  focused?: boolean;
}) {
  // Demo metas pre-populate reasoning + tool calls when the bubble doesn't
  // yet carry them — keeps the design-reference panel honest without
  // coupling demo fixtures to live wire data.
  const meta = DEMO_REPLY_META[bubble.key];
  const tools: (ToolCallEntry & { durationMs?: number })[] =
    bubble.tool_calls.length > 0
      ? bubble.tool_calls
      : meta?.tools.map((t, i) => ({
          call_id: `${bubble.key}:${i}`,
          name: t.name,
          input: t.args,
          output: undefined,
          status: "ok" as const,
          durationMs: t.durationMs,
        })) ?? [];
  const reasoning = bubble.reasoning || meta?.reasoning || "";
  const tokens = meta?.tokens ?? 0;
  const durationMs = meta?.durationMs ?? 0;
  const hasMeta = tools.length > 0 || reasoning.length > 0 || tokens > 0;

  const [open, setOpen] = useState(meta?.expanded ?? false);
  const isLive = bubble.phase !== "persisted";
  const agent = bubble.agent_id
    ? (agents.find((a) => a.id === bubble.agent_id) ?? null)
    : null;
  const agentName = agent?.name ?? bubble.agent_name ?? "agent";
  const agentMonogramId = agent?.id ?? bubble.agent_id ?? "agent";

  return (
    <article
      data-request-id={bubble.request_id}
      className={cn(
        "border-b border-[var(--color-line)] px-5 py-4 transition-colors",
        focused && "bg-[var(--color-moss-tint)]",
      )}
    >
      <header className="flex items-center gap-2">
        <Monogram
          name={agentName}
          id={agentMonogramId}
          size={22}
          tone="moss"
          avatarUrl={agent?.avatar_url}
        />
        <span className="font-[var(--font-display)] text-[13px] font-bold text-[var(--color-ink)]">
          {agentName}
        </span>
        <span className="border border-[var(--color-moss)] px-1 font-[var(--font-mono)] text-[9.5px] font-bold uppercase tracking-[0.14em] text-[var(--color-moss)]">
          AGENT
        </span>
        <span className="ml-auto font-[var(--font-mono)] text-[11px] text-[var(--color-fg-muted)]">
          {clockTime(bubble.ts)}
        </span>
      </header>

      <div className="mt-1.5 text-[13px] leading-[1.5] text-[var(--color-ink)]">
        {bubble.text ? (
          <Markdown text={bubble.text} className="text-[13px]" />
        ) : isLive && !bubble.error ? (
          <ThinkingIndicator />
        ) : null}
      </div>

      {bubble.error ? <FailureBanner bubble={bubble} /> : null}

      {bubble.wire_requests.length > 0 && (
        <div className="mt-1">
          {bubble.wire_requests.map((req) => (
            <WireMcpRequestCard
              key={`wire:${req.catalog_id}`}
              entry={req}
              sessionId={thread?.root_session_id ?? null}
              agentId={bubble.agent_id ?? null}
            />
          ))}
        </div>
      )}

      {hasMeta && (
        <button
          onClick={() => setOpen((v) => !v)}
          className="mt-2.5 flex w-full items-center gap-2 border border-[var(--color-line)] bg-[var(--color-paper-2)] px-2.5 py-1 font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)] hover:text-[var(--color-ink)] transition-colors"
        >
          {open ? (
            <ChevronDown className="h-3 w-3" />
          ) : (
            <ChevronRight className="h-3 w-3" />
          )}
          <span className="font-medium">reasoning</span>
          <span className="text-[var(--color-line-2)]">|</span>
          <span>{tools.length} tools</span>
          <span className="text-[var(--color-line-2)]">|</span>
          <span>
            {tokens >= 1000 ? `${(tokens / 1000).toFixed(1)}k` : tokens} tok
          </span>
          <span className="text-[var(--color-line-2)]">|</span>
          <span className="ml-auto">{formatMs(durationMs)}</span>
        </button>
      )}

      {open && (
        <div className="mt-2 space-y-3 border border-[var(--color-moss-soft)] bg-[var(--color-moss-tint)] p-3">
          {reasoning && (
            <div>
              <div className="mb-1.5 font-[var(--font-mono)] text-[10px] uppercase tracking-[0.16em] text-[var(--color-moss-deep)]">
                Reasoning
              </div>
              <p className="font-[var(--font-sans)] text-[12px] italic leading-[1.55] text-[var(--color-ink-2)] whitespace-pre-wrap">
                {reasoning}
              </p>
            </div>
          )}
          {tools.length > 0 && (
            <div>
              <div
                className={cn(
                  "mb-1.5 font-[var(--font-mono)] text-[10px] uppercase tracking-[0.16em] text-[var(--color-moss-deep)]",
                  reasoning && "border-t border-[var(--color-moss-soft)] pt-2",
                )}
              >
                Tool Calls
              </div>
              <div className="space-y-1">
                {tools.map((t) => (
                  <ToolCallLine
                    key={t.call_id}
                    call={{
                      call_id: t.call_id,
                      name: t.name,
                      input: t.input,
                      output: t.output,
                      status: t.status,
                    }}
                    durationMs={(t as { durationMs?: number }).durationMs}
                  />
                ))}
              </div>
            </div>
          )}
        </div>
      )}
    </article>
  );
}

function ThinkingCard() {
  return (
    <article className="border-b border-[var(--color-line)] px-5 py-4">
      <ThinkingIndicator />
    </article>
  );
}

function ThinkingIndicator() {
  return (
    <div
      className="inline-flex items-center gap-1.5 font-[var(--font-mono)] text-[11.5px] text-[var(--color-muted-foreground)]"
      aria-label="Agent is thinking"
    >
      <span className="flex gap-0.5">
        <span className="h-1 w-1 animate-pulse rounded-full bg-[var(--color-moss)] [animation-delay:0ms]" />
        <span className="h-1 w-1 animate-pulse rounded-full bg-[var(--color-moss)] [animation-delay:150ms]" />
        <span className="h-1 w-1 animate-pulse rounded-full bg-[var(--color-moss)] [animation-delay:300ms]" />
      </span>
      <span>thinking…</span>
    </div>
  );
}
