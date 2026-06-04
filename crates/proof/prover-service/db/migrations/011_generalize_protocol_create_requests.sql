-- Migration 011: Generalize create-time protocol request storage.
--
-- The worker-facing protocol uses opaque string session ids and can represent
-- proof kinds that do not have an OP Succinct backend proof_type.
ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS session_id TEXT;

-- Preserve historical updated_at values while backfilling public session ids.
ALTER TABLE proof_requests DISABLE TRIGGER update_proof_requests_updated_at;

UPDATE proof_requests
SET session_id = id::text
WHERE session_id IS NULL;

ALTER TABLE proof_requests ENABLE TRIGGER update_proof_requests_updated_at;

-- Keep session_id nullable during the expand phase so legacy writers can keep
-- inserting proof_requests. The effective id still remains unique: new protocol
-- rows use session_id, while legacy rows fall back to id::text.
CREATE UNIQUE INDEX IF NOT EXISTS idx_proof_requests_effective_session_id
ON proof_requests((COALESCE(session_id, id::text)));

ALTER TABLE proof_requests ALTER COLUMN proof_type DROP NOT NULL;

COMMENT ON COLUMN proof_requests.session_id IS 'Public protocol session identifier. Defaults to id::text for legacy rows.';
COMMENT ON COLUMN proof_requests.proof_type IS 'Backend-specific OP Succinct proof type for ZK requests; NULL for protocol proof kinds without an internal ZK backend.';
