-- Human-in-the-loop approval gating (issue #200).
--
-- An agent calls the `ask_approval` tool before a consequential, approval-gated
-- action. The tool inserts a `pending_approval` row, posts an interactive prompt
-- (Discord buttons / Lark card) and ENDS the turn — the worker is
-- run-to-completion with no wait state, so this is built on the scheduler
-- fresh-trigger pattern, not turn suspension. A later human click resolves the
-- row (pending → approved/denied) and enqueues a *fresh* trigger seeding the
-- decision; the agent resumes and re-attempts the gated tool, which a hard
-- pre-execution gate now allows because a matching `approved` row exists for the
-- DAG.
--
-- `gated_tool` is the key the hard gate matches on: `has_approved_for_dag(root,
-- tool)` looks for an `approved` row with this column equal to the tool the
-- agent is about to run. `root_request_id` reuses the original DAG root so turn
-- budget (`prompt_request_dags`) and lineage are preserved on resume.
--
-- Tenancy: `org_id` denormalised for RLS + cheap org-scoped scans. The webhook
-- intake (Discord Gateway / Lark card callback) has no Patom session principal,
-- so its reads/decide run privileged (RLS bypass) with `org_id` taken from the
-- verified app — the RLS policy here is the WITH CHECK backstop for the
-- tenant-side writes the `ask_approval` tool performs.

CREATE TABLE pending_approval (
    id                      UUID PRIMARY KEY,
    org_id                  UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    thread_id               UUID NOT NULL REFERENCES threads(id)        ON DELETE CASCADE,
    requesting_agent_id     UUID NOT NULL REFERENCES agents(id)         ON DELETE CASCADE,
    requesting_colleague_id UUID NOT NULL REFERENCES colleagues(id)     ON DELETE CASCADE,
    -- The conversation tree this approval belongs to; resume reuses it as the
    -- fresh trigger's root so the turn budget is shared, not reset.
    root_request_id         UUID NOT NULL REFERENCES prompt_requests(id) ON DELETE CASCADE,
    -- Human-readable description of what is being authorized.
    action_summary          TEXT NOT NULL CHECK (octet_length(action_summary) BETWEEN 1 AND 2048),
    -- The tool name this approval authorizes; the hard gate matches on it.
    gated_tool              TEXT NOT NULL CHECK (octet_length(gated_tool) BETWEEN 1 AND 64),
    -- Approver policy discriminant. 'anyone' / 'one_of' carry no single
    -- colleague; 'colleague' pins exactly one (in `approver_colleague`).
    approver_kind           TEXT NOT NULL CHECK (approver_kind IN ('anyone', 'colleague', 'one_of')),
    approver_colleague      UUID NULL REFERENCES colleagues(id) ON DELETE CASCADE,
    status                  TEXT NOT NULL DEFAULT 'pending'
                                CHECK (status IN ('pending', 'approved', 'denied', 'expired')),
    -- Where the interactive prompt was posted (for editing on resolve).
    platform                TEXT NOT NULL CHECK (platform IN ('discord', 'lark', 'web')),
    platform_app_id         TEXT NULL,
    platform_container      TEXT NULL,
    platform_reply_to       TEXT NULL,
    -- Filled by `attach_message` after the prompt is posted; the resolve path
    -- edits this message to the resolved view.
    platform_message_id     TEXT NULL,
    -- Dedupes repeated `ask_approval` calls in one DAG (UNIQUE with org_id).
    idempotency_key         TEXT NOT NULL,
    decided_by_colleague    UUID NULL REFERENCES colleagues(id) ON DELETE SET NULL,
    decided_at              TIMESTAMPTZ NULL,
    expires_at              TIMESTAMPTZ NOT NULL,
    created_at              TIMESTAMPTZ NOT NULL,

    -- A decided row (approved/denied) records who + when; a non-decided row
    -- (pending/expired) records neither. Keeps the audit trail honest.
    CONSTRAINT pending_approval_decided_consistency CHECK (
        (status IN ('pending', 'expired')
            AND decided_by_colleague IS NULL AND decided_at IS NULL)
        OR
        (status IN ('approved', 'denied')
            AND decided_by_colleague IS NOT NULL AND decided_at IS NOT NULL)
    ),
    -- A 'colleague' policy pins exactly one approver; the other policies pin none.
    CONSTRAINT pending_approval_approver_consistency CHECK (
        (approver_kind = 'colleague' AND approver_colleague IS NOT NULL)
        OR
        (approver_kind IN ('anyone', 'one_of') AND approver_colleague IS NULL)
    )
);

-- Idempotent create: ON CONFLICT (org_id, idempotency_key) DO NOTHING.
CREATE UNIQUE INDEX pending_approval_idem_idx ON pending_approval (org_id, idempotency_key);
-- The hard gate's lookup: an approved row for this DAG + tool.
CREATE INDEX pending_approval_dag_idx ON pending_approval (root_request_id, status, gated_tool);
-- The expiry sweep scans only still-pending rows.
CREATE INDEX pending_approval_expiry_idx ON pending_approval (status, expires_at)
    WHERE status = 'pending';

ALTER TABLE pending_approval ENABLE ROW LEVEL SECURITY;
ALTER TABLE pending_approval FORCE ROW LEVEL SECURITY;
CREATE POLICY pending_approval_org_isolation ON pending_approval
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- `OneOf` approver set: the explicit whitelist of colleagues who may decide.
-- Keyed by colleague id, which transparently backs a real OR shadow user — a
-- whitelisted approver who has never logged into Patom resolves (via the same
-- idempotent shadow mint) to the colleague the clicking chat surface produces.
CREATE TABLE pending_approval_approvers (
    approval_id  UUID NOT NULL REFERENCES pending_approval(id) ON DELETE CASCADE,
    colleague_id UUID NOT NULL REFERENCES colleagues(id)       ON DELETE CASCADE,
    org_id       UUID NOT NULL REFERENCES organizations(id)    ON DELETE CASCADE,
    PRIMARY KEY (approval_id, colleague_id)
);
CREATE INDEX pending_approval_approvers_org_idx ON pending_approval_approvers (org_id);

ALTER TABLE pending_approval_approvers ENABLE ROW LEVEL SECURITY;
ALTER TABLE pending_approval_approvers FORCE ROW LEVEL SECURITY;
CREATE POLICY pending_approval_approvers_org_isolation ON pending_approval_approvers
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

-- Per-agent admin config: which tool names require approval before execution.
-- Both the hard gate and the system-prompt builder read this — a static
-- `requires_approval()` marker can't cover tenant-specific MCP tools (e.g.
-- `refund_customer`, `merge_pr`), so the gated set is data, not code.
CREATE TABLE agent_gated_tools (
    agent_id   UUID NOT NULL REFERENCES agents(id)        ON DELETE CASCADE,
    tool_name  TEXT NOT NULL CHECK (octet_length(tool_name) BETWEEN 1 AND 64),
    org_id     UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (agent_id, tool_name)
);
CREATE INDEX agent_gated_tools_org_idx ON agent_gated_tools (org_id);

ALTER TABLE agent_gated_tools ENABLE ROW LEVEL SECURITY;
ALTER TABLE agent_gated_tools FORCE ROW LEVEL SECURITY;
CREATE POLICY agent_gated_tools_org_isolation ON agent_gated_tools
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
