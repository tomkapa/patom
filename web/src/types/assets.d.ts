declare module "*.png" {
  const src: string;
  export default src;
}

// Runtime config injected by the Rust server into `index.html` before `</head>`
// at startup. Read synchronously by `src/lib/analytics.ts` — no fetch roundtrip.
// Absent (or key is "") → analytics is a hard no-op (OSS / self-host).
interface Window {
  __PATOM_CONFIG__?: { posthogKey?: string; posthogHost?: string };
}
