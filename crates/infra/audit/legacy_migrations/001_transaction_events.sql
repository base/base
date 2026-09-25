CREATE TABLE IF NOT EXISTS transaction_events (
    event_id TEXT PRIMARY KEY,
    schema_version TEXT NOT NULL,
    event_time TIMESTAMPTZ NOT NULL,
    ingested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    producer TEXT NOT NULL,
    event_type TEXT NOT NULL,
    network TEXT,
    tx_hash TEXT,
    block_hash TEXT,
    block_number BIGINT,
    payload_id TEXT,
    request_id TEXT,
    data JSONB NOT NULL
);

CREATE INDEX IF NOT EXISTS transaction_events_tx_hash_event_time_idx
    ON transaction_events (tx_hash, event_time)
    WHERE tx_hash IS NOT NULL;

CREATE INDEX IF NOT EXISTS transaction_events_block_number_event_time_idx
    ON transaction_events (block_number, event_time)
    WHERE block_number IS NOT NULL;

CREATE INDEX IF NOT EXISTS transaction_events_block_hash_event_time_idx
    ON transaction_events (block_hash, event_time)
    WHERE block_hash IS NOT NULL;

CREATE INDEX IF NOT EXISTS transaction_events_payload_id_event_time_idx
    ON transaction_events (payload_id, event_time)
    WHERE payload_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS transaction_events_producer_event_type_event_time_idx
    ON transaction_events (producer, event_type, event_time);

CREATE INDEX IF NOT EXISTS transaction_events_rejected_event_time_idx
    ON transaction_events (event_type, event_time DESC)
    WHERE event_type IN ('SIMULATION_FAILED', 'BUILDER_REJECTED');

CREATE INDEX IF NOT EXISTS transaction_events_bundle_hash_event_time_idx
    ON transaction_events ((data->>'bundle_hash'), event_time)
    WHERE data ? 'bundle_hash';

CREATE INDEX IF NOT EXISTS transaction_events_bundle_id_event_time_idx
    ON transaction_events ((data->>'bundle_id'), event_time)
    WHERE data ? 'bundle_id';

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'audit_archiver') THEN
        GRANT SELECT ON _sqlx_migrations TO audit_archiver;
        GRANT SELECT, INSERT, UPDATE, DELETE ON transaction_events TO audit_archiver;
    END IF;
END $$;
