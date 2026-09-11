-- no-transaction
--
-- The text counterpart of idx_shadow_blocks_canonical_hash, backing the
-- shadow-candidates-by-canonical lookups (`repo::list_reorged_by_canonical` and
-- its batch form). Partial on NOT NULL for the same reason as the original: an
-- unresolved row has no replacement to be found by.
--
-- See 0011 for why this is `-- no-transaction`, one statement, and IF NOT EXISTS.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_shadow_blocks_canonical_hash_hex
  ON shadow_blocks (canonical_hash_hex) WHERE canonical_hash_hex IS NOT NULL;
