ALTER TABLE proof_requests
ADD COLUMN IF NOT EXISTS request_protocol_version BIGINT NOT NULL DEFAULT 0;

COMMENT ON COLUMN proof_requests.request_protocol_version IS
'Prover protocol required by this job; version 0 is the legacy journal without schedule pinning.';

-- Claim queries now match request_protocol_version exactly. Without it in the index, unclaimable
-- rows of the other protocol sort ahead of claimable ones (ORDER BY start_block_number ASC) and are
-- rechecked on every claim, which degrades as drained legacy jobs accumulate.
DROP INDEX IF EXISTS idx_proof_requests_job_claim;
CREATE INDEX IF NOT EXISTS idx_proof_requests_job_claim
ON proof_requests(
    job_status,
    api_proof_type,
    request_protocol_version,
    start_block_number,
    created_at
);

DROP INDEX IF EXISTS idx_proof_requests_zk_job_claim;
CREATE INDEX IF NOT EXISTS idx_proof_requests_zk_job_claim
ON proof_requests(
    job_status,
    api_proof_type,
    zk_vm,
    (COALESCE(zk_backend, 'cluster')),
    request_protocol_version,
    start_block_number,
    created_at
)
WHERE api_proof_type IN ('compressed', 'snark_plonk');
