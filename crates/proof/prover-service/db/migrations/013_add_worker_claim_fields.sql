-- Migration 013: Add worker-owned claim and lock fields to proof_requests.
--
-- The external worker API (getNextProof / heartbeat / submitProof) claims proof
-- jobs directly from proof_requests. This separates three lifecycles on one row:
--   * requester-facing proof lifecycle: `status` (CREATED/PENDING/RUNNING/SUCCEEDED/FAILED)
--   * worker-facing job lifecycle:       `job_status` (PENDING/CLAIMED/SUCCEEDED/FAILED)
--   * backend session lifecycle:         existing `proof_sessions` rows
--
-- Invariant: a CLAIMED job must have worker_id, lock_id, and lock_expires_at set.
ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS job_status VARCHAR(20) NOT NULL DEFAULT 'PENDING';
ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS worker_id TEXT;
ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS lock_id UUID;
ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS lock_expires_at TIMESTAMP WITH TIME ZONE;
ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS claimed_at TIMESTAMP WITH TIME ZONE;
ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS attempt INTEGER NOT NULL DEFAULT 0;
ALTER TABLE proof_requests ADD COLUMN IF NOT EXISTS last_heartbeat_at TIMESTAMP WITH TIME ZONE;

-- Backfill job_status from the requester status so terminal rows are never
-- exposed as claimable. Preserve historical updated_at while backfilling.
ALTER TABLE proof_requests DISABLE TRIGGER update_proof_requests_updated_at;

UPDATE proof_requests
SET job_status = CASE status
    WHEN 'SUCCEEDED' THEN 'SUCCEEDED'
    WHEN 'FAILED' THEN 'FAILED'
    ELSE 'PENDING'
END;

ALTER TABLE proof_requests ENABLE TRIGGER update_proof_requests_updated_at;

-- Claim queries scan for the oldest claimable job of a given proof type.
CREATE INDEX IF NOT EXISTS idx_proof_requests_job_claim
ON proof_requests(job_status, api_proof_type, created_at);

-- Ownership lookups by fencing token.
CREATE INDEX IF NOT EXISTS idx_proof_requests_lock_id
ON proof_requests(lock_id);

-- Worker-scoped ownership checks.
CREATE INDEX IF NOT EXISTS idx_proof_requests_worker_job
ON proof_requests(worker_id, job_status);

-- Expired-lock reaping without a full table scan.
CREATE INDEX IF NOT EXISTS idx_proof_requests_lock_expiry
ON proof_requests(lock_expires_at)
WHERE job_status = 'CLAIMED';

COMMENT ON COLUMN proof_requests.job_status IS 'Worker-owned job lifecycle: PENDING, CLAIMED, SUCCEEDED, FAILED.';
COMMENT ON COLUMN proof_requests.worker_id IS 'Worker that currently holds (or last held) the job claim.';
COMMENT ON COLUMN proof_requests.lock_id IS 'Fencing token issued on claim; rotated on every (re)claim.';
COMMENT ON COLUMN proof_requests.lock_expires_at IS 'Wall-clock time when the current worker claim expires.';
COMMENT ON COLUMN proof_requests.claimed_at IS 'Time when the current worker claim was acquired.';
COMMENT ON COLUMN proof_requests.attempt IS 'Number of times this job has been claimed; bounds the retry budget.';
COMMENT ON COLUMN proof_requests.last_heartbeat_at IS 'Time of the most recent worker heartbeat for the current claim.';
