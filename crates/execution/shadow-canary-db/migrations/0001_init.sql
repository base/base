CREATE TABLE shadow_blocks(
  number BIGINT NOT NULL,
  hash TEXT NOT NULL,
  parent_hash TEXT NOT NULL,
  timestamp BIGINT NOT NULL,
  tx_count INT NOT NULL,
  gas_used BIGINT NOT NULL,
  da_bytes BIGINT NOT NULL,
  state_root TEXT NOT NULL,
  build_latency_ms BIGINT,
  deadline_miss BOOL NOT NULL,
  fb_count INT,
  panicked BOOL NOT NULL,
  builder_version TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY(number, hash)
);
CREATE INDEX idx_shadow_blocks_number ON shadow_blocks(number DESC);
