-- Workspace settings: invite-by-email surface.
--
-- One row per pending invite. A row is "pending" while
-- `consumed_at IS NULL` and `expires_at > now()`. The single-use URL
-- token is stored hashed (BYTEA, 32-byte SHA-256 of the URL-safe
-- base64 raw secret); the FE only ever sees the cleartext at the
-- moment of issuance. The store rotates the secret on resend so a
-- leaked link can be invalidated by re-sending.
--
-- RLS lives on `org_id` via the existing `app_user_is_member` helper
-- so admins of one workspace cannot enumerate invites in another.
-- Acceptance runs through the privileged store (table owner, RLS
-- off) because the accepting user is by definition not yet a
-- member; see `src/auth/invites.rs::PgInviteStore::accept`.

CREATE TABLE org_invites (
    id          UUID PRIMARY KEY,
    org_id      UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    email       CITEXT NOT NULL CHECK (octet_length(email) BETWEEN 3 AND 320),
    role        TEXT NOT NULL CHECK (role IN ('owner','admin','member')),
    token_hash  BYTEA NOT NULL CHECK (octet_length(token_hash) = 32),
    invited_by  UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL,
    expires_at  TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ
);

-- One pending invite per (org, email). A second invite for the same
-- email overwrites the pending row (UPSERT in the store) rather than
-- piling up rows. Consumed rows are kept as audit history.
CREATE UNIQUE INDEX org_invites_org_email_pending_idx
    ON org_invites (org_id, email)
    WHERE consumed_at IS NULL;

-- Token-hash lookup for the accept endpoint. Hash is a perfect
-- index — equality only, no range scans.
CREATE UNIQUE INDEX org_invites_token_hash_idx ON org_invites (token_hash);

CREATE INDEX org_invites_org_status_idx ON org_invites (org_id, consumed_at, expires_at);

ALTER TABLE org_invites ENABLE ROW LEVEL SECURITY;
ALTER TABLE org_invites FORCE ROW LEVEL SECURITY;
CREATE POLICY org_invites_org_isolation ON org_invites
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
