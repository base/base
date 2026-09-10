-- Add retry_count to proof_requests for auto-retry of stuck PENDING requests.
-- The stuck detector resets requests to CREATED up to MAX_PROOF_RETRIES times
-- before permanently failing them.
ALTER TABLE proof_requests ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;
