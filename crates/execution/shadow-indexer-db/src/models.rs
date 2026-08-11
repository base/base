use chrono::{DateTime, Utc};

/// Row representation for a shadow indexer block.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ShadowBlockRow {
    /// Block number.
    pub number: i64,
    /// Block hash.
    pub hash: String,
    /// Parent block hash.
    pub parent_hash: String,
    /// Block timestamp.
    pub timestamp: i64,
    /// Number of transactions in the block.
    pub tx_count: i32,
    /// Total gas used.
    pub gas_used: i64,
    /// State root hash.
    pub state_root: String,
    /// Whether the block was reorged out.
    pub reorged_out: bool,
    /// Canonical block hash at the same height after reorg.
    pub canonical_hash: Option<String>,
    /// Builder version string.
    pub builder_version: String,
    /// Row creation time.
    pub created_at: DateTime<Utc>,
}

/// Row representation for a single transaction within a shadow indexer block.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ShadowBlockTransactionRow {
    /// Block number the transaction was included in.
    pub block_number: i64,
    /// Hash of the block the transaction was included in.
    pub block_hash: String,
    /// Zero-based position of the transaction within the block.
    pub tx_index: i32,
    /// Transaction hash.
    pub tx_hash: String,
    /// Recovered sender address, when signature recovery succeeds.
    pub sender: Option<String>,
    /// EIP-2718 transaction type byte (0x7e denotes an OP deposit).
    pub tx_type: i16,
    /// Effective priority fee per gas (tip) in wei, as a base-10 string to preserve full u128 range.
    pub effective_priority_fee_per_gas: Option<String>,
    /// Block base fee per gas in wei.
    pub base_fee_per_gas: Option<i64>,
    /// Gas consumed by this transaction.
    pub gas_used: i64,
    /// Whether the containing block was reorged out.
    pub reorged_out: bool,
    /// Row creation time.
    pub created_at: DateTime<Utc>,
}
