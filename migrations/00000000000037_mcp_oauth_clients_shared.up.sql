-- Shared (platform-owned) OAuth clients on `mcp_oauth_clients`.
--
-- Background. Google's MCP servers (gmailmcp/calendarmcp) auth via
-- `https://accounts.google.com/`, which does NOT support RFC 7591
-- Dynamic Client Registration. Same will be true of Microsoft 365 when
-- it lands. The product fix is one OAuth client owned by the Patom
-- operator (e.g. the same client_id/secret already wired into "Login
-- with Google"), reused across every tenant — Anthropic's pattern for
-- Claude Desktop's Gmail connector.
--
-- Schema change.
--   * `org_id` becomes nullable. A NULL row is a shared platform client;
--     a non-NULL row is per-org (DCR or operator-provisioned) and
--     behaves unchanged.
--   * The composite PK `(org_id, issuer)` cannot hold NULL on org_id,
--     so we drop it and rely on two partial unique indexes that
--     together enforce: at most one shared row per issuer; at most one
--     row per (org, issuer) pair. No FKs target this table (greped
--     migrations/), so dropping the PK is safe.
--   * Provenance is inferred from data shape, not stored:
--       shared    ⇔ org_id IS NULL
--       dcr       ⇔ org_id IS NOT NULL AND registration_client_uri IS NOT NULL
--       operator  ⇔ org_id IS NOT NULL AND registration_client_uri IS NULL
--     so no new column.
--
-- RLS. Mirrors `mcp_catalog`'s global/org-scoped split:
--   * SELECT: shared rows readable by any authenticated principal;
--     org-scoped rows readable only by that org's members.
--   * INSERT/UPDATE/DELETE: only on rows your org owns. Shared rows are
--     written exclusively by the boot-time seeder, which runs under
--     `run_privileged` (RLS off).
--
-- Encryption note. The existing `OrgEncryptor` derives a per-org KEK via
-- HKDF(master, salt=org_uuid). Shared rows have no org, so the seeder
-- and store seal/open under the nil UUID as a platform sentinel. The
-- nil UUID can never collide with a real org (created via
-- `gen_random_uuid()` / `Uuid::new_v4`).
--
-- CLAUDE.md §14: paired down restores the original NOT NULL + composite PK.

ALTER TABLE mcp_oauth_clients DROP CONSTRAINT mcp_oauth_clients_pkey;

ALTER TABLE mcp_oauth_clients ALTER COLUMN org_id DROP NOT NULL;

CREATE UNIQUE INDEX mcp_oauth_clients_org_issuer_key
    ON mcp_oauth_clients (org_id, issuer) WHERE org_id IS NOT NULL;

CREATE UNIQUE INDEX mcp_oauth_clients_shared_issuer_key
    ON mcp_oauth_clients (issuer) WHERE org_id IS NULL;

-- Replace the single `FOR ALL` org-isolation policy with per-operation
-- policies that admit shared (NULL) rows on SELECT only.
DROP POLICY mcp_oauth_clients_org_isolation ON mcp_oauth_clients;

CREATE POLICY mcp_oauth_clients_select ON mcp_oauth_clients
    FOR SELECT TO PUBLIC
    USING (org_id IS NULL OR app_user_is_member(org_id));

CREATE POLICY mcp_oauth_clients_insert ON mcp_oauth_clients
    FOR INSERT TO PUBLIC
    WITH CHECK (org_id IS NOT NULL AND app_user_is_member(org_id));

CREATE POLICY mcp_oauth_clients_update ON mcp_oauth_clients
    FOR UPDATE TO PUBLIC
    USING      (org_id IS NOT NULL AND app_user_is_member(org_id))
    WITH CHECK (org_id IS NOT NULL AND app_user_is_member(org_id));

CREATE POLICY mcp_oauth_clients_delete ON mcp_oauth_clients
    FOR DELETE TO PUBLIC
    USING (org_id IS NOT NULL AND app_user_is_member(org_id));
