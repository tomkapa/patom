-- Cloud-schema billing tables (#131). The `cloud` schema and its
-- `cloud._sqlx_migrations` tracking table are set up by patom-cloud's own
-- Migrator (crates/patom-cloud/src/migrate.rs), off the core migration stream
-- so billing can move to its own database later.

-- Subscriptions: one row per Lemon Squeezy subscription, keyed to an org.
-- `org_id` is a plain UUID with NO foreign key to public.organizations — a
-- cross-schema FK would block splitting billing into its own database, so
-- referential integrity is enforced at the app layer. No card data is stored;
-- Lemon Squeezy (the Merchant of Record) owns it. We keep only ids + status.
CREATE TABLE cloud.subscriptions (
    id                 UUID PRIMARY KEY,
    org_id             UUID NOT NULL,
    ls_customer_id     TEXT,
    ls_subscription_id TEXT NOT NULL,
    ls_variant_id      TEXT,
    plan               TEXT NOT NULL,
    status             TEXT NOT NULL,
    current_period_end TIMESTAMPTZ,
    created_at         TIMESTAMPTZ NOT NULL,
    updated_at         TIMESTAMPTZ NOT NULL
);

-- The Lemon Squeezy subscription id is the natural key for webhook upserts.
CREATE UNIQUE INDEX subscriptions_ls_subscription_id_key
    ON cloud.subscriptions (ls_subscription_id);
-- The entitlement gate resolves the active subscription per org.
CREATE INDEX subscriptions_org_id_idx ON cloud.subscriptions (org_id);

-- Per-org RLS, mirroring every core domain table (migration 18): visibility is
-- gated by org membership via the public SECURITY DEFINER helper from
-- migration 14. The webhook writes privileged (RLS off) since it has no user
-- principal; user reads go through the membership policy.
ALTER TABLE cloud.subscriptions ENABLE ROW LEVEL SECURITY;
ALTER TABLE cloud.subscriptions FORCE ROW LEVEL SECURITY;
CREATE POLICY subscriptions_org_isolation ON cloud.subscriptions
    FOR ALL TO PUBLIC
    USING      (public.app_user_is_member(org_id))
    WITH CHECK (public.app_user_is_member(org_id));

-- Webhook idempotency ledger: one row per processed Lemon Squeezy event id, so
-- a redelivered event is applied exactly once. Written privileged from the
-- webhook handler; `org_id` is informational (NULL until/if the event maps to
-- an org).
CREATE TABLE cloud.webhook_events (
    ls_event_id TEXT PRIMARY KEY,
    org_id      UUID,
    received_at TIMESTAMPTZ NOT NULL
);

ALTER TABLE cloud.webhook_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE cloud.webhook_events FORCE ROW LEVEL SECURITY;
CREATE POLICY webhook_events_org_isolation ON cloud.webhook_events
    FOR ALL TO PUBLIC
    USING      (org_id IS NULL OR public.app_user_is_member(org_id))
    WITH CHECK (org_id IS NULL OR public.app_user_is_member(org_id));

-- Core's grants (migration 14) only cover the `public` schema, so the
-- tenant-scoped `patom_app` role (used by `auth::begin_as*`) needs explicit
-- access to read subscriptions under the RLS policy above — for a future
-- "billing status for my org" read. Writes stay privileged (the webhook only),
-- and the idempotency ledger is internal, so neither gets a grant here.
-- `patom_app` is created by core migration 14, which always runs first.
GRANT USAGE ON SCHEMA cloud TO patom_app;
GRANT SELECT ON cloud.subscriptions TO patom_app;
