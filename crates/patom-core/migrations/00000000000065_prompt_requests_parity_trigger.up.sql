-- Re-add the org-parity trigger on prompt_requests (note 2).
--
-- Migration 63 dropped the old `prompt_requests_enforce_org` trigger when it
-- removed `session_id` (the trigger checked org parity against the parent
-- session). The trigger model is now polymorphic: a row carries exactly one of
-- `state_id` (FK agent_thread_state) or `background_turn_id` (FK background_turns)
-- — the `prompt_requests_claim_key_xor` CHECK enforces the XOR. This re-adds the
-- defense-in-depth parity check so a trigger row's `org_id` must match its
-- parent's org (mirrors the turn_metrics/tool_calls/session_todos triggers from
-- migration 63). RLS already isolates by org membership; this catches a
-- cross-org `(org_id, state_id)` mismatch the way the child tables do.

CREATE OR REPLACE FUNCTION enforce_prompt_requests_org() RETURNS TRIGGER
    LANGUAGE plpgsql AS $$
DECLARE parent_org UUID;
BEGIN
    IF NEW.state_id IS NOT NULL THEN
        SELECT org_id INTO parent_org FROM agent_thread_state WHERE id = NEW.state_id;
        IF parent_org IS NULL THEN
            RAISE EXCEPTION 'prompt_requests.state_id % references missing state', NEW.state_id;
        END IF;
    ELSE
        SELECT org_id INTO parent_org FROM background_turns WHERE id = NEW.background_turn_id;
        IF parent_org IS NULL THEN
            RAISE EXCEPTION 'prompt_requests.background_turn_id % references missing background turn', NEW.background_turn_id;
        END IF;
    END IF;
    IF parent_org <> NEW.org_id THEN
        RAISE EXCEPTION 'prompt_requests.org_id % != claim-key parent org %', NEW.org_id, parent_org;
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER prompt_requests_enforce_org
    BEFORE INSERT OR UPDATE OF org_id, state_id, background_turn_id ON prompt_requests
    FOR EACH ROW EXECUTE FUNCTION enforce_prompt_requests_org();
