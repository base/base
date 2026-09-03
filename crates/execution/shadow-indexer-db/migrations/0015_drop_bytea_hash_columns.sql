-- Completes the BYTEA -> TEXT move: drops the byte columns and renames the hex
-- mirrors onto their names, so `shadow_blocks.hash` and
-- `shadow_blocks.canonical_hash` are the text the ETL can actually read.
--
-- PRECONDITION: the backfill has finished and
-- `shadow_blocks_hash_hex_not_null` has been VALIDATEd out of band. Without
-- that, the SET NOT NULL below scans the heap under ACCESS EXCLUSIVE instead of
-- reading the constraint, and fails outright if any row still holds NULL. That
-- failure is deliberate -- it is the gate that stops this landing on a database
-- that has not been prepared -- but it panics the builder at startup, so run the
-- backfill first.
--
-- ORDERING: deploy the builder before shadow-metrics. The builder runs
-- migrations at startup and then reads the new names; shadow-metrics runs no
-- migrations and reads whatever is there. Between the two deploys its by-hash
-- endpoints will error. That window is unavoidable -- no rename preserves both
-- names at once -- so keep it short and roll them together.
--
-- Every statement here is catalog-only. Nothing scans, nothing rewrites.

SET LOCAL lock_timeout = '5s';

-- Constant time: proven from the validated constraint rather than the heap.
ALTER TABLE shadow_blocks ALTER COLUMN hash_hex SET NOT NULL;

-- Dropping these takes three indexes with them, two of them by predicate rather
-- than by key: idx_shadow_blocks_hash, idx_shadow_blocks_canonical_hash, and
-- idx_shadow_blocks_unresolved. 0011-0013 already rebuilt all three against the
-- hex columns, so the queries behind them stay index-backed across the cutover.
--
-- DROP COLUMN is catalog-only and returns no space to the volume; the dead bytes
-- are reclaimed as rows are rewritten or expire out of retention. It relieves
-- future rows, not today's disk usage.
ALTER TABLE shadow_blocks DROP COLUMN hash;
ALTER TABLE shadow_blocks DROP COLUMN canonical_hash;

ALTER TABLE shadow_blocks RENAME COLUMN hash_hex TO hash;
ALTER TABLE shadow_blocks RENAME COLUMN canonical_hash_hex TO canonical_hash;

ALTER INDEX idx_shadow_blocks_hash_hex RENAME TO idx_shadow_blocks_hash;
ALTER INDEX idx_shadow_blocks_canonical_hash_hex RENAME TO idx_shadow_blocks_canonical_hash;
ALTER INDEX idx_shadow_blocks_unresolved_hex RENAME TO idx_shadow_blocks_unresolved;

ALTER TABLE shadow_blocks
  RENAME CONSTRAINT shadow_blocks_hash_hex_format TO shadow_blocks_hash_format;
ALTER TABLE shadow_blocks
  RENAME CONSTRAINT shadow_blocks_canonical_hash_hex_format TO shadow_blocks_canonical_hash_format;

-- The column's own NOT NULL now carries this guarantee, and keeping the CHECK
-- would cost an extra expression evaluation on every insert.
ALTER TABLE shadow_blocks DROP CONSTRAINT shadow_blocks_hash_hex_not_null;
