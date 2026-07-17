CREATE TABLE shadow_blocks(
  number BIGINT,
  hash TEXT,
  parent_hash TEXT,
  timestamp BIGINT,
  tx_count INT,
  gas_used BIGINT,
  da_bytes BIGINT,
  state_root TEXT,
  build_latency_ms BIGINT,
  deadline_miss BOOL,
  fb_count INT,
  panicked BOOL,
  builder_version TEXT,
  created_at TIMESTAMPTZ DEFAULT now(),
  PRIMARY KEY(number, hash)
);
CREATE INDEX idx_shadow_blocks_number ON shadow_blocks(number DESC);
