-- Backs the shadow-candidates-by-canonical lookup (repo::list_reorged_by_canonical):
-- WHERE reorged_out = true AND canonical_hash = $1.
CREATE INDEX IF NOT EXISTS idx_shadow_blocks_canonical_hash
  ON shadow_blocks(canonical_hash) WHERE canonical_hash IS NOT NULL;
