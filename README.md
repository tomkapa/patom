<p align="center">
  <a href="https://github.com/tomkapa/patom">
    <img src="./doc/assets/banner.png" alt="Patom" width="100%" />
  </a>
</p>

<p align="center">
  <strong>Run a company of AI agents the way you'd run a company of people.</strong>
</p>

<p align="center">
  <a href="./LICENSE.md"><img alt="License: FSL-1.1-Apache-2.0" src="https://img.shields.io/badge/license-FSL--1.1--Apache--2.0-blue.svg"></a>
  <img alt="Status: pre-1.0" src="https://img.shields.io/badge/status-pre--1.0-yellow.svg">
</p>

<p align="center">
  <a href="#the-idea">The idea</a> ·
  <a href="#why-patom">Why</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#documentation">Docs</a>
</p>

---

## The idea

Patom is a multi-agent runtime built on a single thesis: **an AI workforce should look and operate like a human one.**

A Patom deployment is shaped like a small company. Agents are *hired* into roles — `account-manager`, `copywriter`, `designer` — each with a job description, their own tool access, and their own memory. They talk to one another the way coworkers talk in Slack: by name, in threads, with `@` mentions. They pick up tasks from humans, hand work off to each other, and put their own follow-ups on the calendar. Every primitive maps to something an organisation already understands — hiring, role boundaries, handoffs, learning on the job.

The implementation is one Rust binary plus a Postgres-backed control plane. The agent core owns no I/O — every external dependency sits behind a trait — so the runtime is testable end-to-end without a network and portable across model providers.

> **Status:** pre-1.0. APIs, schema, and on-disk formats are still moving.

## Why Patom

Most agent frameworks hand you one chat loop with a bag of tools. When you need a second agent, you bolt on a "supervisor" pattern; when you need them to talk, you wire up a message bus; when you need them to keep state, you reach for vector databases and prompt-stuffing tricks. Collaboration is always an addition — never the foundation. None of it looks like how a real team actually works.

Patom inverts that. **Communication, role boundaries, and memory are the primitives.** The single-prompt chat loop is just one of several entry points into the same machinery. That changes what becomes natural to build:

- **The unit of work is not a conversation, it's an organisation.** Sessions, roles, and handoffs are first-class — not patterns you invent on top of a chat API.
- **An agent is not a prompt, it's a hire.** A role name, a job description, an allowlist of tools, a private journal, and a calendar of standing commitments.
- **Boundaries are enforced by the runtime, not by the model.** What an agent can call, who it can talk to, and what it remembers are checked in code before the LLM ever sees the choice.

If you have ever tried to express *"three agents collaborating on a client deliverable, where the designer cannot email the client and the account manager should remember the client's payment terms"* inside a single-loop framework, you already understand the gap Patom fills.

## How it works

Three ideas carry most of the design: **how agents communicate, how they remember, and how they wake themselves up.**

### 1. Communication: agents talk like coworkers

Agents address one another **by role name**, the same way you would write `@designer` in Slack. There is no UUID lookup and no central dispatcher — the runtime keeps an index of every coworker in each agent's system prompt, and a single tool, `send_message`, is the only way one agent reaches another.

```text
send_message {
  to:   { kind: "agent", name: "designer" },
  body: "Could you mock up a homepage hero for Acme Bakery?"
}
```

Plain assistant text never leaves the model. Every reply is an explicit `send_message` call, addressed either to a human or to a named peer. That single seam turns the entire interaction history into a directed graph of named handoffs — auditable, replayable, and shaped exactly like a thread of Slack messages.

```mermaid
flowchart LR
    H((Human)) -->|"@account-manager"| AM[account-manager]
    AM -->|"@brand-strategist"| BS[brand-strategist]
    BS -->|"@copywriter"| CW[copywriter]
    BS -->|"@designer"| DS[designer]
    CW -->|"hand-off"| AM
    DS -->|"hand-off"| AM
    AM -->|"reply"| H
```

A Slack workspace can be plugged in directly. Real `@mentions` in a channel are parsed by the bridge and routed to the named agent; from the agent's side, a Slack conversation looks the same as an internal one. The medium is interchangeable — the protocol is what's load-bearing.

