import index from "./index.html";

const BACKEND = process.env.BACKEND_URL ?? "http://localhost:8080";

// Pure pass-through. `/api/*` must not be stripped — BE listens there
// natively (`src/http/routes/mod.rs`). Everything else falls to the
// bundled `index` route so deep-links resolve to the SPA shell.
const forwardAsIs = (req: Request): Promise<Response> => {
  const url = new URL(req.url);
  const target = `${BACKEND}${url.pathname}${url.search}`;
  return fetch(target, {
    method: req.method,
    headers: req.headers,
    body: req.body,
    redirect: "manual",
    // @ts-expect-error - duplex is required for streaming bodies in fetch
    duplex: "half",
  });
};

// Honor PORT (set by the preview harness' autoPort) but fall back to 5173
// on anything non-numeric or out of range, so a bad env can't crash startup.
const DEFAULT_PORT = 5173;
const parsePort = (raw: string | undefined): number => {
  if (!raw) return DEFAULT_PORT;
  const n = Number.parseInt(raw, 10);
  return Number.isInteger(n) && n >= 1 && n <= 65535 ? n : DEFAULT_PORT;
};

const server = Bun.serve({
  port: parsePort(process.env.PORT),
  development: true,
  routes: {
    "/api/*": forwardAsIs,
    "/auth/oidc/*": forwardAsIs,
    "/mcp-oauth/*": forwardAsIs,
    "/*": index,
  },
});

console.log(`web dev → http://localhost:${server.port}`);
console.log(`proxy   → ${BACKEND}`);
