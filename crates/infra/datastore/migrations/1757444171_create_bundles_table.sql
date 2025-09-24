DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'bundle_state') THEN
        CREATE TYPE bundle_state AS ENUM (
            'Ready',
            'BundleLimit',
            'AccountLimits',
            'GlobalLimits',
            'IncludedInFlashblock',
            'IncludedInBlock'
        );
    END IF;
END$$;


-- Create bundles table
CREATE TABLE IF NOT EXISTS bundles (
    id UUID PRIMARY KEY,
    "state" bundle_state NOT NULL,

    senders CHAR(42)[],
    minimum_base_fee BIGINT, -- todo find a larger type
    txn_hashes CHAR(66)[],

    txs TEXT[] NOT NULL,
    reverting_tx_hashes CHAR(66)[],
    dropping_tx_hashes CHAR(66)[],

    block_number BIGINT,
    min_timestamp BIGINT,
    max_timestamp BIGINT,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);