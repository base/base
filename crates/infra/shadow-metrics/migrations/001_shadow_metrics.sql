CREATE TABLE IF NOT EXISTS shadow_blocks(
  number BIGINT NOT NULL,
  hash TEXT NOT NULL,
  parent_hash TEXT NOT NULL,
  timestamp BIGINT NOT NULL,
  tx_count INT NOT NULL,
  gas_used BIGINT NOT NULL,
  da_bytes BIGINT NOT NULL,
  state_root TEXT NOT NULL,
  reorged_out BOOL NOT NULL DEFAULT false,
  canonical_hash TEXT,
  builder_version TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY(number, hash)
);
CREATE INDEX IF NOT EXISTS idx_shadow_blocks_number ON shadow_blocks(number DESC);

CREATE TABLE IF NOT EXISTS shadow_block_transactions(
  block_number BIGINT NOT NULL,
  block_hash TEXT NOT NULL,
  tx_index INT NOT NULL,
  tx_hash TEXT NOT NULL,
  sender TEXT,
  tx_type SMALLINT NOT NULL,
  effective_priority_fee_per_gas TEXT,
  base_fee_per_gas BIGINT,
  gas_used BIGINT NOT NULL,
  reorged_out BOOL NOT NULL DEFAULT false,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY(block_hash, tx_index)
);
CREATE INDEX IF NOT EXISTS idx_shadow_block_transactions_number ON shadow_block_transactions(block_number, tx_index);
