-- Migration 010: Store protocol-native requester fields on proof_requests.
--
ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS request_payload JSONB;
ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS api_proof_type VARCHAR(32);
ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS zk_vm VARCHAR(32);
ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS tee_kind VARCHAR(32);

-- Preserve historical updated_at values while backfilling protocol fields.
ALTER TABLE proof_requests DISABLE TRIGGER update_proof_requests_updated_at;

UPDATE proof_requests
SET
    api_proof_type = COALESCE(
        api_proof_type,
        CASE proof_type
            WHEN 'op_succinct_sp1_cluster_snark_groth16' THEN 'snark_groth16'
            ELSE 'compressed'
        END
    ),
    zk_vm = COALESCE(zk_vm, 'sp1'),
    request_payload = COALESCE(
        request_payload,
        CASE proof_type
            WHEN 'op_succinct_sp1_cluster_snark_groth16' THEN jsonb_build_object(
                'session_id', id::text,
                'request', jsonb_build_object(
                    'proof_type', 'snark_groth16',
                    'payload', jsonb_build_object(
                        'proof', jsonb_strip_nulls(jsonb_build_object(
                            'start_block_number', start_block_number,
                            'number_of_blocks_to_prove', number_of_blocks_to_prove,
                            'sequence_window', sequence_window,
                            'l1_head', l1_head,
                            'intermediate_root_interval', intermediate_root_interval,
                            'zk_vm', 'sp1'
                        )),
                        'prover_address', COALESCE(
                            prover_address,
                            '0x0000000000000000000000000000000000000000'
                        )
                    )
                )
            )
            ELSE jsonb_build_object(
                'session_id', id::text,
                'request', jsonb_build_object(
                    'proof_type', 'compressed',
                    'payload', jsonb_strip_nulls(jsonb_build_object(
                        'start_block_number', start_block_number,
                        'number_of_blocks_to_prove', number_of_blocks_to_prove,
                        'sequence_window', sequence_window,
                        'l1_head', l1_head,
                        'intermediate_root_interval', intermediate_root_interval,
                        'zk_vm', 'sp1'
                    ))
                )
            )
        END
    );

ALTER TABLE proof_requests ENABLE TRIGGER update_proof_requests_updated_at;

-- Keep these columns nullable during the expand phase so old service binaries
-- can continue inserting proof_requests during rolling deploys.

CREATE INDEX IF NOT EXISTS idx_proof_requests_api_proof_type
ON proof_requests(api_proof_type);

COMMENT ON COLUMN proof_requests.request_payload IS 'Original protocol ProofRequest payload serialized as JSONB.';
COMMENT ON COLUMN proof_requests.api_proof_type IS 'Protocol proof type: compressed, snark_groth16, tee.';
COMMENT ON COLUMN proof_requests.zk_vm IS 'Protocol ZK VM discriminator for ZK proofs.';
COMMENT ON COLUMN proof_requests.tee_kind IS 'Protocol TEE implementation discriminator for TEE proofs.';
