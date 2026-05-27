# Patom

**Patom** is a provider-agnostic, hookable multi-agent runtime written in Rust. It lets a small team operate a fleet of specialised LLM agents that each have their own memory, their own tool allowlist, and the ability to schedule their own wake-ups — collaborating over a structured `send_message` protocol instead of one monolithic prompt.

> Status: pre-1.0. APIs, schema, and on-disk formats are still moving.

---

## Why Patom

Most "agent frameworks" hand you one chat loop with a bag of tools. Patom is built around three load-bearing ideas that change that shape:

1. **Per-agent memory bounded by role.** Each agent owns its own private journal. The account-manager's memory of how a client signs off contracts never leaks into the copywriter's voice rules.
2. **Per-agent tool boundaries enforced in code.** An agent's `allowed_mcp_servers` and `allowed_mcp_tools` lists are checked by the runtime before a tool ever reaches the model — the designer literally cannot call Gmail because Gmail isn't in their ToolBox.
3. **Autonomous wake-ups.** Agents schedule one-time and recurring tasks for themselves via `schedule_task`. A `prompt_requests` row appears at fire time and the agent runs the same code path as a human prompt — there's no "the human clicked a button" beat.

Combined, those three turn a chat agent into something closer to an org chart: roles, memory, and side-effects on real clocks.

---

## Architecture

Patom is one Rust binary (`patom-rs`) plus a Postgres-backed control plane. Every external dependency sits behind a trait so the agent core has no I/O of its own — that's what makes the runtime testable end-to-end without a network.

### Component diagram

```mermaid
flowchart TB
    subgraph Clients["Clients"]
        WEB["Web UI (web/)"]
        SLACK["Slack workspace"]
        BRUNO["Bruno / API clients"]
    end

    subgraph HTTP["HTTP layer (axum + tower)"]
        ROUTER["router + auth_layer + CSRF"]
        ROUTES["routes/*  (agents, sessions, memory, mcp, slack, scheduling, orgs)"]
    end

    subgraph Core["Agent core"]
        AGENT["Agent (turn loop)"]
        HOOK["HookChain (policy)"]
        TOOLS["ToolBox (built-ins + scoped MCP)"]
        OBSERVER["TurnObserver"]
    end

    subgraph Runtime["Runtime"]
        QUEUE["PgPromptQueue"]
        WORKERS["WorkerPool"]
        DAG["DagBudget (depth/fan-out caps)"]
        STREAM["ThreadStream (SSE)"]
        SCHED["ScheduledTaskScheduler"]
    end

    subgraph Providers["LLM providers"]
        ANTHROPIC["Anthropic (claudius)"]
        OPENAI["OpenAI / compatible"]
        EMBED["Embeddings"]
    end

    subgraph MCP["MCP integrations"]
        REG["McpRegistry"]
        SCOPED["ScopedMcpSource (per-agent allowlist)"]
        OAUTH["OAuth flow + Refresher"]
        CATALOG["Vendor catalog (Notion, Gmail, GCal, Pencil, ...)"]
    end

    subgraph Storage["Storage (Postgres + R2)"]
        PG[("Postgres + pgvector")]
        R2["Cloudflare R2 (assets)"]
        CRYPTO["Per-org envelope crypto"]
    end

    subgraph Obs["Observability"]
        TRACE["tracing + OTel"]
        LF["Langfuse / OTLP exporter"]
    end

    WEB --> ROUTER
    SLACK --> ROUTER
    BRUNO --> ROUTER
    ROUTER --> ROUTES
    ROUTES --> QUEUE
    ROUTES --> AGENT
    QUEUE --> WORKERS
    SCHED --> QUEUE
    WORKERS --> AGENT
    AGENT --> HOOK
    AGENT --> TOOLS
    AGENT --> OBSERVER
    AGENT --> Providers
    TOOLS --> SCOPED
    SCOPED --> REG
    REG --> CATALOG
    REG --> OAUTH
    OAUTH --> CRYPTO
    CRYPTO --> PG
    AGENT --> STREAM
    STREAM --> WEB
    DAG --> WORKERS
    ROUTES --> PG
    ROUTES --> R2
    AGENT --> PG
    Core --> TRACE
    Runtime --> TRACE
    TRACE --> LF
```

### Turn lifecycle

```mermaid
sequenceDiagram
    participant U as User / Slack / Schedule
    participant H as HTTP route
    participant Q as PgPromptQueue
    participant W as Worker
    participant A as Agent
    participant Hk as HookChain
    participant P as Provider (Anthropic/OpenAI)
    participant T as Tool (built-in or MCP)
    participant DB as Postgres

    U->>H: POST prompt / wakeup fires
    H->>Q: insert prompt_request (with lease key)
    Q-->>W: claim row (leased)
    W->>A: build turn (session + memory + tools)
    A->>Hk: pre-turn hooks (allow / deny / mutate)
    A->>P: call model with tool schemas
    P-->>A: tool_use + text
    loop tool_calls
        A->>T: invoke (with timeout, bounded)
        T-->>A: result
    end
    A->>Hk: post-turn hooks
    A->>DB: persist turn, memory writes, todos
    W->>Q: ack / release lease
    A-->>H: SSE / response
```

### Data model (Postgres)

The schema lives under [`migrations/`](./migrations) and is tenant-scoped end-to-end (RLS on top of `org_id`). Highlights:

