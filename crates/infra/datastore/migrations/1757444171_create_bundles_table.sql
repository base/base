DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'bundle_state') THEN
        CREATE TYPE bundle_state AS ENUM (
            'Ready',
            'IncludedByBuilder'
        );
    END IF;
END$$;

-- Table for maintenance job, that can be walked back on hash mismatches
CREATE TABLE IF NOT EXISTS maintenance (
    block_number BIGINT PRIMARY KEY,
    block_hash CHAR(66) NOT NULL,
    finalized BOOL NOT NULL DEFAULT false
);

-- Create bundles table
CREATE TABLE IF NOT EXISTS bundles (
    id UUID PRIMARY KEY,
    bundle_state bundle_state NOT NULL,
    state_changed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    txn_hashes CHAR(66)[],
    senders CHAR(42)[],
    minimum_base_fee BIGINT, -- todo use numeric

    txs TEXT[] NOT NULL,
    reverting_tx_hashes CHAR(66)[],
    dropping_tx_hashes CHAR(66)[],

    block_number BIGINT,
    min_timestamp BIGINT,
    max_timestamp BIGINT,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

