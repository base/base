-- no-transaction
-- Speeds retention deletes: WHERE event_type = ANY(...) AND ingested_at < ...
-- CONCURRENTLY so ingest can continue while the index builds.
CREATE INDEX CONCURRENTLY IF NOT EXISTS transaction_events_event_type_ingested_at_idx
    ON transaction_events (event_type, ingested_at);
