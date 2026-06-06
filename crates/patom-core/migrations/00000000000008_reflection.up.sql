-- Reflection support (doc/memory.md §1.6, §2.4 — Phase 4).
--
-- `reflection_checkpoints.reflection_event_id` becomes nullable. A reflection
-- that produces zero memory mutations still records its checkpoint so the
-- next sweep advances; without nullability we would have to mint a synthetic
-- "no-op" journal event just to satisfy the FK. Pre-launch — see
-- `feedback_no_backcompat`: no rows exist yet, so this is a one-shot relax
-- with no migration churn.

ALTER TABLE reflection_checkpoints
    ALTER COLUMN reflection_event_id DROP NOT NULL;
