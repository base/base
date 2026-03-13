-- Create proof_requests table
CREATE TABLE IF NOT EXISTS proof_requests (
    -- Primary identifier (our UUID, returned to caller immediately)
    id UUID PRIMARY KEY,

    -- Request parameters
    start_block_number BIGINT NOT NULL,
    number_of_blocks_to_prove BIGINT NOT NULL,
    sequence_window BIGINT,

    -- Proving backend tracking
    proving_backend_id VARCHAR(50),  -- Backend identifier (e.g., 'generic_zkvm')
    proving_backend_session_id VARCHAR(255),  -- Backend-specific session ID

    -- Receipts (binary data)
    stark_receipt BYTEA,
    snark_receipt BYTEA,

    -- Status tracking: CREATED -> PENDING -> RUNNING -> SUCCEEDED/FAILED
    status VARCHAR(20) NOT NULL DEFAULT 'CREATED',
    error_message TEXT,

    -- Timestamps
    created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMP WITH TIME ZONE
);

-- Indexes for common queries
CREATE INDEX IF NOT EXISTS idx_proof_requests_status ON proof_requests(status);
CREATE INDEX IF NOT EXISTS idx_proof_requests_backend_session ON proof_requests(proving_backend_session_id);
CREATE INDEX IF NOT EXISTS idx_proof_requests_created_at ON proof_requests(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_proof_requests_backend_id ON proof_requests(proving_backend_id);

-- Function to automatically update updated_at timestamp
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Trigger to call the function on every update
DROP TRIGGER IF EXISTS update_proof_requests_updated_at ON proof_requests;
CREATE TRIGGER update_proof_requests_updated_at
    BEFORE UPDATE ON proof_requests
    FOR EACH ROW
    EXECUTE FUNCTION update_updated_at_column();

-- Create proof_request_outbox table for reliable task processing
CREATE TABLE IF NOT EXISTS proof_request_outbox (
    -- Auto-incrementing sequence for ordering
    sequence_id BIGSERIAL PRIMARY KEY,

    -- Reference to the proof request
    proof_request_id UUID NOT NULL REFERENCES proof_requests(id),

    -- Request parameters (JSON)
    request_params JSONB NOT NULL,

    -- Processing status
    processed BOOLEAN NOT NULL DEFAULT FALSE,
    processed_at TIMESTAMP WITH TIME ZONE,

    -- Retry tracking
    retry_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,

    -- Timestamps
    created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW()
);

-- Indexes for outbox queries
CREATE INDEX IF NOT EXISTS idx_outbox_processed ON proof_request_outbox(processed, created_at);
CREATE INDEX IF NOT EXISTS idx_outbox_proof_request_id ON proof_request_outbox(proof_request_id);

-- Comment on status column
COMMENT ON COLUMN proof_requests.status IS 'Status: CREATED, PENDING, RUNNING, SUCCEEDED, FAILED';
