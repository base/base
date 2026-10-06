-- no-transaction
-- Drop the rejected-events partial index before recreating it with
-- BUILDER_EXPIRED. CONCURRENTLY must be the only statement in this
-- migration; Postgres treats a multi-statement query as a transaction.
DROP INDEX CONCURRENTLY IF EXISTS transaction_events_rejected_event_time_idx;
