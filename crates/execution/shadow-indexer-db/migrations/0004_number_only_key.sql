-- Collapses the (number, hash) primary key to number-only.
--
-- Invariant after this migration: every row remaining in `shadow_blocks` is a
-- reorged-out shadow block. Canonical (non-reorged) rows are never retained
-- under a number-only key, because a canonical row and its shadow sibling
-- would otherwise collide on `number` with no way to pick a winner that
-- preserves both.
--
-- Rows discarded by the dedupe below (older duplicates for a given `number`,
-- and all canonical-only rows) are unconsumed-metric losses accepted at plan
-- time: any metric that had not yet been read off those rows before this
-- migration ran is dropped, not carried forward.
--
-- Deploy order: shadow-indexer first (it applies this migration at startup
-- via `ShadowDbConfig::init_pool`), shadow-metrics second. shadow-metrics
-- must not run against the old schema after this migration is applied.

CREATE TABLE shadow_blocks_new(
  number         BIGINT      NOT NULL PRIMARY KEY,
  hash           BYTEA       NOT NULL,
  canonical_hash BYTEA,
  created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
  payload        JSONB       NOT NULL
);

INSERT INTO shadow_blocks_new
  (number, hash, canonical_hash, created_at, updated_at, payload)
SELECT DISTINCT ON (number)
  number, hash, canonical_hash, created_at, updated_at, payload
FROM shadow_blocks
WHERE reorged_out = true
ORDER BY number, updated_at DESC, hash DESC;

-- Composite, and in exactly the order the metrics cursor scans, so the row-value
-- comparison `(updated_at, number) > ($1, $2)` in `list_since` is index-backed.
CREATE INDEX idx_shadow_blocks_updated_at_new
  ON shadow_blocks_new(updated_at, number);

ALTER INDEX shadow_blocks_pkey             RENAME TO shadow_blocks_legacy_pkey;
ALTER INDEX idx_shadow_blocks_updated_at   RENAME TO idx_shadow_blocks_updated_at_legacy;
ALTER INDEX idx_shadow_blocks_number       RENAME TO idx_shadow_blocks_number_legacy;
ALTER TABLE shadow_blocks                  RENAME TO shadow_blocks_legacy;

ALTER TABLE shadow_blocks_new              RENAME TO shadow_blocks;
ALTER INDEX shadow_blocks_new_pkey         RENAME TO shadow_blocks_pkey;
ALTER INDEX idx_shadow_blocks_updated_at_new RENAME TO idx_shadow_blocks_updated_at;

ALTER TABLE shadow_metrics_cursor DROP COLUMN last_hash;
