-- Migration 014: Persist the ZK proving backend selected by each request.
--
-- Callers choose mock / dry_run / cluster / network per request. Workers claim
-- jobs whose zk_backend is in their advertised capability list.
BEGIN;

ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS zk_backend VARCHAR(32);

-- Preserve historical updated_at values while backfilling.
ALTER TABLE proof_requests DISABLE TRIGGER update_proof_requests_updated_at;

-- Existing ZK rows were always executed by cluster-configured hosts.
UPDATE proof_requests
SET zk_backend = COALESCE(zk_backend, 'cluster')
WHERE api_proof_type IN ('compressed', 'snark_groth16')
   OR (api_proof_type IS NULL AND proof_type IS NOT NULL);

ALTER TABLE proof_requests ENABLE TRIGGER update_proof_requests_updated_at;

-- Keep NULL equivalent to the compatibility default while legacy service
-- binaries may still insert rows during a rolling deployment.
CREATE INDEX IF NOT EXISTS idx_proof_requests_zk_job_claim
ON proof_requests(
    job_status,
    api_proof_type,
    zk_vm,
    (COALESCE(zk_backend, 'cluster')),
    start_block_number,
    created_at
)
WHERE api_proof_type IN ('compressed', 'snark_groth16');

COMMENT ON COLUMN proof_requests.zk_backend IS 'Protocol ZK proving backend: mock, dry_run, cluster, network.';

COMMIT;
