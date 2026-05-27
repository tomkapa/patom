# ADR-0005 — Per-agent MCP tool scoping enforced in code

- **Status:** Accepted
- **Date:** 2026-05-14
- **Deciders:** core

## Context

In a five-agent agency deployment, the designer should not be able to email the client, and the account-manager should not be editing design files. Two ways to enforce that boundary:

1. **Prompt-only.** The role prompt says "you do not use Gmail." A confused or jailbroken model can ignore this.
2. **Code-level.** The Gmail tool is literally not in the agent's `ToolBox` for the turn. The model cannot call what it cannot see.

The product positioning depends on this being a *real* boundary — the value proposition to a buyer is "the designer cannot accidentally email the client, ever, even if jailbroken." A prompt-only guard does not deliver that.

## Decision

**Each agent carries an explicit `allowed_mcp_servers` list and an optional `allowed_mcp_tools` list. The runtime filters the org's MCP toolset down to the allowlist *before* the model sees any of it.** Tools the agent is not allowed to call are not in the ToolBox; the model cannot call what is not there.

- `agents_allowed_mcp_catalog` — per-agent allowlist of catalog entries (e.g. `notion`, `gmail`).
- `agent_allowed_mcp_tools` — optional within-catalog granularity ("notion: only `get_page` and `update_page`").
- Absence of any allowlist row means **no MCP access**. Built-in tools (`memory_*`, `send_message`, `search_agents`, `schedule_task`, …) remain available to every agent regardless.
- The filtering seam is `src/mcp/scoped.rs::ScopedMcpSource`. It runs at turn-build time, not inside a hook — hooks see only tools the agent is already allowed to call.

The decision is not honest unless we name the limit: **tool-class enforcement is mechanical; resource-level scoping within a tool is prompt-only.** Notion's `update_page` can in principle touch any page in the workspace; "the designer only writes to the handoff page" is enforced by the role prompt, not the MCP allowlist. We state this directly in product positioning so claims don't oversell.

## Consequences

**What becomes easy:**

- A jailbroken designer asking for a Gmail call gets a tool-not-found error — there is no tool to invoke. The boundary is real.
- Adding a new tool to an agent is a database operation, not a code change.
- Audit ("what can this agent do?") is one query.

**What becomes hard:**

- An agent denied a tool cannot recover by switching to "another route to the same effect" — there isn't one. This is the design intent, but it means the role prompt must teach the agent to delegate (via `send_message`) when it hits a wall.
- Resource-level scoping within a tool requires a per-tool MCP gateway with policy enforcement — out of scope today. The role-prompt firewall is the v1 mitigation; structural enforcement is Phase 2.

**What we live with:**

- A small per-turn cost to filter the tool list. Bounded; the agent's allowlist is small (single-digit catalogs per agent in realistic deployments).
- The honesty caveat about within-tool scoping is on us to state in marketing/positioning, not on the runtime to plug.

## Alternatives considered

- **Prompt-only.** Doesn't deliver the buyer-facing guarantee. Rejected.
- **Org-wide allowlist with role-based filtering enforced inside each tool.** Pushes policy into every tool author's hands; brittle. Rejected.
- **A hook that denies tool calls outside the allowlist.** Tool is in the ToolBox, model can still try to call it, error returns at runtime. Worse UX for the model (it sees a denial reason and may retry); the deny-list approach also leaks tool names through error messages. Rejected — filter at the source.
