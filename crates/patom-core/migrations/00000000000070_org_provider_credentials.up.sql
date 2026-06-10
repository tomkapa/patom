-- org_provider_credentials — per-org BYO LLM provider API keys (#141).
--
-- The free-credit gate (#154) blocks platform inference at a zero balance; this
-- table is the escape valve: a workspace stores its own provider key and that
-- provider's turns route to the BYO client (no credit debit) immediately. A
-- workspace may hold a key for several providers at once (union of keyed
-- providers) — one row per (org, provider).
--
-- Mirrors the `mcp_server_credentials` seam (migration 21): the secret API key
-- is envelope-encrypted (AES-256-GCM under a per-org KEK derived from
-- `PATOM_MASTER_KEK`; see `src/crypto`), RLS-scoped by org, never returned in
-- plaintext. Unlike MCP credentials there is a single payload shape (just the
-- key), so no `kind` column is needed. `base_url` is a non-secret endpoint
-- override and lives in a plaintext column.
--
-- Pre-launch single-step migration: new empty table, no backfill.

CREATE TABLE org_provider_credentials (
    org_id            UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- Low-cardinality provider discriminator; matches `ProviderId::as_str`.
    provider          TEXT NOT NULL CHECK (provider IN ('anthropic', 'openai', 'deepseek')),
    ciphertext        BYTEA NOT NULL,
    nonce             BYTEA NOT NULL CHECK (octet_length(nonce) = 12),
    key_version       SMALLINT NOT NULL DEFAULT 1,
    -- Non-secret endpoint override (proxy / compatible gateway). NULL = the
    -- provider's public default.
    base_url          TEXT,
    -- Last time the key was validated against the live provider (informational;
    -- drives the UI status badge). NULL until first validated.
    last_validated_at TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL,
    updated_at        TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, provider)
);

ALTER TABLE org_provider_credentials ENABLE ROW LEVEL SECURITY;
ALTER TABLE org_provider_credentials FORCE  ROW LEVEL SECURITY;
CREATE POLICY org_provider_credentials_org_isolation ON org_provider_credentials
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
