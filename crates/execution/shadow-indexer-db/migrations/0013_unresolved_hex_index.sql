-- no-transaction
--
-- Rebuilds the unresolved-backlog index against the text column.
--
-- This one is easy to miss. `idx_shadow_blocks_unresolved` indexes `created_at`
-- but is partial on `WHERE canonical_hash IS NULL`, so its *predicate* depends
-- on a column migration 0015 drops. Postgres drops such an index along with the
-- column, without complaint and without naming it. The loss would be silent and
-- slow rather than loud: `repo::unresolved_backlog` runs on every retention tick
-- and would quietly start sequentially scanning the whole table.
--
-- Building the replacement here, before the drop, keeps that query index-backed
-- across the cutover. 0015 renames this onto the original name.
--
-- See 0011 for why this is `-- no-transaction`, one statement, and IF NOT EXISTS.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_shadow_blocks_unresolved_hex
  ON shadow_blocks (created_at) WHERE canonical_hash_hex IS NULL;
