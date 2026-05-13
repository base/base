-- Migration 009: Enforce at most one ACTIVE session per (proof_request, session_type)
-- Prevents concurrent status pollers from creating duplicate SNARK Groth16 jobs on
-- the SP1 cluster. See CHAIN-4254 (Immunefi 75630).
--
-- The index is partial on the active states (SUBMITTING and RUNNING) so terminal
-- (FAILED/COMPLETED) sessions remain as audit history without blocking a retried
-- request from creating a fresh session for the same (proof_request_id, session_type)
-- pair. SUBMITTING covers the reservation window between slot reservation and the
-- moment the row is updated with the real backend session id; RUNNING covers the
-- live backend job thereafter.

-- Resolve any pre-existing active duplicates before adding the constraint. Terminal
-- duplicates are left alone since the partial index does not see them. SUBMITTING
-- did not exist before this migration, so only RUNNING rows can be duplicated here.
WITH ranked AS (
    SELECT
        id,
        ROW_NUMBER() OVER (
            PARTITION BY proof_request_id, session_type
            ORDER BY created_at ASC, id ASC
        ) AS row_num
    FROM proof_sessions
    WHERE status IN ('SUBMITTING', 'RUNNING')
)
DELETE FROM proof_sessions
USING ranked
WHERE proof_sessions.id = ranked.id
  AND ranked.row_num > 1;

CREATE UNIQUE INDEX IF NOT EXISTS idx_proof_sessions_request_type_active_unique
ON proof_sessions(proof_request_id, session_type)
WHERE status IN ('SUBMITTING', 'RUNNING');
