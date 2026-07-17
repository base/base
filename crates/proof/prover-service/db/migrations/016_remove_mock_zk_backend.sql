-- Migration 016: Remove mock ZK backend; rewrite stored 'mock' values to dry_run.
BEGIN;

UPDATE proof_requests
SET zk_backend = 'dry_run'
WHERE zk_backend = 'mock';

UPDATE proof_requests
SET request_payload = jsonb_set(
    request_payload,
    '{request,payload,zk_backend}',
    '"dry_run"'
)
WHERE request_payload IS NOT NULL
  AND request_payload #>> '{request,proof_type}' = 'compressed'
  AND request_payload #>> '{request,payload,zk_backend}' = 'mock';

UPDATE proof_requests
SET request_payload = jsonb_set(
    request_payload,
    '{request,payload,proof,zk_backend}',
    '"dry_run"'
)
WHERE request_payload IS NOT NULL
  AND request_payload #>> '{request,proof_type}' = 'snark_plonk'
  AND request_payload #>> '{request,payload,proof,zk_backend}' = 'mock';

COMMENT ON COLUMN proof_requests.zk_backend IS 'Protocol ZK proving backend: dry_run, cluster, network.';

COMMIT;
