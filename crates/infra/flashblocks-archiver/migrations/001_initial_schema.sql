-- Flashblocks archiver database schema

-- Builders table - stores information about payload builders
CREATE TABLE IF NOT EXISTS builders (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    url TEXT NOT NULL UNIQUE,
    name TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Flashblocks table - stores the full flashblock messages
CREATE TABLE IF NOT EXISTS flashblocks (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    builder_id UUID NOT NULL REFERENCES builders(id),
    payload_id TEXT NOT NULL,
    flashblock_index BIGINT NOT NULL,
    block_number BIGINT NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    raw_message JSONB NOT NULL,
    
    -- Base payload fields (when present)
    parent_beacon_block_root TEXT,
    parent_hash TEXT,
    fee_recipient TEXT,
    prev_randao TEXT,
    gas_limit BIGINT,
    base_timestamp BIGINT,
    extra_data TEXT,
    base_fee_per_gas TEXT, -- Stored as hex string for U256
    
    -- Delta/diff fields
    state_root TEXT NOT NULL,
    receipts_root TEXT NOT NULL,
    logs_bloom TEXT NOT NULL,
    gas_used BIGINT NOT NULL,
    block_hash TEXT NOT NULL,
    withdrawals_root TEXT,
    
    UNIQUE(builder_id, payload_id, flashblock_index)
);

-- Transactions table - stores individual transactions from flashblocks
CREATE TABLE IF NOT EXISTS transactions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    flashblock_id UUID NOT NULL REFERENCES flashblocks(id) ON DELETE CASCADE,
    builder_id UUID NOT NULL REFERENCES builders(id),
    payload_id TEXT NOT NULL,
    flashblock_index BIGINT NOT NULL,
    block_number BIGINT NOT NULL,
    
    -- Transaction data (raw bytes)
    tx_data BYTEA NOT NULL,
    tx_index INTEGER NOT NULL,
    
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    
    UNIQUE(flashblock_id, tx_index)
);

-- Withdrawals table - stores withdrawals from flashblocks
CREATE TABLE IF NOT EXISTS withdrawals (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    flashblock_id UUID NOT NULL REFERENCES flashblocks(id) ON DELETE CASCADE,
    builder_id UUID NOT NULL REFERENCES builders(id),
    payload_id TEXT NOT NULL,
    flashblock_index BIGINT NOT NULL,
    block_number BIGINT NOT NULL,
    
    -- Withdrawal data
    withdrawal_index BIGINT NOT NULL,
    validator_index BIGINT NOT NULL,
    address TEXT NOT NULL,
    amount BIGINT NOT NULL,
    
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    
    UNIQUE(flashblock_id, withdrawal_index)
);

-- Receipts table - stores transaction receipts from metadata
CREATE TABLE IF NOT EXISTS receipts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    flashblock_id UUID NOT NULL REFERENCES flashblocks(id) ON DELETE CASCADE,
    builder_id UUID NOT NULL REFERENCES builders(id),
    payload_id TEXT NOT NULL,
    flashblock_index BIGINT NOT NULL,
    block_number BIGINT NOT NULL,
    
    -- Receipt data from OpReceipt
    tx_hash TEXT NOT NULL,
    tx_type INTEGER NOT NULL, -- 0=Legacy, 1=EIP-2930, 2=EIP-1559, 4=EIP-7702, 126=Deposit
    status BOOLEAN NOT NULL, -- Transaction success/failure
    cumulative_gas_used BIGINT NOT NULL,
    logs JSONB NOT NULL, -- Array of log objects
    
    -- Optimism deposit-specific fields (only for Deposit type)
    deposit_nonce BIGINT,
    deposit_receipt_version BIGINT,
    
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    
    UNIQUE(flashblock_id, tx_hash)
);

-- Account balances table - stores new account balances from metadata
CREATE TABLE IF NOT EXISTS account_balances (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    flashblock_id UUID NOT NULL REFERENCES flashblocks(id) ON DELETE CASCADE,
    builder_id UUID NOT NULL REFERENCES builders(id),
    payload_id TEXT NOT NULL,
    flashblock_index BIGINT NOT NULL,
    block_number BIGINT NOT NULL,
    
    -- Account data
    address TEXT NOT NULL,
    balance TEXT NOT NULL,
    
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    
    UNIQUE(flashblock_id, address)
);

-- Indexes for better query performance
CREATE INDEX IF NOT EXISTS idx_flashblocks_builder_block ON flashblocks(builder_id, block_number);
CREATE INDEX IF NOT EXISTS idx_flashblocks_received_at ON flashblocks(received_at);
CREATE INDEX IF NOT EXISTS idx_flashblocks_payload_id ON flashblocks(payload_id);
CREATE INDEX IF NOT EXISTS idx_flashblocks_block_number ON flashblocks(block_number);

CREATE INDEX IF NOT EXISTS idx_transactions_flashblock_id ON transactions(flashblock_id);
CREATE INDEX IF NOT EXISTS idx_transactions_builder_block ON transactions(builder_id, block_number);
CREATE INDEX IF NOT EXISTS idx_transactions_payload_id ON transactions(payload_id);

CREATE INDEX IF NOT EXISTS idx_withdrawals_flashblock_id ON withdrawals(flashblock_id);
CREATE INDEX IF NOT EXISTS idx_withdrawals_builder_block ON withdrawals(builder_id, block_number);

CREATE INDEX IF NOT EXISTS idx_receipts_flashblock_id ON receipts(flashblock_id);
CREATE INDEX IF NOT EXISTS idx_receipts_tx_hash ON receipts(tx_hash);
CREATE INDEX IF NOT EXISTS idx_receipts_builder_block ON receipts(builder_id, block_number);
CREATE INDEX IF NOT EXISTS idx_receipts_tx_type ON receipts(tx_type);
CREATE INDEX IF NOT EXISTS idx_receipts_status ON receipts(status);

CREATE INDEX IF NOT EXISTS idx_account_balances_flashblock_id ON account_balances(flashblock_id);
CREATE INDEX IF NOT EXISTS idx_account_balances_builder_block ON account_balances(builder_id, block_number);
CREATE INDEX IF NOT EXISTS idx_account_balances_address ON account_balances(address);