### 2. Memory: agents learn the way employees learn

Each agent keeps a private journal that survives across sessions. It is not the raw transcript — it is the agent's distilled understanding of the job, organised the way a person organises what they know about their work:

| Kind             | What it captures                                                       |
|------------------|------------------------------------------------------------------------|
| **Self**         | The agent's own preferences, voice, rules of thumb.                    |
| **Other**        | What the agent has learned about the humans it works with.             |
| **Collaborator** | Which coworker to delegate to for which kind of work.                  |
| **Procedure**    | How-tos and patterns that worked (or did not).                         |
| **Open**         | Questions the agent knows it cannot yet answer.                        |

Every memory has a lifecycle — **tentative → held → validated → core** — that records how strongly the agent should trust it. New observations enter as `tentative`; they mature to `held`; they are promoted to `validated` when independent signals confirm them; and only an operator can pin a memory as `core`. Promotion and demotion give the system a model of confidence without asking the LLM to reason about numbers.

```mermaid
flowchart LR
    T[tentative] --> H[held]
    H --> V[validated]
    H -. "operator pin" .-> C[core]
    V -. "operator pin" .-> C
    T -. "forget" .-> X[(removed)]
    H -. "forget" .-> X
```

Memory does not update on a fixed schedule. When a conversation has been quiet long enough, a **reflection** turn fires automatically: the agent re-reads the trimmed transcript, decides what to remember, update, or forget, and writes the result back to its journal — exactly the way a person reflects on a meeting after it ends. Consolidation happens in the idle moments between bursts of work, not in the middle of them.

At turn time, relevant memories are recalled by vector similarity and rendered into the system prompt — grouped by kind, tagged by state — so the agent reasons over *"what I believe and how strongly"* rather than an opaque blob.

### 3. Autonomous wake-ups: agents own their calendar

Agents can schedule **their own** follow-ups — one-off or recurring, timezone-aware — through a `schedule_task` tool. When the timer fires, the runtime synthesises a fresh prompt to the agent, indistinguishable from one a human would have sent.

```text
schedule_task {
  when:   { recurring: { weekdays: ["fri"], time: "16:00", tz: "Europe/London" } },
  prompt: "Compose the Friday status email for Acme Bakery."
}
```

This is what turns a fleet of agents from a reactive service desk into a proactive team. The account manager remembers to follow up on Monday; the project coordinator sends the Friday status report without being asked. From the runtime's perspective, a calendar tick is just another input — the agent runs the same code path either way.

## Architecture

For readers who want one level deeper. The rest of the design notes live in [`doc/`](./doc/) and [`CLAUDE.md`](./CLAUDE.md).

```mermaid
flowchart TB
    subgraph Surfaces["Surfaces"]
        WEB["Web UI"]
        SLACK["Slack bridge"]
        API["HTTP API"]
    end

    subgraph Runtime["Runtime"]
        QUEUE["Durable prompt queue<br/>(leased rows in Postgres)"]
        WORKERS["Worker pool"]
        SCHED["Scheduler<br/>(cron + DST)"]
    end

    subgraph Core["Agent core"]
        AGENT["Turn loop"]
        HOOKS["Policy hooks<br/>(allow / deny / mutate)"]
        TOOLS["ToolBox<br/>(built-ins + scoped MCP)"]
        MEMORY["Memory<br/>(read / write / reflect)"]
    end

    subgraph Providers["Providers"]
        LLM["LLM adapters<br/>(Anthropic, OpenAI)"]
        MCP["MCP integrations<br/>(Notion, Gmail, Pencil, ...)"]
    end

    subgraph State["State"]
        PG[("Postgres + pgvector")]
    end

    Surfaces --> QUEUE
    SCHED --> QUEUE
    QUEUE --> WORKERS
    WORKERS --> AGENT
    AGENT --> HOOKS
    AGENT --> TOOLS
    AGENT --> MEMORY
    AGENT --> LLM
    TOOLS --> MCP
    AGENT --> PG
    MEMORY --> PG
    QUEUE --> PG
```

A few load-bearing pieces worth naming:

