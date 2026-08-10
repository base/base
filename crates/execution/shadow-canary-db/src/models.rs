use chrono::{DateTime, Utc};

/// Row representation for a shadow canary block.
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
    /// Data availability bytes.
    pub da_bytes: i64,
    /// State root hash.
    pub state_root: String,
    /// Build latency in milliseconds.
    pub build_latency_ms: Option<i64>,
    /// Whether the block missed its deadline.
    pub deadline_miss: bool,
    /// Fallback block count.
    pub fb_count: Option<i32>,
    /// Whether the builder panicked.
    pub panicked: bool,
    /// Whether the block was reorged out.
    pub reorged_out: bool,
    /// Canonical block hash at the same height after reorg.
    pub canonical_hash: Option<String>,
    /// Builder version string.
    pub builder_version: String,
    /// Row creation time.
    pub created_at: DateTime<Utc>,
}
