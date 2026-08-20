-- no-transaction
-- Include BUILDER_EXPIRED in the rejected-events partial index used by
-- GET /api/rejected. CONCURRENTLY so ingest can continue while the index
-- builds.
CREATE INDEX CONCURRENTLY IF NOT EXISTS transaction_events_rejected_event_time_idx
    ON transaction_events (event_type, event_time DESC)
    WHERE event_type IN ('SIMULATION_FAILED', 'BUILDER_REJECTED', 'BUILDER_EXPIRED');
