-- org_budgets / org_budget_usage — per-org monthly spend cap in micro-USD.
--
-- Patom caps *turns* per DAG (prompt_request_dags, migration 4) and records
-- per-turn token counts (turn_metrics, migration 44) but nothing converts that
-- to money or stops spend at a limit. These two tables add a per-org monthly
-- budget: `org_budgets` holds the cap + warn threshold (config), and
-- `org_budget_usage` is the atomic per-period counter the worker settles into
-- after each provider call (see src/budget/service.rs).
--
-- Both tables key directly on org_id, so RLS alone isolates tenants — no
-- denormalised-org enforcement trigger is needed (unlike turn_metrics, which
-- carried org_id alongside session_id).
--
-- Pre-launch single-step migration: new empty tables, no backfill, no online
-- migration risk (feedback_no_backcompat).

CREATE TABLE org_budgets (
    org_id                UUID PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    -- NULL = unlimited (no cap configured). A configured cap must be positive.
    monthly_cap_micro_usd BIGINT
                           CHECK (monthly_cap_micro_usd IS NULL OR monthly_cap_micro_usd > 0),
    -- Soft warn threshold in basis points (8000 = 80%). 1..=10000.
    warn_threshold_bps    INTEGER NOT NULL DEFAULT 8000
                           CHECK (warn_threshold_bps BETWEEN 1 AND 10000),
    created_at            TIMESTAMPTZ NOT NULL,
    updated_at            TIMESTAMPTZ NOT NULL
);

CREATE TABLE org_budget_usage (
    org_id          UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- First day of the billing month, UTC. Computed app-side from the Clock
    -- (CLAUDE.md §11) so tests pin the period boundary deterministically.
    period_start    DATE NOT NULL,
    used_micro_usd  BIGINT NOT NULL DEFAULT 0 CHECK (used_micro_usd >= 0),
    -- Set once per period when usage first crosses warn_threshold_bps.
    -- NULL = the soft alert has not fired yet this period.
    warned_at       TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, period_start)
);

ALTER TABLE org_budgets      ENABLE ROW LEVEL SECURITY;
ALTER TABLE org_budgets      FORCE  ROW LEVEL SECURITY;
ALTER TABLE org_budget_usage ENABLE ROW LEVEL SECURITY;
ALTER TABLE org_budget_usage FORCE  ROW LEVEL SECURITY;

-- Tenant reads (admission gate via begin_as_user) see only their own org. The
-- worker settle / per-turn gate uses begin_privileged and bypasses RLS, same
-- as PgDagBudget::bump_or_fail and PgTurnMetricsStore::record.
CREATE POLICY org_budgets_org_isolation ON org_budgets
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

CREATE POLICY org_budget_usage_org_isolation ON org_budget_usage
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
