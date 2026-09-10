-- Persist the requested intermediate-root interval on the proof request itself
-- so retries can recover the original value when the outbox row has been
-- pruned. NULL preserves the existing semantics (worker falls back to
-- DEFAULT_INTERMEDIATE_ROOT_INTERVAL).
ALTER TABLE proof_requests ADD COLUMN intermediate_root_interval BIGINT;
