-- Lark adapter — the agent↔human DM binding (issue #178, arm 3).
--
-- A never-seen Lark DM has no chat_id; the bot sends to the recipient directly
-- by open_id (`im/v1/messages?receive_id_type=open_id`) and Lark routes it to
-- the (auto-created) p2p chat. Because every outbound turn can re-send by the
-- same open_id, the binding the outbound router needs is the recipient's
-- open_id — known up front from the directory shadow, no post-send capture. The
-- p2p chat is not a threaded conversation, so it cannot live in `lark_threads`
-- (whose PK requires a NOT NULL lark_thread_id).
--
--   outbound: the router looks up by patom_thread_id — whom to send to — so that
--             column is UNIQUE. The lookup runs BEFORE binding, so a re-fire
--             reuses the same recipient (idempotent; never opens a second chat).
--
-- app_id pins the DM to the bot that owns it (whose tenant_access_token posts).

CREATE TABLE lark_dms (
    org_id          UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    app_id          TEXT NOT NULL CHECK (octet_length(app_id) BETWEEN 1 AND 128),
    patom_thread_id UUID NOT NULL UNIQUE REFERENCES threads(id) ON DELETE CASCADE,
    open_id         TEXT NOT NULL CHECK (octet_length(open_id) BETWEEN 1 AND 128),
    created_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (org_id, app_id, patom_thread_id),
    FOREIGN KEY (org_id, app_id) REFERENCES lark_apps (org_id, app_id) ON DELETE CASCADE
);

ALTER TABLE lark_dms ENABLE ROW LEVEL SECURITY;
ALTER TABLE lark_dms FORCE ROW LEVEL SECURITY;
CREATE POLICY lark_dms_org_isolation ON lark_dms
    FOR ALL TO PUBLIC
    USING      (app_user_is_member(org_id))
    WITH CHECK (app_user_is_member(org_id));
