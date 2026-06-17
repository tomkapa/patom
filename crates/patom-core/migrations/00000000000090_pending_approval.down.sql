-- Reverse of 00000000000090_pending_approval.up.sql. Policies fall with their
-- tables but are dropped explicitly to mirror the up's ordering and the other
-- chat-surface down migrations. Child tables drop first (FK direction).

DROP POLICY IF EXISTS agent_gated_tools_org_isolation ON agent_gated_tools;
DROP TABLE IF EXISTS agent_gated_tools;

DROP POLICY IF EXISTS pending_approval_approvers_org_isolation ON pending_approval_approvers;
DROP TABLE IF EXISTS pending_approval_approvers;

DROP POLICY IF EXISTS pending_approval_org_isolation ON pending_approval;
DROP TABLE IF EXISTS pending_approval;
