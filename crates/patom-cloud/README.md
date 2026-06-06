# patom-cloud

Paid-tier / billing code for Patom. Part of the open-core split (issue #133).

## Licensing

Licensed under `LicenseRef-FSL-1.1-Apache-2.0`, same as the rest of the repo
(see the root [`LICENSE.md`](../../LICENSE.md)). The source is public — under
FSL a competitor still may not resell Patom — but this crate is **not part of
the default build**.

## Why it's a separate crate

`patom-cloud` is compiled **only** when the `patom-server` binary is built with
its `cloud` feature (`cargo build -p patom-server --features cloud`). The default
`cargo build` — the OSS / self-host binary — links none of it. The crate
boundary is the compiler enforcing that commercial code can never leak into the
free product. (The binary lives in `patom-server`, not `patom-core`, so that
`patom-cloud` can depend on `patom-core` without forming a dependency cycle.)

A CI guard (`cargo tree -p patom-server`) asserts `patom-cloud` is absent from
the default binary's dependency graph.

## What belongs here vs in patom-core

| Goes in `patom-cloud` | Stays in `patom-core` |
|---|---|
| Charging / payment integration (Lemon Squeezy, #131) | The product itself (agents, sessions, MCP, auth, orgs) |
| The concrete `Entitlements` implementation backed by billing (#134) | The `Entitlements` **trait** + free-tier stub (the seam, #134) |
| `billing`-schema migrations + stores | Core schema migrations |

Rule of thumb: if a self-hoster on the free tier shouldn't run it, it lives
here. The seam (a trait) lives in core; the paid *answer* lives here.

## Status

Empty scaffold. Entitlement impl → #134. Lemon Squeezy billing → #131.
