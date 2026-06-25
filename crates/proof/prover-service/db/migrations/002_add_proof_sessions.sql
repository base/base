-- Migration 002: Add proof_sessions table and proof_type column
-- This migration adds support for multi-stage proving workflow (STARK -> SNARK conversion)
-- and allows proof_type to determine success criteria

-- Part 1: Create proof_sessions table for tracking multi-stage proving workflow
-- Each proof request can have multiple sessions (e.g., STARK generation, STARK->SNARK conversion)
CREATE TABLE IF NOT EXISTS proof_sessions (
    id BIGSERIAL PRIMARY KEY,

    -- Reference to the proof request
    proof_request_id UUID NOT NULL REFERENCES proof_requests(id) ON DELETE CASCADE,

    -- Session metadata
    session_type VARCHAR(20) NOT NULL, -- 'STARK', 'SNARK', etc.
    backend_session_id VARCHAR(255) NOT NULL, -- Backend-specific session ID

    -- Status: RUNNING, COMPLETED, FAILED
    status VARCHAR(20) NOT NULL DEFAULT 'RUNNING',
    error_message TEXT,

    -- Timestamps
    created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMP WITH TIME ZONE
);

-- Indexes for common queries
CREATE INDEX IF NOT EXISTS idx_proof_sessions_request ON proof_sessions(proof_request_id);
CREATE INDEX IF NOT EXISTS idx_proof_sessions_backend ON proof_sessions(backend_session_id);
CREATE INDEX IF NOT EXISTS idx_proof_sessions_status ON proof_sessions(status, created_at);
CREATE INDEX IF NOT EXISTS idx_proof_sessions_type ON proof_sessions(session_type);

-- Comment on columns
COMMENT ON COLUMN proof_sessions.session_type IS 'Type of proving session: STARK (initial proof generation), SNARK (STARK to SNARK conversion), etc.';
COMMENT ON COLUMN proof_sessions.status IS 'Session status: RUNNING, COMPLETED, FAILED';

-- Part 2: Add proof_type column to proof_requests
-- Determines what constitutes a successful proof (e.g., STARK only vs STARK+SNARK)
ALTER TABLE proof_requests
ADD COLUMN proof_type VARCHAR(50) NOT NULL DEFAULT 'generic_zkvm_cluster_compressed';

-- Create index for querying by proof type
CREATE INDEX IF NOT EXISTS idx_proof_requests_proof_type ON proof_requests(proof_type);

-- Comment on proof_type column
COMMENT ON COLUMN proof_requests.proof_type IS 'Type of proof: generic_zkvm_cluster_compressed';

-- Part 3: Backfill existing data from proof_requests into proof_sessions
-- All existing proofs are STARK proofs
-- Only run if the old column still exists (idempotent migration)
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_name = 'proof_requests'
        AND column_name = 'proving_backend_session_id'
    ) THEN
        INSERT INTO proof_sessions (
            proof_request_id,
            session_type,
            backend_session_id,
            status,
            created_at,
            completed_at
        )
        SELECT
            id as proof_request_id,
            'STARK' as session_type,
            proving_backend_session_id as backend_session_id,
            CASE
                WHEN status = 'RUNNING' THEN 'RUNNING'
                WHEN status = 'SUCCEEDED' THEN 'COMPLETED'
                WHEN status = 'FAILED' THEN 'FAILED'
                ELSE 'RUNNING'
            END as status,
            created_at,
            completed_at
        FROM proof_requests
        WHERE proving_backend_session_id IS NOT NULL;
    END IF;
END $$;

-- Part 4: Remove old single-session columns from proof_requests
-- Drop indexes first (these are safe to drop if they don't exist)
DROP INDEX IF EXISTS idx_proof_requests_backend_session;
DROP INDEX IF EXISTS idx_proof_requests_backend_id;

-- Drop columns (IF EXISTS clause makes this idempotent)
ALTER TABLE proof_requests DROP COLUMN IF EXISTS proving_backend_id;
ALTER TABLE proof_requests DROP COLUMN IF EXISTS proving_backend_session_id;

-- Update status comment to reflect simplified workflow
COMMENT ON COLUMN proof_requests.status IS 'Overall request status: CREATED, PENDING, RUNNING, SUCCEEDED, FAILED';
