ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS tee_signer TEXT;

CREATE INDEX IF NOT EXISTS idx_proof_requests_tee_signer
ON proof_requests(tee_signer)
WHERE tee_signer IS NOT NULL AND status = 'SUCCEEDED';
