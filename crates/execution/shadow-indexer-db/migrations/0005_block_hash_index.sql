-- Block-hash equality lookups for the explorer. The (number, hash) primary key
-- leads on number, so `WHERE hash = $1` alone is not index-backed; this btree
-- serves the block-by-hash endpoint across canonical and reorged-out rows.
CREATE INDEX IF NOT EXISTS idx_shadow_blocks_hash ON shadow_blocks(hash);
