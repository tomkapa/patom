-- Per-org rule directive injected into every agent's system prompt.
--
-- Adds a nullable `default_rule` text column on `organizations` that an
-- admin/owner edits via PATCH /me/org/rule. The agent worker reads it on
-- every turn through `OrgRuleResolver` (TTL-cached) and emits an
-- `<organization-rule>...</organization-rule>` block in the system prompt
-- immediately after `<core>` — the most cache-friendly position for a
-- per-org stable directive.
--
-- Nullable on purpose. Unlike `default_language` (NOT NULL, every org
-- must have one) "no rule configured" is a real, meaningful state — the
-- renderer omits the tag entirely. The CHECK mirrors the
-- `OrganizationRule::try_from` smart constructor on both axes — length
-- (≤ 16 KiB to match `MAX_ORG_RULE_BYTES`) AND non-whitespace
-- (`btrim(...) <> ''`) — so a direct SQL write cannot land a value the
-- read path would refuse to parse. Without the non-whitespace guard, an
-- out-of-band `UPDATE ... SET default_rule = '   '` would pass the DB
-- but break `list_user_orgs` for every org the user belongs to and
-- break every agent turn for that org.

ALTER TABLE organizations
    ADD COLUMN default_rule TEXT
        CHECK (
            default_rule IS NULL
            OR (octet_length(default_rule) <= 16384 AND btrim(default_rule) <> '')
        );
