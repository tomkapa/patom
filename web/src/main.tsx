import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
// `QueryClient` is sourced directly from query-core because Bun's dev
// bundler does not hoist transitive `export *` re-exports correctly — the
// react-query barrel resolves `QueryClient` to `undefined` only at runtime
// in `bun dev`. Production (`bun build`) is unaffected.
import { QueryClientProvider } from "@tanstack/react-query";
import { QueryClient } from "@tanstack/query-core";
import { BrowserRouter } from "react-router-dom";
import { App } from "./App";
import { applyTheme, getStoredTheme } from "./lib/theme";
import { init as initAnalytics } from "./lib/analytics";

// Apply persisted theme before first paint to avoid a light→dark flash.
applyTheme(getStoredTheme());

// Boot product analytics. No-op unless BUN_PUBLIC_POSTHOG_KEY is set.
initAnalytics();

const queryClient = new QueryClient();

const rootElement = document.getElementById("root");
if (!rootElement) throw new Error("invariant: #root must exist");

createRoot(rootElement).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>
        <App />
      </BrowserRouter>
    </QueryClientProvider>
  </StrictMode>,
);
