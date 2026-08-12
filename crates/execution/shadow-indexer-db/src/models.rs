use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Row representation for a shadow indexer block.
///
/// Only the columns required for identity, reorg bookkeeping, and range scans are
/// materialized as real columns. All descriptive block and transaction metadata
/// lives in a single JSONB `payload`, so downstream consumers can evolve the shape
/// they read without a database migration.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ShadowBlockRow {
    /// Block number.
    pub number: i64,
    /// Block hash.
    pub hash: String,
    /// Whether the block was reorged out.
    pub reorged_out: bool,
    /// Canonical block hash at the same height after reorg.
    pub canonical_hash: Option<String>,
    /// Row creation time.
    pub created_at: DateTime<Utc>,
    /// Flexible block and transaction metadata persisted as JSONB.
    #[sqlx(json)]
    pub payload: ShadowBlockPayload,
}

/// Flexible block-level metadata persisted in the block payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShadowBlockPayload {
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
    /// Builder version string.
    pub builder_version: String,
    /// Per-transaction metadata for the block.
    pub transactions: Vec<ShadowTransaction>,
}

/// Flexible per-transaction metadata persisted within a block payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShadowTransaction {
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
}
