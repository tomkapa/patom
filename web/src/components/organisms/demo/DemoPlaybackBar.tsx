// Play / pause / restart controls + act indicator for the `/demo` playback.
// Pure props — `DemoView` wires it to `useDemoPlayback`.

import { Pause, Play, RotateCcw } from "lucide-react";
import { Button } from "../../atoms/Button";
import { cn } from "../../../lib/utils";
import type { PlayStatus } from "../../../hooks/useDemoPlayback";

const ACTS = ["Recruit", "Collaborate", "Proactive"];

export function DemoPlaybackBar({
  status,
  act,
  onPlay,
  onPause,
  onRestart,
}: {
  status: PlayStatus;
  act: number;
  onPlay: () => void;
  onPause: () => void;
  onRestart: () => void;
}) {
  const playing = status === "playing";
  return (
    <div className="flex items-center gap-3 border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-4 md:px-8 py-2">
      <span className="font-[var(--font-mono)] text-[10px] uppercase tracking-[0.18em] text-[var(--color-moss-deep)]">
        Demo · synthetic data
      </span>

      <div className="ml-2 flex items-center gap-1.5">
        {ACTS.map((label, i) => (
          <span
            key={label}
            className={cn(
              "font-[var(--font-mono)] text-[11px] transition-colors",
              i === act
                ? "font-bold text-[var(--color-ink)]"
                : "text-[var(--color-fg-muted)]",
            )}
          >
            {i + 1}. {label}
            {i < ACTS.length - 1 ? (
              <span className="ml-1.5 text-[var(--color-line-2)]">›</span>
            ) : null}
          </span>
        ))}
      </div>

      <div className="ml-auto flex items-center gap-1">
        <Button
          variant="ghost"
          size="sm"
          iconOnly
          aria-label={playing ? "Pause" : "Play"}
          onClick={playing ? onPause : onPlay}
        >
          {playing ? <Pause className="h-4 w-4" /> : <Play className="h-4 w-4" />}
        </Button>
        <Button variant="ghost" size="sm" iconOnly aria-label="Restart" onClick={onRestart}>
          <RotateCcw className="h-4 w-4" />
        </Button>
      </div>
    </div>
  );
}
