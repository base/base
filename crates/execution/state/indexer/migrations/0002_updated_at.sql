ALTER TABLE shadow_blocks
  ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
-- Composite, and in exactly the order the metrics cursor scans, so the row-value
-- comparison in `list_reorged_since` is index-backed.
CREATE INDEX IF NOT EXISTS idx_shadow_blocks_updated_at
  ON shadow_blocks(updated_at, number, hash);
