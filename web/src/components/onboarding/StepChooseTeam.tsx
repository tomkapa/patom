import { useMemo, useState } from "react";
import { useMutation } from "@tanstack/react-query";
import {
  ArrowRight,
  Check,
  ChevronRight,
  Cpu,
  Info,
  Shield,
  SlidersHorizontal,
  Users,
} from "lucide-react";
import {
  TEAM_PRESETS,
  findPreset,
  renderPrompt,
  type PresetAgent,
  type PresetId,
  type TeamPreset,
} from "../../data/teamPresets";
import { useAuthStore } from "../../stores/authStore";
import { api } from "../../lib/api";
import { track } from "../../lib/analytics";
import { ApiError } from "../../lib/errors";
import { LucideByName } from "./LucideByName";

/** Step 2 — pick a preset team and hire its agents. The Recruiter (the
 *  org's default-seeded agent) is NOT in any preset; it's always already
 *  hired by the time the wizard runs. Hiring loops `POST /agents`
 *  sequentially (so the entitlements gate applies per call and a partial
 *  failure shows on the failing index). */
export function StepChooseTeam({
  initialPresetId,
  onContinue,
}: {
  initialPresetId: PresetId | null;
  onContinue: (id: PresetId) => void;
}) {
  const [selectedId, setSelectedId] = useState<PresetId>(
    initialPresetId ?? "marketing",
  );
  const selected = useMemo(() => findPreset(selectedId), [selectedId]);
  const workspaceName = useAuthStore(
    (s) =>
      s.me?.orgs.find((o) => o.id === s.me?.active_org_id)?.name ??
      "your workspace",
  );

  const m = useMutation({
    mutationFn: async (preset: TeamPreset) => {
      // Sequential so per-agent failures are observable. Agent names
      // are unique per org on the BE — on retry a previously-hired
      // agent surfaces as a 409 Conflict; we treat that as "already
      // done" and march on, which is what makes the loop honestly
      // resumable from the failing slot.
      for (const a of preset.agents) {
        try {
          await api.createAgent({
            name: a.name,
            description: a.description,
            system_prompt: renderPrompt(a.system_prompt, workspaceName),
            model: a.model,
            allowed_mcp_tools: {},
          });
          // One event per genuinely-created agent. The 409 path below is a
          // resume over an already-hired agent, so it must not re-count.
          track("agent_created");
        } catch (err) {
          if (err instanceof ApiError && err.status === 409) {
            // Already hired on a previous attempt — skip and continue.
            continue;
          }
          throw err;
        }
      }
    },
    onSuccess: (_data, preset) => onContinue(preset.id),
  });

  return (
    <div
      className="flex w-[1200px] flex-col gap-6"
      data-step="choose-team"
    >
      {/* Hero */}
      <div className="flex flex-col gap-2">
        <span className="font-[var(--font-mono)] text-[11px] tracking-[1.5px] text-[var(--color-moss)]">
          WORKSPACE SETUP · {workspaceName.toUpperCase()}
        </span>
        <h1 className="flex items-center gap-2.5 font-[var(--font-display)] text-[34px] font-bold leading-tight tracking-tight text-[var(--color-moss-deep)]">
          <span>Start with a</span>
          <span className="inline-flex items-center justify-center bg-[var(--color-moss)] px-2.5 py-0.5 text-white">
            team
          </span>
          <span>built for your work</span>
        </h1>
        <p className="max-w-[720px] text-[15px] leading-[1.5] text-[var(--color-fg-secondary)]">
          Pick a preset and we'll hire its agents preconfigured — model,
          role, and tools ready to go. Tweak anything later.
        </p>
      </div>

      {/* Panel */}
      <div className="flex h-[600px] gap-5">
        {/* Left: preset list */}
        <ul
          className="flex w-[360px] shrink-0 flex-col gap-2"
          aria-label="Team presets"
          data-testid="preset-list"
        >
          {TEAM_PRESETS.slice(0, 4).map((p) => (
            <PresetRow
              key={p.id}
              preset={p}
              selected={p.id === selectedId}
              onSelect={() => setSelectedId(p.id)}
            />
          ))}
          <li className="h-px bg-[var(--color-border-subtle)]" aria-hidden="true" />
          <PresetRow
            preset={findPreset("scratch")}
            selected={selectedId === "scratch"}
            onSelect={() => setSelectedId("scratch")}
          />
        </ul>

        {/* Right: preview */}
        <div className="flex flex-1 flex-col bg-[var(--color-surface-primary)] ring-1 ring-[var(--color-moss-deep)]">
          {/* Head */}
          <div className="flex items-center gap-3.5 border-b border-[var(--color-border-subtle)] px-5 py-5">
            <div className="flex h-12 w-12 items-center justify-center bg-[var(--color-moss)] text-white">
              <LucideByName name={selected.icon} size={24} />
            </div>
            <div className="flex min-w-0 flex-1 flex-col gap-0.5">
              <div className="font-[var(--font-display)] text-[20px] font-bold text-[var(--color-moss-deep)]">
                {selected.display_name === "Start from scratch"
                  ? "Start from scratch"
                  : `${selected.display_name} team`}
              </div>
              <div className="text-[13px] text-[var(--color-fg-secondary)]">
                {selected.blurb}
              </div>
            </div>
            <div className="flex items-center gap-1.5 bg-[var(--color-moss-soft)] px-2.5 py-1.5">
              <Users className="h-3.5 w-3.5 text-[var(--color-moss)]" />
              <span className="font-[var(--font-mono)] text-[12px] font-medium text-[var(--color-moss)]">
                {selected.agents.length} agents
              </span>
            </div>
          </div>

          {/* Roster — 2-column grid */}
          <div className="flex flex-1 flex-col gap-3 overflow-auto px-5 py-5">
            <div className="font-[var(--font-mono)] text-[10px] tracking-[1.5px] text-[var(--color-fg-muted)]">
              AGENTS WE'LL HIRE
            </div>
            {selected.agents.length === 0 && (
              <div className="rounded-sm bg-[var(--color-surface-secondary)] px-4 py-6 text-[13px] text-[var(--color-fg-secondary)]">
                No preset agents — your Recruiter will be the only one hired,
                and you can add agents from the workspace.
              </div>
            )}
            <div className="grid grid-cols-2 gap-3">
              {selected.agents.map((a) => (
                <AgentCard key={a.name} agent={a} />
              ))}
            </div>
          </div>

          {/* Foot */}
          <div className="flex items-center justify-between gap-4 border-t border-[var(--color-border-subtle)] bg-[var(--color-surface-secondary)] px-5 py-4">
            <div className="flex items-center gap-2 text-[13px] text-[var(--color-fg-secondary)]">
              <Info className="h-3.5 w-3.5 text-[var(--color-fg-muted)]" />
              {selected.agents.length === 0
                ? "Just your Recruiter — add agents yourself later."
                : `Hires ${selected.agents.length} agents alongside your default Recruiter.`}
            </div>
            <div className="flex items-center gap-2.5">
              <button
                type="button"
                disabled
                className="inline-flex cursor-not-allowed items-center justify-center gap-1.5 bg-[var(--color-surface-primary)] px-3.5 py-2.5 text-[14px] font-medium text-[var(--color-moss-deep)] opacity-50 ring-1 ring-[var(--color-border-subtle)]"
                title="Customize will arrive after onboarding"
              >
                <SlidersHorizontal className="h-3.5 w-3.5" />
                Customize
              </button>
              <button
                type="button"
                disabled={m.isPending}
                onClick={() => m.mutate(selected)}
                className="inline-flex cursor-pointer items-center justify-center gap-2 bg-[var(--color-moss)] px-4.5 py-2.5 text-[14px] font-semibold text-white transition-colors hover:bg-[var(--color-moss-deep)] disabled:cursor-not-allowed disabled:opacity-50"
                data-testid="onboarding-hire-team"
              >
                {m.isPending
                  ? "Hiring…"
                  : selected.agents.length === 0
                    ? "Continue"
                    : "Hire team & continue"}
                <ArrowRight className="h-3.5 w-3.5" />
              </button>
            </div>
          </div>
          {m.isError && (
            <div className="border-t border-[var(--color-rose-soft)] bg-[var(--color-rose-soft)] px-5 py-2 text-[12px] text-[var(--color-rose)]">
              Couldn't hire one of the agents. Try again — already-created
              agents are kept and the loop resumes from the failing slot.
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

function PresetRow({
  preset,
  selected,
  onSelect,
}: {
  preset: TeamPreset;
  selected: boolean;
  onSelect: () => void;
}) {
  return (
    <li>
      <button
        type="button"
        onClick={onSelect}
        aria-pressed={selected}
        data-testid={`preset-${preset.id}`}
        className={
          "flex w-full cursor-pointer items-center gap-3 px-3.5 py-3 text-left transition-colors " +
          (selected
            ? "bg-[var(--color-moss-soft)] ring-[1.5px] ring-[var(--color-moss)]"
            : "bg-[var(--color-surface-primary)] ring-1 ring-[var(--color-border-subtle)] hover:bg-[var(--color-surface-secondary)]")
        }
      >
        <div
          className={
            "flex h-10 w-10 items-center justify-center " +
            (selected
              ? "bg-[var(--color-moss)] text-white"
              : "bg-[var(--color-surface-secondary)] text-[var(--color-moss-deep)] ring-1 ring-[var(--color-border-subtle)]")
          }
        >
          <LucideByName name={preset.icon} size={19} />
        </div>
        <div className="flex min-w-0 flex-1 flex-col gap-0.5">
          <div className="text-[14px] font-semibold text-[var(--color-moss-deep)]">
            {preset.display_name}
          </div>
          <div className="font-[var(--font-mono)] text-[11px] text-[var(--color-fg-muted)]">
            {preset.roster_summary}
          </div>
        </div>
        {selected ? (
          <span className="flex h-5 w-5 items-center justify-center rounded-full bg-[var(--color-moss)] text-white">
            <Check className="h-3 w-3" strokeWidth={3} />
          </span>
        ) : (
          <ChevronRight className="h-4 w-4 text-[var(--color-fg-muted)]" />
        )}
      </button>
    </li>
  );
}

function AgentCard({ agent }: { agent: PresetAgent }) {
  return (
    <div className="flex flex-col gap-3 bg-[var(--color-surface-primary)] p-3.5 ring-1 ring-[var(--color-border-subtle)]">
      <div className="flex items-center gap-2.5">
        <div className="flex h-9 w-9 items-center justify-center bg-[var(--color-moss-soft)] text-[var(--color-moss)]">
          <LucideByName name={agent.icon} size={18} />
        </div>
        <div className="flex min-w-0 flex-1 flex-col gap-0.5">
          <div className="text-[14px] font-semibold text-[var(--color-moss-deep)]">
            {agent.name}
          </div>
          <div className="text-[12px] text-[var(--color-fg-muted)]">
            {agent.description}
          </div>
        </div>
      </div>
      <div className="flex flex-wrap items-center gap-1.5">
        <span className="inline-flex items-center gap-1.5 bg-[var(--color-surface-secondary)] px-2 py-1 font-[var(--font-mono)] text-[11px] text-[var(--color-moss-deep)] ring-1 ring-[var(--color-border-subtle)]">
          <Cpu className="h-3 w-3 text-[var(--color-moss)]" />
          {agent.model_label}
        </span>
        <span className="inline-flex items-center gap-1.5 bg-[var(--color-surface-secondary)] px-2 py-1 font-[var(--font-mono)] text-[11px] text-[var(--color-fg-secondary)] ring-1 ring-[var(--color-border-subtle)]">
          <Shield className="h-3 w-3 text-[var(--color-fg-secondary)]" />
          {agent.tools_hint}
        </span>
      </div>
    </div>
  );
}

