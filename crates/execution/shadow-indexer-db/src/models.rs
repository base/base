use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Row representation for a shadow indexer block.
///
/// Only the columns required for identity, reorg bookkeeping, and range scans are
/// materialized as real columns. The full executed block and its receipts live in a
/// single JSONB `payload`, so downstream consumers can evolve the shape they read
/// without a database migration.
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
    /// Full executed block and receipts persisted as JSONB.
    #[sqlx(json)]
    pub payload: ShadowBlockPayload,
}

/// Block payload persisted as JSONB.
///
/// `block` and `receipts` are captured verbatim from the node's consensus types as opaque
/// JSON, so fields the node already produces become available downstream without a schema
/// migration or a bespoke per-field type in this crate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShadowBlockPayload {
    /// Builder version string, injected by the writer before persistence.
    pub builder_version: String,
    /// Recovered block (sealed header, body, and recovered senders) as serialized by the node.
    pub block: Value,
    /// Execution receipts for the block, in transaction order.
    pub receipts: Value,
}