- `orgs`, `users`, `org_members`, `org_invites`, `org_rules`, `org_language` — multi-tenant governance.
- `agents`, `agent_prompt_versions`, `agents_allowed_mcp_catalog`, `agent_allowed_mcp_tools` — agent definitions + per-agent tool scoping.
- `sessions`, `session_messages`, `session_todos`, `turn_metrics`, `tool_calls` — conversation history and per-turn observability.
- `agent_memory` — per-agent journal (Self / Other / Procedure / Open / Collaborator kinds), with `tentative → held → core` lifecycle and pgvector recall.
- `prompt_requests` + worker leases — the durable queue that drives the runtime.
- `mcp_catalog`, `mcp_servers`, `mcp_credentials`, `mcp_oauth_clients`, `mcp_oauth_pending` — MCP integration metadata, envelope-encrypted upstream credentials per-org.
- `slack_workspaces`, `slack_identities`, `slack_threads` — Slack bridge.
- `scheduled_tasks` — recurring + one-time agent wake-ups with IANA timezone + DST.

### Module map

| Crate path | Responsibility |
|---|---|
| `src/agent_core/` | Provider-agnostic turn loop. Owns no I/O. |
| `src/agents/` | Agent records, system-prompt registry, prompt versioning. |
| `src/runtime/` | Durable queue, worker pool, DAG budget, thread stream. |
| `src/session/` | Session/message persistence. |
| `src/memory/` | Per-agent memory, librarian, reflection scheduler. |
| `src/hook/` | Pre/post-turn policy hooks. |
| `src/tools/` | Built-in tools (`memory_*`, `send_message`, `schedule_task`, `web_fetch`, ...). |
| `src/mcp/` | MCP client, registry, per-agent scoping, OAuth + refresher. |
| `src/provider/` | Anthropic + OpenAI provider adapters; embeddings. |
| `src/scheduling/` | Cron-like scheduled tasks; timezone-aware. |
| `src/slack/` | Slack bridge (events, signing, threads). |
| `src/orgs/`, `src/auth/` | Multi-tenant governance + Google OAuth + cookie sessions. |
| `src/crypto/` | Per-org envelope encryption for MCP credentials. |
| `src/observability/` | `tracing` + OpenTelemetry wiring. |
| `src/http/` | axum router, auth middleware, route handlers. |
| `web/` | TypeScript/Bun web UI. |

For the data-flow rationale and the deeper "why" behind each seam, read [`CLAUDE.md`](./CLAUDE.md) and the design notes under [`doc/`](./doc/).

---

## Getting started

### Prerequisites

- Rust toolchain (see [`rust-toolchain.toml`](./rust-toolchain.toml)).
- Docker / Docker Compose (for the bundled Postgres + pgvector image).
- [Bun](https://bun.sh) if you want to develop the web UI.
- An Anthropic or OpenAI API key.

### Run it locally

```bash
# 1. Start Postgres
docker compose up -d

# 2. Configure env (see config.rs for the full set)
cp .env.example .env   # then fill in keys
# Required-ish:
#   DATABASE_URL=postgres://patom:patom@localhost:5432/patom
#   ANTHROPIC_API_KEY=...
#   OPENAI_API_KEY=...        # only if you want OpenAI / embeddings
#   PATOM_JWT_SECRET=...      # session cookies
#   PATOM_ORG_KEK=...         # MCP credential envelope key

# 3. Run migrations + start the server
cargo run --release

# 4. (optional) Web UI
cd web && bun install && bun run dev
```

The HTTP server boots on `0.0.0.0:8080` by default; the web UI dev server proxies API calls to it.

### Run the tests

Patom is TDD-first (see [`CLAUDE.md`](./CLAUDE.md) §3). Integration tests use real Postgres via `#[sqlx::test]`.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

All four gates (`fmt`, `clippy`, `check`, `test`) must be green before a PR lands.

---

## Engineering rules

Patom inherits its style from TigerBeetle's TIGER_STYLE and NASA's Power of Ten. The full ruleset — types-encode-invariants, no recursion, every loop has a bound, assertions crash the process, one error type per module boundary, etc. — is in [`CLAUDE.md`](./CLAUDE.md). It's binding, not advisory.

If you're contributing, read it before you read any module.

---

## Project layout

```
.
├── CLAUDE.md             # engineering rules (binding)
├── Cargo.toml            # workspace + lockstep deps
├── docker-compose.yml    # Postgres + pgvector
├── migrations/           # sqlx migrations (paired up/down)
├── src/                  # Rust crate
├── web/                  # web UI (Bun + TypeScript)
├── doc/                  # design notes, plans, pitch material
└── tests/                # integration tests
```

---

## Contributing

We welcome contributions. Start with [`CONTRIBUTING.md`](./CONTRIBUTING.md). Security issues go through [`SECURITY.md`](./SECURITY.md) — please don't open public issues for vulnerabilities.

By participating you agree to our [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md).

---

## License

Patom is **source-available** under the [Functional Source License, Version 1.1, with an Apache 2.0 Future License](./LICENSE.md) (`FSL-1.1-Apache-2.0`).

In plain English:

- **You may** use, modify, self-host, and redistribute Patom for any purpose that is not a Competing Use — internal use, non-commercial education and research, and professional services delivered to licensees are all explicitly permitted.
- **You may not** use Patom to offer a commercial product or service that substitutes for Patom or for any product/service we offer using Patom (i.e. you can't take this repo and run it as a competing SaaS).
- **Two years after each release**, that release automatically converts to the [Apache License 2.0](http://www.apache.org/licenses/LICENSE-2.0). The non-compete is time-limited, not perpetual.

If you need a commercial license that lifts the Competing Use restriction sooner, contact the maintainers.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in Patom by you shall be licensed under the same FSL-1.1-Apache-2.0 terms, without any additional terms or conditions.
