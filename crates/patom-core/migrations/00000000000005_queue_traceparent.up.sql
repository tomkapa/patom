-- Trace propagation across the queue handoff. Without this, the producer's
-- HTTP/tool span and the worker's `handle_claim` span land in different OTel
-- traces, so a single agent-chain conversation fragments into N disconnected
-- traces in the backend. The column carries the W3C `traceparent` header
-- (`00-<trace-id>-<span-id>-<flags>`) captured at enqueue time; the worker
-- uses it as the parent context when building its claim span.
--
-- Nullable: rows enqueued without an active OTel context (test scaffolding,
-- or runs with the exporter disabled) still go through; the worker just
-- starts a fresh root span.

ALTER TABLE prompt_requests
    ADD COLUMN traceparent TEXT NULL;
