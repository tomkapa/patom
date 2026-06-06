-- Multi-agent communication — sessions become 2-party (Human ↔ Agent or
-- Agent ↔ Agent) with causal links and a DAG anchor. `send_message` is the
-- only delivery mechanism in the next steps; this migration prepares the
-- schema. Pre-launch, no backfill — see CLAUDE.md §14: this drops columns the
-- old single-agent flow used.

-- ───────────────────────────────────────────────────────────────────────────
-- sessions: drop the bound `agent_id`, add (a, b) participant columns,
-- causal `parent_session_id`, and `root_request_id` DAG anchor.
-- ───────────────────────────────────────────────────────────────────────────
ALTER TABLE sessions DROP COLUMN agent_id;

ALTER TABLE sessions
    ADD COLUMN parent_session_id      UUID NULL REFERENCES sessions(id) ON DELETE SET NULL,
    -- Anchors the session to the DAG root request. The root row is always a
    -- prompt_requests.id; no FK so the session can be inserted in the same
    -- transaction as that request without ordering tricks.
    ADD COLUMN root_request_id        UUID NOT NULL,
    ADD COLUMN participant_a_kind     TEXT NOT NULL
                                      CHECK (participant_a_kind IN ('human','agent')),
    ADD COLUMN participant_a_agent_id UUID NULL REFERENCES agents(id),
    ADD COLUMN participant_b_kind     TEXT NOT NULL
                                      CHECK (participant_b_kind IN ('human','agent')),
    ADD COLUMN participant_b_agent_id UUID NULL REFERENCES agents(id),
    ADD CONSTRAINT sessions_a_kind_agent CHECK (
        (participant_a_kind = 'agent') = (participant_a_agent_id IS NOT NULL)
    ),
    ADD CONSTRAINT sessions_b_kind_agent CHECK (
        (participant_b_kind = 'agent') = (participant_b_agent_id IS NOT NULL)
    ),
    -- Canonical ordering — `Participant::canonical_pair` mirrors this
    -- tuple `<` so two callers naming the same conversation always upsert
    -- into the same `(participant_a, participant_b)` slot. Postgres tuple
    -- compare uses string-lex on the kind columns, so `'agent' < 'human'`
    -- and Rust's canonical order also puts Agent before Human.
    ADD CONSTRAINT sessions_participants_distinct CHECK (
        (participant_a_kind, participant_a_agent_id)
        < (participant_b_kind, participant_b_agent_id)
    );

-- One session per (DAG, canonical pair). The unique index is the dedupe key
-- the `send_message` upsert keys off. `NULLS NOT DISTINCT` (PG 15+) is
-- mandatory: `participant_*_agent_id` is `NULL` for the human side, and the
-- default Postgres NULLs-are-distinct semantics would let two
-- "(human, NULL, agent, X)" rows coexist on the same root — defeating the
-- whole point of canonical-pair dedup.
CREATE UNIQUE INDEX sessions_dag_pair_unique
    ON sessions (root_request_id,
                 participant_a_kind, participant_a_agent_id,
                 participant_b_kind, participant_b_agent_id)
    NULLS NOT DISTINCT;

-- Look up every session in a DAG when computing quiescence / dispatching.
CREATE INDEX sessions_root_idx ON sessions (root_request_id);

-- ───────────────────────────────────────────────────────────────────────────
-- session_messages: replace `role` with sender/receiver pair. `sender_kind`
-- gains a `system` value reserved for worker-injected nudges (ping-pong
-- guard) and never appears as a `receiver_kind`.
-- ───────────────────────────────────────────────────────────────────────────
ALTER TABLE session_messages DROP COLUMN role;

ALTER TABLE session_messages
    ADD COLUMN sender_kind       TEXT NOT NULL
                                 CHECK (sender_kind IN ('human','agent','system')),
    ADD COLUMN sender_agent_id   UUID NULL REFERENCES agents(id),
    ADD COLUMN receiver_kind     TEXT NOT NULL
                                 CHECK (receiver_kind IN ('human','agent')),
    ADD COLUMN receiver_agent_id UUID NULL REFERENCES agents(id),
    ADD CONSTRAINT session_messages_sender_kind_agent CHECK (
        (sender_kind = 'agent') = (sender_agent_id IS NOT NULL)
    ),
    ADD CONSTRAINT session_messages_receiver_kind_agent CHECK (
        (receiver_kind = 'agent') = (receiver_agent_id IS NOT NULL)
    );

-- ───────────────────────────────────────────────────────────────────────────
-- prompt_requests: queue rows know who's talking to whom + DAG anchor.
-- receiver_kind is constrained to 'agent' because human-bound deliveries
-- never enqueue (they publish on the response stream directly).
-- ───────────────────────────────────────────────────────────────────────────
ALTER TABLE prompt_requests
    ADD COLUMN sender_kind       TEXT NOT NULL
                                 CHECK (sender_kind IN ('human','agent')),
    ADD COLUMN sender_agent_id   UUID NULL REFERENCES agents(id),
    ADD COLUMN receiver_kind     TEXT NOT NULL
                                 CHECK (receiver_kind = 'agent'),
    ADD COLUMN receiver_agent_id UUID NOT NULL REFERENCES agents(id),
    ADD COLUMN root_request_id   UUID NOT NULL,
    ADD CONSTRAINT prompt_requests_sender_kind_agent CHECK (
        (sender_kind = 'agent') = (sender_agent_id IS NOT NULL)
    );

-- ───────────────────────────────────────────────────────────────────────────
-- prompt_request_dags: DAG-wide turn budget. The send_message tool atomically
-- bumps `turns_used` and rolls back its insert when `turns_used >= turns_cap`.
-- Cascade-deletes when the root request goes away so the table cannot leak
-- rows past their owning DAG.
-- ───────────────────────────────────────────────────────────────────────────
CREATE TABLE prompt_request_dags (
    root_request_id UUID PRIMARY KEY REFERENCES prompt_requests(id) ON DELETE CASCADE,
    turns_used      BIGINT NOT NULL DEFAULT 0 CHECK (turns_used >= 0),
    turns_cap       BIGINT NOT NULL CHECK (turns_cap > 0),
    created_at      TIMESTAMPTZ NOT NULL
);
