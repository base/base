-- Migration 016: Remove the placeholder mock ZK backend from stored values.
--
-- Rewrite any remaining zk_backend = 'mock' rows to dry_run, then refresh the
-- column comment. The Mock protocol variant has been removed from Rust.
BEGIN;

UPDATE proof_requests
SET zk_backend = 'dry_run'
WHERE zk_backend = 'mock';

COMMENT ON COLUMN proof_requests.zk_backend IS 'Protocol ZK proving backend: dry_run, cluster, network.';

COMMIT;
