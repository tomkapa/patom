// Visual + API-token wiring metadata for well-known MCP catalog ids.
// The authoritative catalog listing now lives on the backend
// (`GET /mcp-catalog`); this file supplies what the wire shape does
// not — brand monogram/colors, `iconSlug` for `simple-icons`, and the
// `apiTokenHeader`/`apiTokenPrefix`/`apiTokenHelpUrl` triple that the
// API-token flow needs to build the static-headers credential. Keyed
// by `catalog_id` and looked up via `entryById` / `entryForServer`.

export type CatalogCategory =
  | "productivity"
  | "dev"
  | "comms"
  | "data"
  | "custom";

export type CatalogEntry = {
  /** Stable slug — also used as the MCP server `alias` seed. */
  id: string;
  name: string;
  blurb: string;
  category: CatalogCategory;
  /** Single character or short string rendered when no brand icon is available. */
  monogram: string;
  /** Background hex for the tile + table monogram. */
  tileBg: string;
  /** Foreground hex for the tile glyph. */
  tileFg: string;
  /** simple-icons slug for the brand icon. When present, the icon image
   *  is shown instead of the monogram character. Slug must match a key in
   *  `src/data/brandIcons.ts` (all lowercase, no spaces). */
  iconSlug?: string;
  /** Canonical hosted MCP URL. The user can override it before submit. */
  defaultUrl: string;
  auth: "oauth" | "apiToken";
  apiTokenHeader?: string;
  apiTokenPrefix?: string;
  /** Optional vendor-side help page for "Where do I find this?" link. */
  apiTokenHelpUrl?: string;
  /** Approximate count for the catalog tile chip. */
  toolCount?: number;
};

