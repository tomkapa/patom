-- Lark adapter — the Lark-thread <-> Patom-thread binding (analogue of
-- slack_threads).
--
-- One row per Lark-rooted conversation: a Lark (tenant_key, chat_id,
-- lark_thread_id) triple <-> one Patom thread_id. lark_thread_id is the
-- message's `thread_id` for a reply, else the root `message_id` (Lark threads
-- under the first reply, like Slack's thread_ts).
--
--   inbound:  lookup by (tenant_key, chat_id, lark_thread_id) — new thread or
--             continuation.
--   outbound: the stream pump looks up by patom_thread_id — where to post — so
--             that column is UNIQUE (which also supplies its index).
--
-- app_id pins the thread to the bot whose long-connection delivered its events,
-- so the outbound poster knows which app_secret / tenant_access_token to use.

CREATE TABLE lark_threads (
    org_id          UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    app_id          TEXT NOT NULL CHECK (octet_length(app_id) BETWEEN 1 AND 128),
    tenant_key      TEXT NOT NULL CHECK (octet_length(tenant_key) BETWEEN 1 AND 128),
    chat_id         TEXT NOT NULL CHECK (octet_length(chat_id) BETWEEN 1 AND 128),
    lark_thread_id  TEXT NOT NULL CHECK (octet_length(lark_thread_id) BETWEEN 1 AND 128),
    patom_thread_id UUID NOT NULL UNIQUE REFERENCES threads(id) ON DELETE CASCADE,
    created_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (tenant_key, chat_id, lark_thread_id),
    FOREIGN KEY (org_id, app_id) REFERENCES lark_apps (org_id, app_id) ON DELETE CASCADE
);

ALTER TABLE lark_threads ENABLE ROW LEVEL SECURITY;
ALTER TABLE lark_threads FORCE ROW LEVEL SECURITY;
CREATE POLICY lark_threads_org_isolation ON lark_threads
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
