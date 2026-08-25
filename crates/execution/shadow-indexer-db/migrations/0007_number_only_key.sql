-- Re-keys `shadow_blocks` on `number` alone and drops `reorged_out`.
--
-- Canonical blocks stopped being persisted in #4624, so every row the ExEx
-- writes is a block the chain discarded and `reorged_out` no longer separates
-- anything. With canonical rows gone a height holds one shadow block, so `hash`
-- stops being part of the row's identity: a later `ChainCommitted` resolves the
-- row at its height through `ON CONFLICT (number)` instead of a second
-- statement keyed on the old composite.
--
-- Rows are migrated, not truncated: `shadow_blocks` also backs the explorer
-- endpoints added in #4571 (`/blocks/{id}`, `/shadow-blocks/{id}`,
-- `/shadow-candidates`), and nothing prunes it, so the table is the whole
-- history rather than a metrics scratch buffer. Collapsing to the new key
-- discards two classes of row that cannot survive it:
--   * pre-#4624 canonical rows, which are not discarded blocks and collide on
--     `number` with the shadow sibling that supersedes them;
--   * older candidates at a height, matching the going-forward semantics where
--     a second reorg at one height replaces the row rather than storing a
--     sibling.
--
-- `lock_timeout` stays short so a shadow-metrics poll holding AccessShare fails
-- the migration fast and lets the container retry instead of blocking startup.
-- `statement_timeout` is raised because `ADD PRIMARY KEY` and the rebuilt
-- `updated_at` index each scan the full table, and neither can run CONCURRENTLY
-- inside the transaction sqlx wraps this migration in.
--
-- Deploy shadow-indexer before shadow-metrics: the indexer applies this at
-- startup via `ShadowDbConfig::init_pool`, and a reader that still projects
-- `reorged_out` or writes `last_hash` errors on every poll until it is rolled.
-- The indexer's own rollout has the same shape in reverse: an old writer pod
-- still naming `reorged_out` fails every flush until it is replaced.

SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '300s';

-- Not discarded blocks; `reorged_out` is about to stop distinguishing them.
DELETE FROM shadow_blocks WHERE reorged_out = false;

-- One candidate per height, newest first. `hash` breaks `updated_at` ties so the
-- choice is deterministic rather than dependent on physical row order.
DELETE FROM shadow_blocks AS older
  USING shadow_blocks AS newer
  WHERE older.number = newer.number
    AND (older.updated_at, older.hash) < (newer.updated_at, newer.hash);

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

-- Backs the unresolved-backlog gauges. Rows awaiting a canonical block are a small
-- fraction of the table, so a partial index keeps the reader's per-poll COUNT/MIN
-- off a full scan. Complements 0006, which indexes the resolved side.
CREATE INDEX idx_shadow_blocks_unresolved
  ON shadow_blocks(created_at) WHERE canonical_hash IS NULL;

-- `number` is unique, so `updated_at` ties break on it alone.
--
-- The cursor row itself is kept. Its `(last_updated_at, last_number)` prefix
-- still orders against surviving rows, and every row written after this
-- migration carries a later `updated_at`, so nothing is skipped. Clearing it
-- would send the reader through `max_cursor`, which by then reports the live
-- table tip and drops everything written before shadow-metrics is rolled.
-- Cost of keeping it: a row tied on `(updated_at, number)` with the watermark
-- but previously distinguished by `last_hash` is not re-read. At most one row,
-- once.
ALTER TABLE shadow_metrics_cursor DROP COLUMN last_hash;
