-- Slack adapter — Phase 1 (identity link + thread map).
--
-- Two tables in one migration because they share the same composite FK
-- to `slack_workspaces(org_id, team_id)` and are introduced together as
-- part of the same shippable slice.
--
-- `slack_identities` links a Slack user to a Relay user. Phase 1 leaves
-- this empty by default and falls back to the workspace's
-- `installed_by_user_id` for unlinked users; Phase 2 (see GitHub issue)
-- adds the DM-based linking flow that populates rows here.
--
-- `slack_threads` is the bidirectional bridge between a Slack thread
-- (channel + thread_ts) and a Relay DAG (root_request_id). One row per
-- live Slack-rooted conversation. The webhook handler looks up by
-- (team, channel, thread_ts) — PK — to decide "new session or
-- continuation"; the stream pump looks up by `root_request_id` — UNIQUE
-- — to find where to post.

CREATE TABLE slack_identities (
    org_id        UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    team_id       TEXT NOT NULL,
    slack_user_id TEXT NOT NULL
                  CHECK (octet_length(slack_user_id) BETWEEN 1 AND 32),
    user_id       UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    linked_at     TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, team_id, slack_user_id),
    -- Composite FK enforces that an identity cannot point at a
    -- workspace from a different org.
    FOREIGN KEY (org_id, team_id) REFERENCES slack_workspaces(org_id, team_id)
        ON DELETE CASCADE
);

-- A Slack identity can be linked at most once across the whole product
-- (regardless of which Relay org). This prevents a single Slack user
-- from being claimed by two Relay tenants and is the safety net for
-- the tenant-isolation invariant the webhook relies on.
CREATE UNIQUE INDEX slack_identities_slack_user_idx
    ON slack_identities(team_id, slack_user_id);

ALTER TABLE slack_identities ENABLE ROW LEVEL SECURITY;
ALTER TABLE slack_identities FORCE ROW LEVEL SECURITY;
CREATE POLICY slack_identities_org_isolation ON slack_identities
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));

CREATE TABLE slack_threads (
    org_id            UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    team_id           TEXT NOT NULL,
    channel_id        TEXT NOT NULL
                      CHECK (octet_length(channel_id) BETWEEN 1 AND 32),
    -- The Slack `ts` that anchors the thread: `thread_ts` for replies,
    -- falling back to the parent message's `ts` for a mention on a
    -- top-level message (Slack auto-creates a thread on first reply).
    thread_ts         TEXT NOT NULL
                      CHECK (octet_length(thread_ts) BETWEEN 1 AND 32),
    -- The Relay DAG root. Every prompt_requests row whose
    -- root_request_id matches this value posts back into the Slack
    -- thread above.
    root_request_id   UUID NOT NULL UNIQUE,
    session_id        UUID NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (team_id, channel_id, thread_ts),
    FOREIGN KEY (org_id, team_id) REFERENCES slack_workspaces(org_id, team_id)
        ON DELETE CASCADE
);

-- Stream pump lookup: given a `root_request_id` from a broadcast event,
-- find the channel + thread_ts to post into. UNIQUE column already
-- yields an index, but name it explicitly for clarity.
CREATE INDEX slack_threads_root_idx ON slack_threads(root_request_id);

ALTER TABLE slack_threads ENABLE ROW LEVEL SECURITY;
ALTER TABLE slack_threads FORCE ROW LEVEL SECURITY;
CREATE POLICY slack_threads_org_isolation ON slack_threads
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
