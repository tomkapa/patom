-- Reverse the `agents.system_prompt` drop. Restores the column and
-- backfills it from each agent's current (= max-version)
-- `agent_prompt_versions` row.

ALTER TABLE agents ADD COLUMN system_prompt TEXT;

UPDATE agents a
   SET system_prompt = apv.system_prompt
  FROM agent_prompt_versions apv
 WHERE apv.agent_id = a.id
   AND apv.version  = (SELECT MAX(version)
                         FROM agent_prompt_versions
                        WHERE agent_id = a.id);

ALTER TABLE agents ALTER COLUMN system_prompt SET NOT NULL;
