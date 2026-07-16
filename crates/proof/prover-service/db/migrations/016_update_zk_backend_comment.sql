-- Migration 016: Update zk_backend column comment after removing the mock backend.
--
-- The placeholder mock backend was removed from the protocol Rust types. This
-- migration only refreshes the column comment to match the remaining values.
BEGIN;

COMMENT ON COLUMN proof_requests.zk_backend IS 'Protocol ZK proving backend: dry_run, cluster, network.';

COMMIT;