export const MCP_CATALOG: CatalogEntry[] = [
  {
    id: "notion",
    name: "Notion",
    blurb: "Read & write pages, databases, comments.",
    category: "productivity",
    monogram: "N",
    tileBg: "#000000",
    tileFg: "#FFFFFF",
    iconSlug: "notion",
    defaultUrl: "https://mcp.notion.com/mcp",
    auth: "oauth",
    toolCount: 12,
  },
  {
    id: "linear",
    name: "Linear",
    blurb: "Create issues, query projects, manage cycles.",
    category: "productivity",
    monogram: "L",
    tileBg: "#5E6AD2",
    tileFg: "#FFFFFF",
    iconSlug: "linear",
    defaultUrl: "https://mcp.linear.app/mcp",
    auth: "oauth",
    toolCount: 14,
  },
  {
    id: "github",
    name: "GitHub",
    blurb: "Repos, issues, pull requests, actions.",
    category: "dev",
    monogram: "G",
    tileBg: "#181717",
    tileFg: "#FFFFFF",
    iconSlug: "github",
    defaultUrl: "https://api.githubcopilot.com/mcp/",
    auth: "apiToken",
    apiTokenHeader: "Authorization",
    apiTokenPrefix: "Bearer ",
    apiTokenHelpUrl: "https://github.com/settings/tokens?type=beta",
    toolCount: 21,
  },
  {
    id: "slack",
    name: "Slack",
    blurb: "Post messages, search channels, read threads.",
    category: "comms",
    monogram: "S",
    tileBg: "#4A154B",
    tileFg: "#FFFFFF",
    iconSlug: "slack",
    defaultUrl: "https://mcp.slack.com/mcp",
    auth: "oauth",
    toolCount: 9,
  },
  {
    id: "google-drive",
    name: "Google Drive",
    blurb: "Read, write, and share docs and sheets.",
    category: "productivity",
    monogram: "D",
    tileBg: "#4285F4",
    tileFg: "#FFFFFF",
    iconSlug: "googledrive",
    defaultUrl: "https://mcp.googleapis.com/drive",
    auth: "oauth",
    toolCount: 8,
  },
  {
    id: "figma",
    name: "Figma",
    blurb: "Pull files, comments, and component metadata.",
    category: "productivity",
    monogram: "F",
    tileBg: "#F24E1E",
    tileFg: "#FFFFFF",
    iconSlug: "figma",
    defaultUrl: "https://api.figma.com/v1/mcp",
    auth: "apiToken",
    apiTokenHeader: "X-Figma-Token",
    apiTokenPrefix: "",
    apiTokenHelpUrl:
      "https://help.figma.com/hc/en-us/articles/8085703771159-Manage-personal-access-tokens",
    toolCount: 6,
  },
  {
    id: "jira",
    name: "Jira",
    blurb: "Tickets, sprints, backlog, JQL search.",
    category: "productivity",
    monogram: "J",
    tileBg: "#0052CC",
    tileFg: "#FFFFFF",
    iconSlug: "jira",
    defaultUrl: "https://api.atlassian.com/ex/jira/mcp",
    auth: "apiToken",
    apiTokenHeader: "Authorization",
    apiTokenPrefix: "Bearer ",
    apiTokenHelpUrl:
      "https://id.atlassian.com/manage-profile/security/api-tokens",
    toolCount: 16,
  },
  {
    id: "sentry",
    name: "Sentry",
    blurb: "Errors, releases, and performance traces.",
    category: "dev",
    monogram: "S",
    tileBg: "#362D59",
    tileFg: "#FFFFFF",
    iconSlug: "sentry",
    defaultUrl: "https://mcp.sentry.dev/v1",
    auth: "apiToken",
    apiTokenHeader: "Authorization",
    apiTokenPrefix: "Bearer ",
    apiTokenHelpUrl: "https://docs.sentry.io/account/auth-tokens/",
    toolCount: 11,
  },
  {
    id: "stripe",
    name: "Stripe",
    blurb: "Customers, subscriptions, invoices, payouts.",
    category: "data",
    monogram: "S",
    tileBg: "#635BFF",
    tileFg: "#FFFFFF",
    iconSlug: "stripe",
    defaultUrl: "https://api.stripe.com/v1/mcp",
    auth: "apiToken",
    apiTokenHeader: "Authorization",
    apiTokenPrefix: "Bearer ",
    apiTokenHelpUrl: "https://dashboard.stripe.com/apikeys",
    toolCount: 18,
  },
  {
    id: "postgres",
    name: "PostgreSQL",
    blurb: "Query, insert, and inspect schemas.",
    category: "data",
    monogram: "P",
    tileBg: "#4169E1",
    tileFg: "#FFFFFF",
    iconSlug: "postgresql",
    defaultUrl: "https://mcp.example.com/postgres",
    auth: "apiToken",
    apiTokenHeader: "Authorization",
    apiTokenPrefix: "Bearer ",
    toolCount: 7,
  },
  {
    id: "hubspot",
    name: "HubSpot",
    blurb: "Contacts, deals, lifecycle properties.",
    category: "data",
    monogram: "H",
    tileBg: "#FF7A59",
    tileFg: "#FFFFFF",
    iconSlug: "hubspot",
    defaultUrl: "https://api.hubapi.com/mcp/v1",
    auth: "oauth",
    toolCount: 10,
  },
];

export function entryById(id: string): CatalogEntry | undefined {
  return MCP_CATALOG.find((e) => e.id === id);
}

/** Match a saved MCP server back to its catalog entry. The backend
 *  `catalog_id` field is the authoritative tie (it's the FK target);
 *  the URL-host fallback covers tenant-custom rows whose `catalog_id`
 *  doesn't exist as a tile in this hard-coded visual catalog. */
export function entryForServer(server: {
  catalog_id: string;
  config: { type: "http"; url: string };
}): CatalogEntry | undefined {
  const byId = entryById(server.catalog_id);
  if (byId) return byId;
  try {
    const host = new URL(server.config.url).host;
    return MCP_CATALOG.find((e) => new URL(e.defaultUrl).host === host);
  } catch {
    return undefined;
  }
}
