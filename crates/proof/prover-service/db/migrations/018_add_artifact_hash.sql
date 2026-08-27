-- Persist the exact TEE image or ZK artifact hash used for worker routing.
--
-- Existing rows intentionally remain NULL and therefore cannot be claimed by
-- artifact-aware workers. Operators must backfill deployed legacy artifact
-- hashes before rolling out workers that require those jobs; this migration
-- does not guess environment-specific constants.
BEGIN;

ALTER TABLE proof_requests
ADD COLUMN IF NOT EXISTS artifact_hash BYTEA;

ALTER TABLE proof_requests
DROP CONSTRAINT IF EXISTS proof_requests_artifact_hash_length;

ALTER TABLE proof_requests
ADD CONSTRAINT proof_requests_artifact_hash_length
CHECK (artifact_hash IS NULL OR octet_length(artifact_hash) = 32);

DROP INDEX IF EXISTS idx_proof_requests_job_claim;
CREATE INDEX idx_proof_requests_tee_job_claim
ON proof_requests(
    job_status,
    api_proof_type,
    tee_kind,
    artifact_hash,
    start_block_number,
    created_at
)
WHERE api_proof_type = 'tee';

DROP INDEX IF EXISTS idx_proof_requests_zk_job_claim;
CREATE INDEX idx_proof_requests_zk_job_claim
ON proof_requests(
    job_status,
    api_proof_type,
    zk_vm,
    (COALESCE(zk_backend, 'cluster')),
    artifact_hash,
    start_block_number,
    created_at
)
WHERE api_proof_type IN ('compressed', 'snark_plonk');

COMMENT ON COLUMN proof_requests.artifact_hash IS
'Exact 32-byte TEE image or ZK artifact hash required for worker routing. NULL legacy rows are nonclaimable until operationally backfilled.';

COMMIT;
