-- Per-agent model selection. NULL = "use workspace default model"
-- (Settings::model). The catalog membership check lives in the application
-- layer (`Model::try_from`) because the allowlist evolves in Rust source,
-- not in SQL — the column CHECK only bounds length to match
-- MODEL_ID_MAX_LEN = 128 in the type layer.
ALTER TABLE agents
    ADD COLUMN model TEXT NULL
        CHECK (model IS NULL OR octet_length(model) BETWEEN 1 AND 128);
