-- Stores cluster proof handle data (proof_id + proof_output_id) as JSONB for
-- self-hosted cluster proving mode. This column is NULL for network mode requests.
-- Network mode uses proof_request_id (BYTEA, 32-byte B256) instead.
-- The two columns serve separate code paths and are never cross-read.
ALTER TABLE requests ADD COLUMN cluster_proof_handle JSONB;