- A **durable, leased queue** is every agent's inbox — whether the trigger is a human message, a Slack mention, an inter-agent handoff, or a scheduled wake-up, all four enter through the same row.
- The **agent core** is provider-agnostic. Model providers and MCP integrations sit behind traits; the loop is testable end-to-end against fakes, no network required.
- **Per-agent tool boundaries** are enforced before the model sees a choice. An agent without `gmail` in its allowlist cannot call Gmail — the tool simply is not in its ToolBox.
- **Per-org envelope encryption** protects upstream MCP credentials at rest; tenant isolation runs on Postgres RLS keyed off `org_id`.

For the deeper rationale — DAG turn budgets, hooks as policy, the reasoning behind the memory lifecycle, the relational schema — start at [`doc/ARCHITECTURE.md`](./doc/ARCHITECTURE.md) and the [`doc/adr/`](./doc/adr/) index.

## Quick start

```bash
# 1. Postgres + pgvector
docker compose up -d

# 2. Configure
cp .env.example .env   # set ANTHROPIC_API_KEY, PATOM_JWT_SECRET, PATOM_ORG_KEK, ...

# 3. Run migrations + start the server (boots on :8080)
cargo run --release

# 4. (optional) Web UI
cd web && bun install && bun run dev
```

### Prerequisites

- Rust toolchain (see [`rust-toolchain.toml`](./rust-toolchain.toml)).
- Docker / Docker Compose for the bundled Postgres + pgvector image.
- [Bun](https://bun.sh) if you want to develop the web UI.
- An Anthropic or OpenAI API key.

The minimum environment to boot:

```bash
DATABASE_URL=postgres://patom:patom@localhost:5432/patom
ANTHROPIC_API_KEY=...
OPENAI_API_KEY=...        # only if you want OpenAI / embeddings
PATOM_JWT_SECRET=...      # session cookies
PATOM_ORG_KEK=...         # MCP credential envelope key
```

The full set lives in [`src/config.rs`](./src/config.rs).

### Tests

Patom is TDD-first. Integration tests use a real Postgres via `#[sqlx::test]`.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

All four gates (`fmt`, `clippy`, `check`, `test`) must be green before a PR lands.

## Documentation

- [`CLAUDE.md`](./CLAUDE.md) — engineering rules. Binding, not advisory. Read before reading any module.
- [`doc/ARCHITECTURE.md`](./doc/ARCHITECTURE.md) — how the system is built, end to end.
- [`doc/adr/`](./doc/adr/) — architecture decision records (the *why* behind each load-bearing choice).
- [`doc/marketing.md`](./doc/marketing.md) — the worked example: Patom marketing itself, with five role-named agents covering strategy, long-form, social, design, and community.
- [`doc/operations/`](./doc/operations/) — operator runbooks ([self-hosting & air-gap install](./doc/operations/self-hosting.md), Slack setup, known integration issues).
- [`migrations/`](./migrations) — the canonical schema.

## Contributing

We welcome contributions. Start with [`CONTRIBUTING.md`](./CONTRIBUTING.md). Security issues go through [`SECURITY.md`](./SECURITY.md) — please do not open public issues for vulnerabilities.

By participating you agree to our [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md).

## License

Patom is **source-available** under the [Functional Source License, Version 1.1, with an Apache 2.0 Future License](./LICENSE.md) (`FSL-1.1-Apache-2.0`).

In plain English:

- **You may** use, modify, self-host, and redistribute Patom for any purpose that is not a Competing Use — internal use, non-commercial education and research, and professional services delivered to licensees are all explicitly permitted. To deploy on your own cluster, see the [self-hosting runbook](./doc/operations/self-hosting.md).
- **You may not** use Patom to offer a commercial product or service that substitutes for Patom or for any product/service we offer using Patom.
- **Two years after each release**, that release automatically converts to the [Apache License 2.0](http://www.apache.org/licenses/LICENSE-2.0). The non-compete is time-limited, not perpetual.

If you need a commercial license that lifts the Competing Use restriction sooner, contact the maintainers.

---

<p align="center">
  Built with care, in Rust.
</p>
