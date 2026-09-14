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
--
-- The timeout is about acquiring the lock, not holding it. Each statement here
-- finishes in under a millisecond, but every one needs ACCESS EXCLUSIVE, and
-- that has to wait out any open reader -- including the Snowflake ETL, whose
-- SELECTs run long enough that they already collided with migration 0007. At
-- 5s a single unlucky overlap fails the migration, which panics the builder at
-- startup and crashloops it.
--
-- Waiting is not free either: once this is queued for ACCESS EXCLUSIVE, new
-- queries queue behind it, so the timeout is also the worst-case stall for
-- everything touching the table. Five minutes is a real stall -- roughly 150
-- blocks of shadow writes at a two-second cadence, which the writer's 1024-slot
-- channel absorbs -- accepted because this runs once, on a canary, and the
-- alternative is a builder that crashloops until someone notices.
SET LOCAL lock_timeout = '5min';

-- Raised above lock_timeout on purpose. statement_timeout covers the lock wait
-- too, so if the server default were lower it, and not lock_timeout, would
-- decide how long this waits -- silently capping the five minutes above. Every
-- statement here is catalog-only and finishes in under a millisecond, so the
-- only thing this ceiling can cut short is the wait itself.
SET LOCAL statement_timeout = '6min';

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
