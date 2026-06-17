-- Extend mcp_oauth_pending with Lark and Discord ping-context groups,
-- the chat-platform peers of the Slack group added in migration 34.
--
-- When an agent emits a `WireMcpRequest` on a Lark / Discord thread, the
-- connect link drives the OAuth start flow and stashes the originating
-- chat target here. After the callback succeeds, `do_lark_ping` /
-- `do_discord_ping` post the "✓ Connected — <Provider>" UX ping back into
-- that thread (the channel-agnostic auto-continue rides Group A — the
-- existing `session_id`/`agent_id` resume_ctx — and needs no new column).
--
-- Group C — `lark_ctx` (`lark_app_id`, `lark_chat_id`, `lark_reply_to`).
-- Group D — `discord_ctx` (`discord_application_id`, `discord_container_id`,
--           `discord_reply_to`).
--
-- The required id pair in each group is all-or-none; `reply_to` is an
-- optional thread anchor that may only be present when its pair is — a
-- reply target without a chat target is meaningless. Both invariants live
-- in CHECK constraints so a half-populated row can never be written.

ALTER TABLE mcp_oauth_pending
    ADD COLUMN lark_app_id           TEXT NULL,
    ADD COLUMN lark_chat_id          TEXT NULL,
    ADD COLUMN lark_reply_to         TEXT NULL,
    ADD COLUMN discord_application_id TEXT NULL,
    ADD COLUMN discord_container_id  TEXT NULL,
    ADD COLUMN discord_reply_to      TEXT NULL;

ALTER TABLE mcp_oauth_pending
    ADD CONSTRAINT mcp_oauth_pending_lark_ctx_all_or_none CHECK (
        num_nonnulls(lark_app_id, lark_chat_id) IN (0, 2)
        AND (lark_reply_to IS NULL OR lark_chat_id IS NOT NULL)
    ),
    ADD CONSTRAINT mcp_oauth_pending_discord_ctx_all_or_none CHECK (
        num_nonnulls(discord_application_id, discord_container_id) IN (0, 2)
        AND (discord_reply_to IS NULL OR discord_container_id IS NOT NULL)
    );
