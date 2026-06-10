import tailwind from "bun-plugin-tailwind";

const result = await Bun.build({
  entrypoints: ["./index.html"],
  outdir: "./dist",
  minify: true,
  // Split the dynamic `import("posthog-js")` in `src/lib/analytics.ts` into its
  // own chunk. Without this, Bun inlines dynamic imports into the main bundle,
  // so the vendor would ship to every user even in key-less builds; with it,
  // the chunk is only fetched once analytics is enabled.
  splitting: true,
  // Inline the PostHog key/host (from BUN_PUBLIC_* env vars) as static literals
  // the analytics seam reads via `globalThis.__POSTHOG_*__` (src/lib/
  // analytics.ts). `define` is deliberate over Bun's `env: "BUN_PUBLIC_*"`
  // option, which only substitutes vars that are *set* and leaves a bare
  // reference otherwise. Reading through `globalThis.*` (not `process.env.*`)
  // means an un-replaced reference — e.g. in `bun dev`, which does no
  // substitution and has no `process` — is a harmless `undefined` property
  // access, not a throw. Unset here ⇒ "" ⇒ analytics no-ops (OSS / self-host).
  define: {
    "globalThis.__POSTHOG_KEY__": JSON.stringify(
      process.env.BUN_PUBLIC_POSTHOG_KEY ?? "",
    ),
    "globalThis.__POSTHOG_HOST__": JSON.stringify(
      process.env.BUN_PUBLIC_POSTHOG_HOST ?? "",
    ),
  },
  plugins: [tailwind],
});

if (!result.success) {
  console.error(result.logs);
  process.exit(1);
}

for (const o of result.outputs) {
  console.log(`  ${o.path.replace(import.meta.dir + "/", "")}`);
}
