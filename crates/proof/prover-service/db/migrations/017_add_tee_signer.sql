ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS tee_signer TEXT;

ALTER TABLE proof_request_outbox
DROP CONSTRAINT proof_request_outbox_proof_request_id_fkey,
ADD CONSTRAINT proof_request_outbox_proof_request_id_fkey
FOREIGN KEY (proof_request_id) REFERENCES proof_requests(id) ON DELETE CASCADE;

CREATE INDEX IF NOT EXISTS idx_proof_requests_tee_signer
ON proof_requests(tee_signer)
WHERE tee_signer IS NOT NULL AND status = 'SUCCEEDED';
