-- Drops the unused transaction-hash GIN index added in migration 0004. The
-- `@> to_jsonb($hash)` explorer lookup it was built for was never wired up:
-- pg_stat_user_indexes reports idx_scan = 0 and no query in the tree references
-- it. At ~4.5 GB it was the costliest index to maintain on the batch-insert
-- path (one GIN entry per transaction per block, plus fastupdate pending-list
-- flushes that stall inserts).
--
-- Plain DROP INDEX (not CONCURRENTLY): sqlx wraps each migration in a
-- transaction. This runs at shadow-indexer startup; the brief ACCESS EXCLUSIVE
-- mirrors the index drops already performed in migration 0007.
SET LOCAL lock_timeout = '5s';

DROP INDEX IF EXISTS idx_shadow_blocks_tx_hashes;
