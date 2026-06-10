// Public, logged-out scripted demo at `/demo` (mounted outside <Protected>).
// It composes the real chat presentational components from playback state and
// imports ZERO data/mutation hooks and nothing from lib/api — so a cold visitor
// fires no request and never 401-bounces to sign-in. A runtime fetch-guard
// makes that guarantee loud if a future edit ever reintroduces a live hook.

import { useEffect, useMemo } from "react";
import { ChatLayout } from "../components/templates/ChatLayout";
import { ChannelHeader } from "../components/organisms/ChannelHeader";
import { Composer } from "../components/organisms/Composer";
import { MessageList } from "../components/organisms/MessageList";
import { Sidebar, dmKey } from "../components/organisms/Sidebar";
import { ThreadPanel } from "../components/organisms/ThreadPanel";
import { DemoPlaybackBar } from "../components/organisms/demo/DemoPlaybackBar";
import { DemoBeatBadges } from "../components/organisms/demo/DemoBeatBadges";
import { useDemoPlayback } from "../hooks/useDemoPlayback";
import { useIsWide } from "../hooks/useMediaQuery";
import { ACT_STARTS, BEATS, DEMO_SEED } from "../lib/demoScript";
import { cn } from "../lib/utils";
import type { Mentionable, ThreadSummary } from "../types/api";
import type { DemoLocation } from "../lib/demoReducer";

const noop = () => {};

/** Block every network call for the life of the demo. Synchronous-safe (returns
 *  a rejected promise rather than throwing) so it never crashes a render, in any
 *  environment, while still surfacing a stray call loudly in the console. */
function useNoNetworkGuard() {
  useEffect(() => {
    const original = window.fetch;
    window.fetch = ((input: RequestInfo | URL) => {
      console.error("[demo] blocked network call:", String(input));
      return Promise.reject(new Error("demo: network disabled"));
    }) as typeof window.fetch;
    return () => {
      window.fetch = original;
    };
  }, []);
}

export function DemoView() {
  useNoNetworkGuard();
  const isWide = useIsWide();
  const { state, status, act, reduced, controls } = useDemoPlayback(
    DEMO_SEED,
    BEATS,
    ACT_STARTS,
  );

  const active = state.activeThreadId;
  const location: DemoLocation = active
    ? (state.threadLocation[active] ?? { kind: "channel", channelId: "c-launch", name: "launch" })
    : { kind: "channel", channelId: "c-launch", name: "launch" };

  const counterpart: Mentionable | undefined =
    location.kind === "dm"
      ? state.roster.find((m) => m.id === location.counterpartId)
      : undefined;

  // The timeline shows only threads in the active location (the #launch
  // channel, or the open DM) — mirroring how ChatView filters its feed.
  const visibleThreads = useMemo<ThreadSummary[]>(() => {
    return state.threads.filter((t) => {
      const loc = state.threadLocation[t.thread_id];
      if (!loc) return false;
      if (location.kind === "channel")
        return loc.kind === "channel" && loc.channelId === location.channelId;
      return loc.kind === "dm" && loc.counterpartId === location.counterpartId;
    });
  }, [state.threads, state.threadLocation, location]);

  const channelLabel =
    location.kind === "dm" ? `dm/${counterpart?.name ?? "agent"}` : location.name;
  const title = location.kind === "dm" ? channelLabel : `#${location.name}`;

  const humans = state.roster.filter((m) => m.kind === "human");
  const agents = state.roster.filter((m) => m.kind === "agent");
  const dmRoster = state.roster.filter((m) => m.id !== state.poster.id);

  const activeThread = state.threads.find((t) => t.thread_id === active) ?? null;
  const feed = active ? state.feeds[active] : undefined;

  return (
    <ChatLayout
      title={title}
      sidebar={
        <Sidebar
          workspace="Folio"
          channels={state.channels}
          dms={dmRoster}
          selectedChannelId={location.kind === "channel" ? location.channelId : null}
          selectedDmKey={location.kind === "dm" && counterpart ? dmKey(counterpart) : null}
          onSelectChannel={noop}
          onSelectDm={noop}
          onAddChannel={noop}
          onManageChannel={noop}
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
            threads={visibleThreads}
            roster={state.roster}
            channel={channelLabel}
            onOpenThread={noop}
          />
          <Composer
            roster={state.roster}
            mode={location.kind === "dm" ? "dm" : "channel"}
            dmCounterpart={counterpart}
            channel={location.kind === "dm" ? counterpart?.name ?? "" : location.name}
            disabled
            disabledHint="Sign up to talk to agents"
            onSubmit={noop}
          />
        </>
      }
      panelOpen
      onPanelClose={noop}
      panel={
        <div
          className={cn(
            "flex h-full flex-col bg-[var(--color-paper)]",
            isWide ? "w-[420px] shrink-0" : "w-full",
          )}
        >
          <DemoPlaybackBar
            status={status}
            act={act}
            onPlay={controls.play}
            onPause={controls.pause}
            onRestart={controls.restart}
          />
          <div className="min-h-0 flex-1">
            <ThreadPanel
              channel={location.kind === "dm" ? channelLabel : location.name}
              thread={activeThread}
              roster={state.roster}
              bubbles={feed?.bubbles ?? []}
              rootMessage={feed?.rootMessage}
              metaByKey={state.metaByKey}
              focusRequestId={null}
              onClose={noop}
              renderAfterBubble={(b) => (
                <DemoBeatBadges
                  badges={state.badgesByKey[b.key]}
                  connections={state.connections}
                  onConnect={controls.resolveGate}
                  reduced={reduced}
                />
              )}
            />
          </div>
        </div>
      }
    />
  );
}
