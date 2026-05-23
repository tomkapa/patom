// Backend-driven helpers for MCP catalog metadata. The authoritative
// catalog listing lives on the backend (`GET /mcp-catalog`,
// `src/mcp/catalog.rs::McpCatalogEntry`); this file only exposes the
// `Map<catalog_id, entry>` lookup the FE consumers use.

import type { McpCatalogEntry } from "../types/api";

export type CatalogLookup = (id: string) => McpCatalogEntry | undefined;

/** Build a `catalog_id → row` lookup from the raw `useMcpCatalog` array.
 *  Returns `undefined` for unknown ids — the caller falls back to the
 *  server's raw `catalog_id` for the display label. */
export function buildCatalogLookup(
  rows: McpCatalogEntry[] | undefined,
): CatalogLookup {
  const map = new Map((rows ?? []).map((row) => [row.catalog_id, row]));
  return (id) => map.get(id);
}
