-- Migration 012: Store protocol-native proof results.
--
-- `result_payload` is the canonical requester/worker API result shape. The
-- legacy STARK/SNARK receipt columns remain populated during the compatibility
-- period for existing ZK service paths.
ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS result_payload JSONB;
ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS submitted_by_worker_id TEXT;
ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS submitted_lock_id TEXT;

-- Preserve historical updated_at values while backfilling protocol results.
BEGIN;

ALTER TABLE proof_requests DISABLE TRIGGER update_proof_requests_updated_at;

UPDATE proof_requests
SET result_payload = CASE
    WHEN COALESCE(
        api_proof_type,
        CASE proof_type
            WHEN 'op_succinct_sp1_cluster_snark_groth16' THEN 'snark_groth16'
            ELSE 'compressed'
        END
    ) = 'snark_groth16' AND snark_receipt IS NOT NULL THEN jsonb_build_object(
        'proof_type', 'snark_groth16',
        'payload', jsonb_build_object(
            'proof', jsonb_build_object(
                'zk_vm', COALESCE(zk_vm, 'sp1'),
                'proof', concat('0x', encode(snark_receipt, 'hex'))
            )
        )
    )
    WHEN COALESCE(
        api_proof_type,
        CASE proof_type
            WHEN 'op_succinct_sp1_cluster_snark_groth16' THEN 'snark_groth16'
            ELSE 'compressed'
        END
    ) = 'compressed' AND stark_receipt IS NOT NULL THEN jsonb_build_object(
        'proof_type', 'compressed',
        'payload', jsonb_build_object(
            'zk_vm', COALESCE(zk_vm, 'sp1'),
            'proof', concat('0x', encode(stark_receipt, 'hex'))
        )
    )
    ELSE result_payload
END
WHERE result_payload IS NULL
  AND (stark_receipt IS NOT NULL OR snark_receipt IS NOT NULL);

ALTER TABLE proof_requests ENABLE TRIGGER update_proof_requests_updated_at;

COMMIT;

COMMENT ON COLUMN proof_requests.result_payload IS 'Protocol ProofResult payload serialized as JSONB.';
COMMENT ON COLUMN proof_requests.submitted_by_worker_id IS 'Worker id that submitted result_payload, when completed through the worker API.';
COMMENT ON COLUMN proof_requests.submitted_lock_id IS 'Worker lock token that submitted result_payload, when completed through the worker API.';
