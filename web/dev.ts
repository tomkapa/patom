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

const server = Bun.serve({
  port: 5173,
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
