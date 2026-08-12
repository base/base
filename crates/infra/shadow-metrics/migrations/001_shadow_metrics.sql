CREATE TABLE IF NOT EXISTS shadow_blocks(
  number BIGINT NOT NULL,
  hash TEXT NOT NULL,
  reorged_out BOOL NOT NULL DEFAULT false,
  canonical_hash TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  payload JSONB NOT NULL,
  PRIMARY KEY(number, hash)
);
CREATE INDEX IF NOT EXISTS idx_shadow_blocks_number ON shadow_blocks(number DESC);
