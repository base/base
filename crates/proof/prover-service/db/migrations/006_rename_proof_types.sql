-- Rename proof_type values from generic_zkvm_cluster_* to op_succinct_sp1_cluster_*
UPDATE proof_requests
SET proof_type = 'op_succinct_sp1_cluster_compressed'
WHERE proof_type = 'generic_zkvm_cluster_compressed';

UPDATE proof_requests
SET proof_type = 'op_succinct_sp1_cluster_snark_groth16'
WHERE proof_type = 'generic_zkvm_cluster_snark_groth16';

-- Update the column default to the new naming
ALTER TABLE proof_requests
ALTER COLUMN proof_type SET DEFAULT 'op_succinct_sp1_cluster_compressed';
