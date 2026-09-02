-- Persist the exact TEE image or ZK artifact hash used for worker routing.
--
-- ==========================================================================
-- DEVIATION FROM THE TDD -- READ BEFORE DEPLOYING
-- ==========================================================================
-- The TDD ("Proof Artifact-Aware Job Routing", 2026-08-04) specifies that this
-- migration backfills every incomplete job with the deployed legacy artifact
-- hashes, making Phase 1 a true no-op. This migration deliberately does NOT
-- backfill: the correct hashes are environment-specific (they differ per
-- network and per deployed AggregateVerifier implementation) and this file
-- cannot derive them without guessing.
--
-- Consequences, all of which must be handled operationally:
--
--   1. Every pre-existing row keeps artifact_hash = NULL. A NULL never
--      satisfies the claim predicate (`artifact_hash = $N`), so those jobs are
--      UNCLAIMABLE, not merely unrouted. They will never be picked up again.
--
--   2. This makes Phase 1 a BREAKING deploy, not the no-op the TDD describes.
--      In-flight jobs are stranded rather than backfilled-and-completed.
--
--   3. The TDD's required Phase 1 end-to-end test step 3 ("Confirm the
--      in-flight job is backfilled and still completes") CANNOT pass as
--      written and must be replaced by the drain check below.
--
-- Required deploy sequence (replaces TDD rollout step 3):
--
--   a. Pause the proposer and challenger so no new jobs are created.
--   b. Let the worker queue drain: block until
--        SELECT COUNT(*) FROM proof_requests
--        WHERE job_status IN ('PENDING', 'CLAIMED');
--      returns 0. Do not proceed while this is non-zero.
--   c. Apply this migration.
--   d. Deploy the artifact-aware prover service, proposer, and challenger.
--   e. Redeploy workers so they advertise their artifact hashes.
--
-- If draining is not acceptable, backfill instead of draining, using the hashes
-- read from the currently deployed AggregateVerifier implementation:
--
--   UPDATE proof_requests SET artifact_hash = '\x<legacy TEE_IMAGE_HASH>'
--   WHERE artifact_hash IS NULL AND api_proof_type = 'tee'
--     AND job_status IN ('PENDING', 'CLAIMED');
--
--   UPDATE proof_requests
--   SET artifact_hash = '\x<keccak256(ZK_RANGE_HASH || ZK_AGGREGATE_HASH)>'
--   WHERE artifact_hash IS NULL AND api_proof_type IN ('compressed', 'snark_plonk')
--     AND job_status IN ('PENDING', 'CLAIMED');
--
-- Note ZK_RANGE_HASH and ZK_AGGREGATE_HASH use *different* SP1 digests; derive
-- the composite with `cargo run -p base-proof-zk-backend --bin vkeys` rather
-- than by hand.
--
-- Monitoring: any row that reaches the queue without a hash is reported by the
-- `prover_service.pending_jobs_by_artifact` gauge under artifact_hash="none".
-- A non-zero value there after deploy means jobs are stranded.
-- ==========================================================================
BEGIN;

ALTER TABLE proof_requests
ADD COLUMN IF NOT EXISTS artifact_hash BYTEA;

ALTER TABLE proof_requests
DROP CONSTRAINT IF EXISTS proof_requests_artifact_hash_length;

ALTER TABLE proof_requests
ADD CONSTRAINT proof_requests_artifact_hash_length
CHECK (artifact_hash IS NULL OR octet_length(artifact_hash) = 32);

DROP INDEX IF EXISTS idx_proof_requests_job_claim;
CREATE INDEX idx_proof_requests_tee_job_claim
ON proof_requests(
    job_status,
    api_proof_type,
    tee_kind,
    artifact_hash,
    start_block_number,
    created_at
)
WHERE api_proof_type = 'tee';

DROP INDEX IF EXISTS idx_proof_requests_zk_job_claim;
CREATE INDEX idx_proof_requests_zk_job_claim
ON proof_requests(
    job_status,
    api_proof_type,
    zk_vm,
    (COALESCE(zk_backend, 'cluster')),
    artifact_hash,
    start_block_number,
    created_at
)
WHERE api_proof_type IN ('compressed', 'snark_plonk');

COMMENT ON COLUMN proof_requests.artifact_hash IS
'Exact 32-byte TEE image or ZK artifact hash required for worker routing. NULL legacy rows are nonclaimable until operationally backfilled.';

COMMIT;
