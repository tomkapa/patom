ALTER TABLE mcp_oauth_pending
    DROP CONSTRAINT IF EXISTS mcp_oauth_pending_lark_ctx_all_or_none,
    DROP CONSTRAINT IF EXISTS mcp_oauth_pending_discord_ctx_all_or_none;

ALTER TABLE mcp_oauth_pending
    DROP COLUMN IF EXISTS lark_app_id,
    DROP COLUMN IF EXISTS lark_chat_id,
    DROP COLUMN IF EXISTS lark_reply_to,
    DROP COLUMN IF EXISTS discord_application_id,
    DROP COLUMN IF EXISTS discord_container_id,
    DROP COLUMN IF EXISTS discord_reply_to;
