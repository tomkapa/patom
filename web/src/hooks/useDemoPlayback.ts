// Drives the `/demo` scripted playback: schedules beats on an injectable clock,
// owns play/pause/restart, the single click-to-continue gate, end-of-script
// looping, and prefers-reduced-motion (jump to the terminal frame, no timers).
// The only time source is `clock`, and the only state source is the pure
// reducer — so a future vitest can mount this with `makeManualClock()` and step
// the whole story deterministically.

import { useEffect, useMemo, useState } from "react";
import { useReducedMotion } from "motion/react";
import { realClock, type Clock } from "../lib/demoClock";
import {
  initialDemoState,
  isGate,
  reduce,
  terminalState,
  type Beat,
  type DemoSeed,
  type DemoState,
} from "../lib/demoReducer";

export type PlayStatus = "playing" | "paused" | "awaiting-gate" | "ended";

/** Per-beat dwell before the *next* beat lands. Agent turns with tool/reasoning
 *  meta linger longer so the viewer can read them; jumps tick by quickly. */
function delayFor(beat: Beat): number {
  switch (beat.type) {
    case "post":
      if (beat.connect) return 2200;
      if (beat.meta && beat.meta.tools.length > 0) return 2600;
      if (beat.meta) return 2000;
      return beat.sender.kind === "human" ? 1500 : 1900;
    case "tile":
      return beat.to === "connecting" ? 500 : 900;
    case "hire":
      return 1000;
    case "mention":
      return 1700;
    case "jump":
      return 700;
  }
}

const LOOP_PAUSE_MS = 4000;

export type DemoControls = {
  play: () => void;
  pause: () => void;
  restart: () => void;
  /** Resolve the one interactive gate (the Connect-Notion click). */
  resolveGate: () => void;
};

export type DemoPlayback = {
  state: DemoState;
  status: PlayStatus;
  /** Index of the act currently playing (0-based). */
  act: number;
  reduced: boolean;
  controls: DemoControls;
};

export function useDemoPlayback(
  seed: DemoSeed,
  beats: Beat[],
  actStarts: number[],
  clock: Clock = realClock,
): DemoPlayback {
  const reduced = !!useReducedMotion();
  const [frame, setFrame] = useState<DemoState>(() =>
    reduced ? terminalState(seed, beats) : initialDemoState(seed),
  );
  const [cursor, setCursor] = useState(reduced ? beats.length : 0);
  const [status, setStatus] = useState<PlayStatus>(reduced ? "ended" : "playing");

  // The scheduler: while playing, arm a single timer for the next beat. The
  // cleanup clears it on pause / unmount / StrictMode double-mount, so a beat
  // is never applied twice.
  useEffect(() => {
    if (reduced || status !== "playing") return;

    if (cursor >= beats.length) {
      const loopId = clock.setTimeout(() => {
        setFrame(initialDemoState(seed));
        setCursor(0);
      }, LOOP_PAUSE_MS);
      return () => clock.clearTimeout(loopId);
    }

    const beat = beats[cursor]!;
    const id = clock.setTimeout(() => {
      setFrame((f) => reduce(f, beat));
      if (isGate(beat)) setStatus("awaiting-gate");
      setCursor((c) => c + 1);
    }, delayFor(beat));
    return () => clock.clearTimeout(id);
  }, [status, cursor, reduced, beats, seed, clock]);

  const controls = useMemo<DemoControls>(
    () => ({
      play: () =>
        setStatus((s) => (s === "ended" ? "playing" : s === "paused" ? "playing" : s)),
      pause: () => setStatus((s) => (s === "playing" ? "paused" : s)),
      restart: () => {
        setFrame(initialDemoState(seed));
        setCursor(0);
        setStatus("playing");
      },
      resolveGate: () => setStatus((s) => (s === "awaiting-gate" ? "playing" : s)),
    }),
    [seed],
  );

  // End-of-script is a transient frame (cursor === length while playing) that
  // the loop timer resets; only surface "ended" when paused there.
  const act = useMemo(() => {
    let a = 0;
    for (let i = 0; i < actStarts.length; i++) {
      if (cursor >= actStarts[i]!) a = i;
    }
    return a;
  }, [cursor, actStarts]);

  return {
    state: frame,
    status,
    act,
    reduced,
    controls,
  };
}
