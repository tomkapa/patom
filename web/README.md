# Patom web

Frontend for the chat UI. See `doc/frontend_plan.md` for the full plan.

## Dev

In one terminal, run the Rust backend:

```sh
cargo run
```

In another:

```sh
cd web
bun install
bun run dev
```

Open <http://localhost:5173>. The dev server proxies `/api/*`,
`/auth/google/*`, and `/mcp-oauth/*` to the Rust backend on `:8080`
(override with `BACKEND_URL=...`); everything else serves the bundled
SPA shell so deep links work on hard refresh.

## Build

```sh
bun run build
```

Produces `dist/`. The Rust binary serves it via `tower-http`'s
`ServeDir` with `index.html` as the SPA fallback. Override the served
directory with `PATOM_WEB_DIST=/abs/path` (default `./web/dist`).

## Typecheck

```sh
bun run typecheck
```
