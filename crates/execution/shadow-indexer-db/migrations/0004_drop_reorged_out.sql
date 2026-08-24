-- The flag now only separates canonical rows left behind by builds that predate
-- #4624. Dropping it makes those indistinguishable from reorged rows, and the
-- reader's cursor never advanced over them, so they would surface as shadow
-- blocks. Abort rather than let that happen silently.
SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '30s';

LOCK TABLE shadow_blocks IN ACCESS EXCLUSIVE MODE;

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM shadow_blocks WHERE NOT reorged_out) THEN
    RAISE EXCEPTION
      'cannot drop shadow_blocks.reorged_out: legacy canonical rows remain';
  END IF;
END
$$;

ALTER TABLE shadow_blocks DROP COLUMN reorged_out;
