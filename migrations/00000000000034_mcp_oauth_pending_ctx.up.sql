-- Extend mcp_oauth_pending with two independent context groups so the
-- OAuth callback can do channel-agnostic auto-continue and an optional
-- Slack thread-ping.
--
-- Group A — `resume_ctx` (`session_id`, `agent_id`). When populated, the
-- callback enqueues a synthetic prompt ("I've connected {name}. Please
-- continue.") to resume the agent loop. Channel-agnostic: web UI, Slack,
-- and any future channel adapter populate this the same way. Replaces
-- the FE-driven `POST /prompts` injection that previously lived only on
-- the web side.
--
-- Group B — `slack_ctx` (`slack_team_id`, `slack_channel_id`,
-- `slack_thread_ts`). When populated, the callback posts the
-- "✓ Connected — <Provider>" UX ping to that Slack thread. Slack-only
-- channel context, independent of Group A.
--
-- Both groups are all-or-none — express the invariant in CHECK
-- constraints so a half-populated row can never be written.

ALTER TABLE mcp_oauth_pending
    ADD COLUMN session_id     UUID NULL,
    ADD COLUMN agent_id       UUID NULL,
    ADD COLUMN slack_team_id  TEXT NULL,
    ADD COLUMN slack_channel_id TEXT NULL,
    ADD COLUMN slack_thread_ts TEXT NULL;

ALTER TABLE mcp_oauth_pending
    ADD CONSTRAINT mcp_oauth_pending_resume_ctx_all_or_none CHECK (
        num_nonnulls(session_id, agent_id) IN (0, 2)
    ),
    ADD CONSTRAINT mcp_oauth_pending_slack_ctx_all_or_none CHECK (
        num_nonnulls(slack_team_id, slack_channel_id, slack_thread_ts) IN (0, 3)
    );
