# Security policy

Patom is pre-1.0 and treats security bugs as the highest-priority class of bug. Thank you for taking the time to disclose responsibly.

## Reporting a vulnerability

**Please do not open a public GitHub issue.**

Email the maintainers at **security@patom.dev** (or, until that alias is live, open a private GitHub Security Advisory at <https://github.com/tomkapa/patom/security/advisories/new>).

In your report, include as much of the following as you can:

- A description of the issue and its impact.
- The component(s) involved (`src/<module>`, migration number, web route, MCP integration, etc.).
- A minimal proof-of-concept or reproduction steps.
- The commit SHA / version you tested against.
- Whether the issue has been disclosed anywhere else.

We will acknowledge your report within **3 business days** and aim to provide a status update within **10 business days**. Coordinated disclosure timelines depend on severity; we will agree on a public disclosure date with you before publishing any advisory.

## Scope

In scope:

- The Rust crate (`src/`) and its public HTTP surface.
- The database schema (`migrations/`) and any RLS or tenancy boundary.
- The web UI (`web/`).
- The MCP integration layer, including OAuth flow and credential storage.
- Slack bridge signature verification and webhook handling.
- Per-org envelope encryption (`src/crypto/`).

Out of scope (please don't report):

- Vulnerabilities in third-party services Patom integrates with (Notion, Gmail, Slack, etc.) — report those to the vendor.
- Issues that require a privileged operator already inside an organization (e.g. an org admin acting maliciously toward members of the same org).
- Denial-of-service requiring sustained, unrealistic load.
- Findings in dependencies for which there is no exploitable code path in Patom — we accept advisories via `cargo audit` and address them on the normal release cadence.

## What we care about

Patom's security model has a few load-bearing invariants. Reports that demonstrate breaks here will be prioritised:

1. **Tenant isolation.** No org may read or write another org's rows. RLS plus `WHERE org_id` are both defences — bypassing either is a vulnerability.
2. **Per-agent tool scoping.** An agent must not be able to invoke a tool that isn't in its `allowed_mcp_servers` / `allowed_mcp_tools`. The scoping is enforced in `src/mcp/scoped.rs` and must hold across every code path — including reflection, scheduled tasks, and `send_message` fan-out.
3. **Per-agent memory scoping.** One agent's memory must not leak into another agent's render, even within the same org and session.
4. **MCP credential confidentiality.** Upstream OAuth tokens and bearer tokens are envelope-encrypted per-org. Plaintext must never appear in logs, traces, error messages, or panic backtraces.
5. **SQL injection.** All queries go through `sqlx` bound parameters or compile-time checked `query!` / `query_as!`. Any `format!`-into-SQL is a vulnerability (see [`CLAUDE.md`](./CLAUDE.md) §10).
6. **Webhook signature verification.** Slack and any future webhook source must be verified with constant-time HMAC compare before any side effect.
7. **Cookie / session safety.** Session JWTs use HS256 with a configured secret; CSRF is enforced on state-changing routes.
8. **Asset upload boundary.** Uploaded bytes are magic-byte sniffed before being handed to R2; SVG / HTML masquerading as `image/png` is a vulnerability.

## Hardening defaults

- `unsafe_code = "forbid"` workspace-wide.
- `panic = "abort"` in release — a corrupted invariant terminates the process; the lease expires and another worker resumes.
- `cargo deny` and `cargo audit` run in CI; advisory failures block merge.
- No floating tokio tasks; every channel is bounded; every I/O `await` is wrapped in a timeout (see [`CLAUDE.md`](./CLAUDE.md) §5).

## Hall of fame

We will list (with permission) researchers who report valid vulnerabilities here once the project has had any.
