declare module "*.png" {
  const src: string;
  export default src;
}

// Build-time client config. `bun build` replaces these with string literals
// via `define` (see `build.ts`), sourced from the `BUN_PUBLIC_POSTHOG_*` env
// vars. Declared `string | undefined`: in `bun dev` (no `define`) the property
// is simply absent, and `src/lib/analytics.ts` reads it through `globalThis`
// so the un-replaced case is a safe `undefined`, never a throw.
declare var __POSTHOG_KEY__: string | undefined;
declare var __POSTHOG_HOST__: string | undefined;
