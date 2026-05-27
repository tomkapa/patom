# ADR-0001 — `send_message` is the only inter-actor channel

- **Status:** Accepted
- **Date:** 2026-05-15
- **Deciders:** core

## Context

The product thesis is that AI agents in Patom should communicate like coworkers in a workplace. Most LLM frameworks treat the assistant's text response as the output — whatever the model says is what the user reads. Multi-agent extensions on top of that pattern bolt on supervisors, routers, or message buses outside the loop.

Two failure modes follow from that shape:

1. **Two output channels.** The model can produce assistant text *and* tool calls. If both are user-deliverable, every turn has two seams to audit; if only the tool call is, the assistant text is silently dropped — confusing for users and for debugging.
2. **No observable handoff.** When agent A asks agent B to do something, there is no structural record of the handoff — only natural-language prose. Auditing, replaying, and visualising a multi-agent conversation become exercises in transcript parsing.

We wanted a single, structural seam where every interaction — human ↔ agent and agent ↔ agent — could be observed, persisted, and replayed.

## Decision

**Plain assistant text is never delivered. Every reply produced by an agent must be an explicit `send_message` tool call.** The tool is the only way an agent reaches a human or a peer.

```text
send_message {
  to:   { kind: "agent", name: "designer" } | { kind: "human" },
  body: "...",
  context_summary?: "..."
}
```

The runtime enforces this at the tool boundary: a turn that produces only plain text is not delivered to any recipient. Agents are taught the rule in `<core>` — the shared system-prompt prelude — so every role inherits it.

## Consequences

**What becomes easy:**

- The entire interaction history is a directed graph of `send_message` calls. Threads (the DAG rooted at a human prompt) are auditable by reading the rows.
- Slack `@mentions`, internal agent messages, and human chat all flow through the same shape — one protocol, multiple media.
- Per-message hooks (PII redaction, content moderation, rate-limiting) have one place to attach.
- Streaming distinguishes `text` chunks (model-internal scratch — never shown) from `agent_message` chunks (the deliverable). The FE renders only the latter; the former remains as collapsible "reasoning" for debugging.

**What becomes hard:**

- Agents must be trained to always call `send_message` instead of replying inline. The `<core>` prompt carries the rule, but role prompts must not contradict it.
- A turn that produces neither `send_message` nor a tool call is silent. We make the failure mode loud — the turn is marked complete with no delivered message, visible in the transcript.

**What we live with:**

- A small protocol cost — every reply is a structured call rather than free text. We've decided the auditability + multi-channel uniformity is worth it.

## Alternatives considered

- **Assistant text is the default; `send_message` only when crossing to another agent.** Two output channels per turn, ambiguous boundary, hard to enforce that the model never accidentally leaks an internal scratch line to the user. Rejected.
- **Inline `@mention` parsing in assistant text.** Fragile, model-format-dependent, can't distinguish addressed-to-coworker from describing-a-coworker. Rejected.
- **A dedicated message bus on top of LLM-as-router.** The router becomes a second LLM call, doubles latency, and we still need the typed tool call internally — we'd be building two protocols. Rejected.
