-- The ZK range program samples intermediate roots at a fixed interval, and TEE
-- proofs keep their checkpoint stride inside `request_payload`. The denormalized
-- column is unused.
--
-- Copy a stored TEE stride into payloads that do not already record one, so
-- dropping the column does not erase it.
UPDATE proof_requests
SET request_payload = jsonb_set(
    request_payload,
    '{request,payload,proof,intermediate_block_interval}',
    to_jsonb(intermediate_root_interval),
    true
)
WHERE api_proof_type = 'tee'
  AND request_payload IS NOT NULL
  AND intermediate_root_interval IS NOT NULL
  AND COALESCE(
      request_payload #>> '{request,payload,proof,intermediate_block_interval}',
      '0'
  ) = '0';

ALTER TABLE proof_requests DROP COLUMN intermediate_root_interval;
