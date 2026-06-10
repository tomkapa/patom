-- org_credit_ledger / org_credits — per-org credit balance + append-only ledger (#154).
--
-- Launch pricing is postponed: workspaces run free on the platform key against a
-- credit balance, and a zero balance gates further platform inference. Credits
-- are a reusable billing primitive (grants, promos, referrals, refunds, later
-- top-ups) — all of which flow through one append-only ledger:
--
--   * `org_credit_ledger` is the immutable history. Every grant / debit /
--     adjustment is one signed row. It is never UPDATEd or DELETEd.
--   * `org_credits` is the materialized hot-path balance, kept in lockstep with
--     the ledger in one transaction (invariant: balance == granted − used).
--
-- Both key directly on org_id, so RLS alone isolates tenants. The worker settle /
-- grant paths use begin_privileged (RLS-bypassing, owner role); tenant reads use
-- begin_as_user (the non-super `patom_app` role) so RLS applies.
--
-- Pre-launch single-step migration: new empty tables, no backfill.

CREATE TABLE org_credit_ledger (
    id               UUID PRIMARY KEY,
    org_id           UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- Signed: grants are positive, debits negative. No CHECK — sign is meaning.
    delta_micro_usd  BIGINT NOT NULL,
    kind             TEXT NOT NULL CHECK (kind IN ('grant', 'debit', 'adjustment')),
    reason           TEXT NOT NULL
                     CHECK (reason IN ('signup_bonus', 'promo', 'referral', 'manual', 'refund', 'usage')),
    -- Unique when present — the dedup key for idempotent grants (e.g.
    -- `signup:{org_id}`). NULL is allowed and (per the SQL standard) several
    -- NULLs coexist, so non-idempotent usage debits need not invent a key.
    idempotency_key  TEXT UNIQUE,
    -- The user/system that caused the entry. NULL = System (convention).
    actor            UUID,
    created_at       TIMESTAMPTZ NOT NULL
);

-- The recent-ledger read API pages newest-first within an org.
CREATE INDEX org_credit_ledger_org_created_idx ON org_credit_ledger (org_id, created_at DESC);

CREATE TABLE org_credits (
    org_id                  UUID PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    -- Signed: post-paid settle may dip the final turn slightly negative.
    balance_micro_usd       BIGINT NOT NULL,
    granted_total_micro_usd BIGINT NOT NULL DEFAULT 0 CHECK (granted_total_micro_usd >= 0),
    used_total_micro_usd    BIGINT NOT NULL DEFAULT 0 CHECK (used_total_micro_usd >= 0),
    updated_at              TIMESTAMPTZ NOT NULL
);

ALTER TABLE org_credit_ledger ENABLE ROW LEVEL SECURITY;
ALTER TABLE org_credit_ledger FORCE  ROW LEVEL SECURITY;
ALTER TABLE org_credits       ENABLE ROW LEVEL SECURITY;
ALTER TABLE org_credits       FORCE  ROW LEVEL SECURITY;

CREATE POLICY org_credit_ledger_org_isolation ON org_credit_ledger
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

CREATE POLICY org_credits_org_isolation ON org_credits
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- Append-only: migration 14 grants the tenant role full CRUD by default, so we
-- explicitly REVOKE mutation on the ledger. The tenant role may INSERT (new
-- entries) and SELECT (the read API) but never UPDATE or DELETE history. The
-- privileged worker runs as the owner role and is unaffected (it, too, only
-- ever appends). `org_credits` keeps full CRUD — it is the mutable balance.
REVOKE UPDATE, DELETE ON org_credit_ledger FROM patom_app;
