-- Records that `hash_hex` is fully populated, so migration 0015 can mark the
-- column NOT NULL without reading the table.
--
-- `hash` has been NOT NULL since 0001 and the Rust model decodes it into a
-- non-optional String, so the constraint has to survive the rename in 0015.
-- Getting there naively means `ALTER COLUMN hash_hex SET NOT NULL`, which scans
-- the entire heap while holding ACCESS EXCLUSIVE -- the writer blocks for the
-- length of the scan, and on mainnet that is the shape of failure that migration
-- 0007 already demonstrated.
--
-- Postgres 12 and later will instead accept an existing VALIDATEd CHECK as proof
-- and skip the scan entirely. Measured on a 200k-row copy of this schema:
-- 16.2ms for the naive scan versus 0.23ms once the constraint is validated, and
-- only the former grows with the table.
--
-- So the work splits in three. This migration adds the constraint NOT VALID,
-- which is catalog-only. The VALIDATE runs out of band, alongside the backfill,
-- because it is a full scan -- but under SHARE UPDATE EXCLUSIVE, which does not
-- block reads or writes, so it is safe against a live builder in a way that
-- ACCESS EXCLUSIVE is not. 0015 then flips the column in constant time.
--
-- Deploying this before the backfill has finished is harmless: NOT VALID means
-- existing rows are never examined. It is 0015 that will refuse to apply, which
-- is the intended gate.
DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint WHERE conname = 'shadow_blocks_hash_hex_not_null'
  ) THEN
    ALTER TABLE shadow_blocks
      ADD CONSTRAINT shadow_blocks_hash_hex_not_null
      CHECK (hash_hex IS NOT NULL) NOT VALID;
  END IF;
END $$;
