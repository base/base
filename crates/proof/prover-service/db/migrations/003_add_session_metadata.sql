-- Migration 003: Add metadata field to proof_sessions
-- This allows backends to store backend-specific data (e.g., artifact IDs, deadlines)

ALTER TABLE proof_sessions
ADD COLUMN metadata JSONB;

COMMENT ON COLUMN proof_sessions.metadata IS 'Backend-specific metadata (JSONB): proof_output_id, deadlines, etc.';

-- Create GIN index for JSONB queries if needed
CREATE INDEX IF NOT EXISTS idx_proof_sessions_metadata ON proof_sessions USING GIN (metadata);
