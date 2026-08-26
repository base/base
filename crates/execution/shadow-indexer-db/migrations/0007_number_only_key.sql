-- Re-keys `shadow_blocks` on `number` alone and drops `reorged_out`.
--
-- Destructive: collapses each height to its newest discarded candidate and
-- deletes the pre-#4624 canonical rows. Neither survives a number-only key.
--
-- Deploy shadow-indexer before shadow-metrics. Neither binary can be rolled
-- back across this migration.

SET LOCAL lock_timeout = '5s';
-- `ADD PRIMARY KEY` and the rebuilt index each scan the full table and cannot
-- use CONCURRENTLY inside the transaction sqlx wraps this migration in.
SET LOCAL statement_timeout = '300s';

DELETE FROM shadow_blocks WHERE reorged_out = false;

-- `hash` breaks `updated_at` ties so the surviving row is deterministic.
DELETE FROM shadow_blocks AS older
  USING shadow_blocks AS newer
  WHERE older.number = newer.number
    AND (older.updated_at, older.hash) < (newer.updated_at, newer.hash);

ALTER TABLE shadow_blocks DROP CONSTRAINT shadow_blocks_pkey;
ALTER TABLE shadow_blocks DROP COLUMN reorged_out;
ALTER TABLE shadow_blocks ADD PRIMARY KEY (number);

DROP INDEX IF EXISTS idx_shadow_blocks_number;

DROP INDEX IF EXISTS idx_shadow_blocks_updated_at;
CREATE INDEX idx_shadow_blocks_updated_at ON shadow_blocks(updated_at, number);

-- Backs the unresolved-backlog gauges the reader polls each tick.
CREATE INDEX idx_shadow_blocks_unresolved
  ON shadow_blocks(created_at) WHERE canonical_hash IS NULL;

-- The cursor row is kept: clearing it sends the reader through `max_cursor`,
-- which reports the live tip and skips everything written before
-- shadow-metrics is rolled.
ALTER TABLE shadow_metrics_cursor DROP COLUMN last_hash;
