-- Adds hex-string mirrors of the two BYTEA hash columns.
--
-- The Snowflake ETL reads this table with psycopg, which hands back a
-- `memoryview` for a BYTEA column. Nothing in the pipeline unwraps it, so what
-- lands in the warehouse is the repr of the Python object -- `<memory at
-- 0x74b086814b80>` -- and not a block hash. The hashes are the join key for
-- every downstream shadow-block query, so they have to arrive as text.
--
-- `hash_hex` and `canonical_hash_hex` carry the same values as their BYTEA
-- counterparts, formatted as `0x`-prefixed lowercase hex. That is already what
-- the shadow-metrics JSON API emits and what every other EVM hash in Snowflake
-- looks like, so the values join without a normalization step.
--
-- Converting the existing columns in place was considered and rejected. A
-- `BYTEA -> TEXT` change is not binary-coercible, so `ALTER COLUMN ... TYPE`
-- rewrites the whole heap and every index while holding ACCESS EXCLUSIVE. On a
-- 50k-row copy of this schema that rewrite took 580ms and changed the
-- relfilenode; on mainnet, where migration 0007's single DELETE ran for over an
-- hour and a table copy exhausted the volume, it is an outage and a disk-space
-- failure at once.
--
-- Adding nullable columns with no default is a catalog-only change instead: no
-- rewrite, no scan, ACCESS EXCLUSIVE held for microseconds.
--
-- Rows written from here on carry both representations, because the writer
-- populates the hex columns in the same statement that binds the bytes. Rows
-- already stored hold NULL until the out-of-band backfill runs. Nothing reads
-- these columns yet, so a half-finished backfill is not observable.

ALTER TABLE shadow_blocks
  ADD COLUMN IF NOT EXISTS hash_hex TEXT,
  ADD COLUMN IF NOT EXISTS canonical_hash_hex TEXT;

-- The writer and the reader agree on a hash by string equality, so a stray
-- uppercase digit or a missing `0x` is a silent lookup miss rather than an
-- error. These constraints turn that into a write failure.
--
-- NOT VALID records the constraint for new and updated rows without scanning
-- the table, which keeps this migration catalog-only. The backfill step
-- validates them afterwards, out of band.
--
-- Guarded on the catalog rather than stated bare: this table has already been
-- migrated by hand on mainnet with the `_sqlx_migrations` checksum written
-- after the fact, and a re-run must not fail on a constraint that already
-- exists.
DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint WHERE conname = 'shadow_blocks_hash_hex_format'
  ) THEN
    ALTER TABLE shadow_blocks
      ADD CONSTRAINT shadow_blocks_hash_hex_format
      CHECK (hash_hex IS NULL OR hash_hex ~ '^0x[0-9a-f]{64}$') NOT VALID;
  END IF;

  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint WHERE conname = 'shadow_blocks_canonical_hash_hex_format'
  ) THEN
    ALTER TABLE shadow_blocks
      ADD CONSTRAINT shadow_blocks_canonical_hash_hex_format
      CHECK (canonical_hash_hex IS NULL OR canonical_hash_hex ~ '^0x[0-9a-f]{64}$') NOT VALID;
  END IF;
END $$;
