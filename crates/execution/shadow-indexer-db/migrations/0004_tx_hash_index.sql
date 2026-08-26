-- Transaction-hash lookup for the explorer API. Hashes are already in the JSONB
-- payload at $.block.block.body.transactions[*].hash (outer `block` is the
-- ShadowBlockPayload field, inner `block` is the RecoveredBlock's sealed block).
-- GIN over the extracted array answers `@> to_jsonb($hash)` and CREATE INDEX
-- populates it from existing rows. Plain (not CONCURRENTLY): sqlx migrations run
-- inside a transaction.
CREATE INDEX IF NOT EXISTS idx_shadow_blocks_tx_hashes
  ON shadow_blocks
  USING gin ((jsonb_path_query_array(payload, '$.block.block.body.transactions[*].hash')) jsonb_path_ops);
