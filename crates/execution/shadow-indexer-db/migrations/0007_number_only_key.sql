-- Re-keys `shadow_blocks` on `number` alone and drops `reorged_out`.
--
-- Canonical blocks stopped being persisted in #4624, so every row the ExEx
-- writes is a block the chain discarded and `reorged_out` no longer separates
-- anything. With canonical rows gone a height holds one shadow block, so `hash`
-- stops being part of the row's identity: a later `ChainCommitted` resolves the
-- row at its height through `ON CONFLICT (number)` instead of a second
-- statement keyed on the old composite.
--
-- Rows are truncated rather than migrated. They only feed Prometheus metrics
-- the reader re-derives from live traffic within one reorg, and the pre-#4624
-- canonical rows would collide on `number` with their shadow siblings anyway.
-- TRUNCATE rather than DROP TABLE: the table object keeps its grants, so the
-- separately-deployed shadow-metrics role does not lose SELECT, and the
-- explorer indexes from 0004-0006 survive untouched.
--
-- The lock guards match 0004: fail fast and let the container retry rather than
-- block startup indefinitely behind a shadow-metrics poll holding AccessShare.
--
-- Deploy shadow-indexer before shadow-metrics: the indexer applies this at
-- startup via `ShadowDbConfig::init_pool`, and a reader that still projects
-- `reorged_out` or writes `last_hash` errors on every poll until it is rolled.

SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '30s';

TRUNCATE shadow_blocks;

ALTER TABLE shadow_blocks DROP CONSTRAINT shadow_blocks_pkey;
ALTER TABLE shadow_blocks DROP COLUMN reorged_out;
ALTER TABLE shadow_blocks ADD PRIMARY KEY (number);

-- Redundant now that `number` is the primary key.
DROP INDEX IF EXISTS idx_shadow_blocks_number;

-- Rebuilt without the dead `hash` tie-breaker, still in the order the metrics
-- cursor scans so the row-value comparison in `list_reorged_since` is
-- index-backed.
DROP INDEX IF EXISTS idx_shadow_blocks_updated_at;
CREATE INDEX idx_shadow_blocks_updated_at ON shadow_blocks(updated_at, number);

-- `number` is unique, so `updated_at` ties break on it alone.
ALTER TABLE shadow_metrics_cursor DROP COLUMN last_hash;

-- The cursor outlived every row it pointed at. Clearing it sends the reader
-- back through `max_cursor`, which reports the empty table and starts at
-- genesis rather than skipping rows written below a stale watermark.
DELETE FROM shadow_metrics_cursor;
