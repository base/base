ALTER TABLE proof_requests
ADD COLUMN IF NOT EXISTS request_protocol_version BIGINT NOT NULL DEFAULT 0;

COMMENT ON COLUMN proof_requests.request_protocol_version IS
'Opaque prover routing version required by this job; 0 is the compatibility default.';

-- Claim queries now match request_protocol_version. Without it in the index, unclaimable rows of
-- the other protocol sort ahead of claimable ones (ORDER BY start_block_number ASC) and are
-- rechecked on every claim, which degrades as drained legacy jobs accumulate.
--
-- The replacements are created under new names before the originals are dropped, so no window
-- exists in which a claim query has no covering index. CREATE INDEX takes a SHARE lock that blocks
-- writes to proof_requests while it builds; that is a bounded pause on claims rather than the
-- unbounded sequential scans a drop-then-create would cause.
CREATE INDEX IF NOT EXISTS idx_proof_requests_job_claim_by_version
ON proof_requests(
    job_status,
    api_proof_type,
    request_protocol_version,
    start_block_number,
    created_at
);
DROP INDEX IF EXISTS idx_proof_requests_job_claim;

CREATE INDEX IF NOT EXISTS idx_proof_requests_zk_job_claim_by_version
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
DROP INDEX IF EXISTS idx_proof_requests_zk_job_claim;
