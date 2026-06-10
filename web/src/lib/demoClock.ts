// The `/demo` playback's only time source. Production passes `realClock`;
// a future vitest pass passes `makeManualClock()` and drives `tick(ms)` so
// the scripted timeline is deterministic — the FE analogue of the project's
// "tests own the clock" rule.

export interface Clock {
  setTimeout(fn: () => void, ms: number): number;
  clearTimeout(id: number): void;
}

export const realClock: Clock = {
  setTimeout: (fn, ms) => window.setTimeout(fn, ms),
  clearTimeout: (id) => window.clearTimeout(id),
};

export interface ManualClock extends Clock {
  /** Advance virtual time by `ms`, firing every timer whose deadline passes. */
  tick(ms: number): void;
  /** Number of timers still pending — handy for test assertions. */
  pending(): number;
}

/** A deterministic clock for tests. Timers fire in deadline order on `tick`. */
export function makeManualClock(): ManualClock {
  let nowMs = 0;
  let nextId = 1;
  const timers = new Map<number, { at: number; fn: () => void }>();

  return {
    setTimeout(fn, ms) {
      const id = nextId++;
      timers.set(id, { at: nowMs + Math.max(0, ms), fn });
      return id;
    },
    clearTimeout(id) {
      timers.delete(id);
    },
    tick(ms) {
      const target = nowMs + Math.max(0, ms);
      // Fire in deadline order; re-scan after each so timers scheduled by a
      // fired callback are honored within the same tick window.
      for (;;) {
        let due: { id: number; at: number; fn: () => void } | null = null;
        for (const [id, t] of timers) {
          if (t.at <= target && (due === null || t.at < due.at)) {
            due = { id, at: t.at, fn: t.fn };
          }
        }
        if (!due) break;
        timers.delete(due.id);
        nowMs = due.at;
        due.fn();
      }
      nowMs = target;
    },
    pending() {
      return timers.size;
    },
  };
}
