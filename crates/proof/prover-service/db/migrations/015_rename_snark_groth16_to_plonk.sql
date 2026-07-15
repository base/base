-- Migration 015: Hard-cutover SP1 SNARK proof type from Groth16 to PLONK.
--
-- Renames stored proof_type / api_proof_type labels and protocol-native JSON
-- discriminators. Invalidates historical Groth16 SNARK receipts (bytes are not
-- convertible) and fails affected SNARK jobs so they are not served as PLONK.
-- Rebuilds the ZK claim index so workers can find snark_plonk jobs.

BEGIN;

UPDATE proof_requests
SET proof_type = 'op_succinct_sp1_cluster_snark_plonk'
WHERE proof_type = 'op_succinct_sp1_cluster_snark_groth16';

-- Rename existing API discriminators and backfill legacy rows that still have
-- NULL api_proof_type after migration 010 (rolling-deploy inserts). Those rows
-- already have the renamed backend proof_type above; repository fallback would
-- otherwise treat them as snark_plonk while leaving Groth16 receipts in place.
UPDATE proof_requests
SET api_proof_type = 'snark_plonk'
WHERE api_proof_type = 'snark_groth16'
   OR (
        api_proof_type IS NULL
        AND proof_type = 'op_succinct_sp1_cluster_snark_plonk'
   );

-- Rewrite protocol-native request JSON discriminators when present.
-- jsonb::text is space-normalized (`"proof_type": "…"`), so match that form only.
UPDATE proof_requests
SET request_payload = replace(
        request_payload::text,
        '"proof_type": "snark_groth16"',
        '"proof_type": "snark_plonk"'
    )::jsonb
WHERE request_payload IS NOT NULL
  AND request_payload::text LIKE '%snark_groth16%';

-- Clear Groth16 snark receipts / result payloads so nothing serves them as PLONK.
-- Match both api_proof_type and backend proof_type so any remaining NULL-api
-- legacy rows are covered even if the backfill above is skipped somehow.
UPDATE proof_requests
SET snark_receipt = NULL,
    result_payload = NULL
WHERE (
        api_proof_type = 'snark_plonk'
        OR proof_type = 'op_succinct_sp1_cluster_snark_plonk'
    )
  AND (snark_receipt IS NOT NULL OR result_payload IS NOT NULL);

-- Fail in-flight and previously-succeeded SNARK requests whose Groth16 results
-- were invalidated above (otherwise SUCCEEDED rows would report success with
-- no receipt via getProof).
UPDATE proof_requests
SET status = 'FAILED',
    job_status = 'FAILED',
    error_message = 'invalidated by migration 015: SP1 SNARK hard-cutover from Groth16 to PLONK',
    completed_at = NOW()
WHERE (
        api_proof_type = 'snark_plonk'
        OR proof_type = 'op_succinct_sp1_cluster_snark_plonk'
    )
  AND status IN ('CREATED', 'PENDING', 'RUNNING', 'SUCCEEDED');

UPDATE proof_sessions
SET status = 'FAILED',
    error_message = 'invalidated by migration 015: SP1 SNARK hard-cutover from Groth16 to PLONK',
    completed_at = NOW()
WHERE status IN ('SUBMITTING', 'RUNNING', 'COMPLETED')
  AND proof_request_id IN (
      SELECT id
      FROM proof_requests
      WHERE api_proof_type = 'snark_plonk'
         OR proof_type = 'op_succinct_sp1_cluster_snark_plonk'
  );

-- Migration 014's claim index predicates on snark_groth16. Recreate it for
-- snark_plonk so worker claim queries can use the purpose-built index.
DROP INDEX IF EXISTS idx_proof_requests_zk_job_claim;
CREATE INDEX IF NOT EXISTS idx_proof_requests_zk_job_claim
ON proof_requests(
    job_status,
    api_proof_type,
    zk_vm,
    (COALESCE(zk_backend, 'cluster')),
    start_block_number,
    created_at
)
WHERE api_proof_type IN ('compressed', 'snark_plonk');

COMMENT ON COLUMN proof_requests.api_proof_type IS 'Protocol proof type: compressed, snark_plonk, tee.';

COMMIT;
