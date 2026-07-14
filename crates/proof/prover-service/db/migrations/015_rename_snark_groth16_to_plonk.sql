-- Migration 015: Hard-cutover SP1 SNARK proof type from Groth16 to PLONK.
--
-- Renames stored proof_type / api_proof_type labels and protocol-native JSON
-- discriminators. Invalidates historical Groth16 SNARK receipts (bytes are not
-- convertible) and fails in-flight SNARK jobs so they are not served as PLONK.

BEGIN;

UPDATE proof_requests
SET proof_type = 'op_succinct_sp1_cluster_snark_plonk'
WHERE proof_type = 'op_succinct_sp1_cluster_snark_groth16';

UPDATE proof_requests
SET api_proof_type = 'snark_plonk'
WHERE api_proof_type = 'snark_groth16';

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
UPDATE proof_requests
SET snark_receipt = NULL,
    result_payload = NULL
WHERE api_proof_type = 'snark_plonk'
  AND (snark_receipt IS NOT NULL OR result_payload IS NOT NULL);

-- Fail non-terminal SNARK requests / worker jobs mid Groth16 aggregation.
UPDATE proof_requests
SET status = 'FAILED',
    job_status = 'FAILED',
    error_message = 'invalidated by migration 015: SP1 SNARK hard-cutover from Groth16 to PLONK',
    completed_at = NOW()
WHERE api_proof_type = 'snark_plonk'
  AND status IN ('CREATED', 'PENDING', 'RUNNING');

UPDATE proof_sessions
SET status = 'FAILED',
    error_message = 'invalidated by migration 015: SP1 SNARK hard-cutover from Groth16 to PLONK',
    completed_at = NOW()
WHERE status IN ('SUBMITTING', 'RUNNING')
  AND proof_request_id IN (
      SELECT id
      FROM proof_requests
      WHERE api_proof_type = 'snark_plonk'
  );

COMMENT ON COLUMN proof_requests.api_proof_type IS 'Protocol proof type: compressed, snark_plonk, tee.';

COMMIT;
