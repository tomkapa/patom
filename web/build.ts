import tailwind from "bun-plugin-tailwind";

const result = await Bun.build({
  entrypoints: ["./index.html"],
  outdir: "./dist",
  minify: true,
  // Split the dynamic `import("posthog-js")` in `src/lib/analytics.ts` into its
  // own chunk. Without this, Bun inlines dynamic imports into the main bundle,
  // so the vendor would ship to every user even in key-less builds. With
  // splitting enabled the chunk is a separate file only fetched at runtime when
  // `enabled` is true (i.e. `window.__PATOM_CONFIG__.posthogKey` is non-empty).
  splitting: true,
  plugins: [tailwind],
});

if (!result.success) {
  console.error(result.logs);
  process.exit(1);
}

for (const o of result.outputs) {
  console.log(`  ${o.path.replace(import.meta.dir + "/", "")}`);
}
