CREATE TABLE shadow_block_transactions(
  block_number BIGINT NOT NULL,
  block_hash TEXT NOT NULL,
  tx_index INT NOT NULL,
  tx_hash TEXT NOT NULL,
  sender TEXT,
  tx_type SMALLINT NOT NULL,
  effective_priority_fee_per_gas TEXT,
  base_fee_per_gas BIGINT,
  gas_used BIGINT NOT NULL,
  reorged_out BOOL NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY(block_hash, tx_index)
);
CREATE INDEX idx_shadow_block_transactions_number ON shadow_block_transactions(block_number, tx_index);
