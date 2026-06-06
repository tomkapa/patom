-- Revert the validator to the pre-rekey UUID-keyed version (migration 26).
DROP FUNCTION IF EXISTS agents_allowed_mcp_tools_valid(jsonb) CASCADE;

CREATE OR REPLACE FUNCTION agents_allowed_mcp_tools_valid(payload jsonb)
RETURNS boolean
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
    key_count int;
    kv record;
    elem text;
    seen_count int;
    distinct_count int;
BEGIN
    IF jsonb_typeof(payload) <> 'object' THEN
        RETURN false;
    END IF;
    SELECT count(*) INTO key_count
    FROM jsonb_object_keys(payload);
    IF key_count > 32 THEN
        RETURN false;
    END IF;
    FOR kv IN SELECT key, value FROM jsonb_each(payload) LOOP
        IF jsonb_typeof(kv.value) = 'null' THEN
            CONTINUE;
        END IF;
        IF jsonb_typeof(kv.value) <> 'array' THEN
            RETURN false;
        END IF;
        IF jsonb_array_length(kv.value) > 64 THEN
            RETURN false;
        END IF;
        FOR elem IN
            SELECT jsonb_array_elements(kv.value) #>> '{}'
        LOOP
            IF elem IS NULL THEN
                RETURN false;
            END IF;
            IF octet_length(elem) < 1 OR octet_length(elem) > 64 THEN
                RETURN false;
            END IF;
        END LOOP;
        SELECT count(*), count(DISTINCT v)
          INTO seen_count, distinct_count
          FROM jsonb_array_elements_text(kv.value) AS v;
        IF seen_count <> distinct_count THEN
            RETURN false;
        END IF;
    END LOOP;
    RETURN true;
END;
$$;

ALTER TABLE agents
    ADD CONSTRAINT agents_allowed_mcp_tools_shape
    CHECK (agents_allowed_mcp_tools_valid(allowed_mcp_tools));
