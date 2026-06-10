-- Reverse the per-org default-model column (#141).
ALTER TABLE organizations DROP COLUMN IF EXISTS default_model;
