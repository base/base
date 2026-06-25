-- Migration 009: Enforce at most one active session per (proof_request, session_type)
-- to prevent concurrent pollers from creating duplicate SNARK Groth16 jobs on the
-- SP1 cluster. See CHAIN-4254 (Immunefi 75630).
--
-- The index is partial on (SUBMITTING, RUNNING); terminal sessions remain as audit
-- history. Pre-existing duplicates are failed (not deleted) to preserve the audit
-- trail, keeping the newest row active.

BEGIN;

-- Fail duplicate active sessions, keeping the newest per (proof_request_id, session_type).
WITH ranked AS (
    SELECT
        id,
        ROW_NUMBER() OVER (
            PARTITION BY proof_request_id, session_type
            ORDER BY created_at DESC, id DESC
        ) AS row_num
    FROM proof_sessions
    WHERE status IN ('SUBMITTING', 'RUNNING')
)
UPDATE proof_sessions
SET status = 'FAILED',
    error_message = 'duplicate session cleared by migration 009',
    completed_at = NOW()
FROM ranked
WHERE proof_sessions.id = ranked.id
  AND ranked.row_num > 1;

CREATE UNIQUE INDEX IF NOT EXISTS idx_proof_sessions_request_type_active_unique
ON proof_sessions(proof_request_id, session_type)
WHERE status IN ('SUBMITTING', 'RUNNING');

COMMIT;
