ALTER TABLE mcp_oauth_pending
    DROP CONSTRAINT IF EXISTS mcp_oauth_pending_resume_ctx_all_or_none,
    DROP CONSTRAINT IF EXISTS mcp_oauth_pending_slack_ctx_all_or_none;

ALTER TABLE mcp_oauth_pending
    DROP COLUMN IF EXISTS session_id,
    DROP COLUMN IF EXISTS agent_id,
    DROP COLUMN IF EXISTS slack_team_id,
    DROP COLUMN IF EXISTS slack_channel_id,
    DROP COLUMN IF EXISTS slack_thread_ts;
