-- organizations.default_model — per-org default LLM model (#141).
--
-- Chosen when a workspace enters its first BYO provider key. An agent whose
-- pinned model's provider has no usable key reroutes to this model instead of
-- the workspace-wide `Settings::model`. NULL = fall back to the process-wide
-- default (current behavior), so existing rows need no backfill and the add is
-- a metadata-only change (no table rewrite).
--
-- The value is a catalog model name (e.g. 'claude-sonnet-4-6'); it is parsed
-- back through `Model::try_from` at the boundary (CLAUDE.md §1), so an
-- unknown/retired name surfaces as an error rather than a silent default. No
-- CHECK against the catalog here — the catalog is code, not schema.

ALTER TABLE organizations ADD COLUMN default_model TEXT;
