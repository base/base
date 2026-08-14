use base_common_consensus::{BaseBlock, BaseReceipt};
use chrono::{DateTime, Utc};
use reth_primitives_traits::RecoveredBlock;
use serde::{Deserialize, Serialize};

use crate::ShadowBlockCursor;

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
    /// Block hash, stored as its raw 32 bytes.
    pub hash: Vec<u8>,
    /// Whether the block was reorged out.
    pub reorged_out: bool,
    /// Canonical block hash at the same height after reorg, stored as its raw 32 bytes.
    pub canonical_hash: Option<Vec<u8>>,
    /// Row creation time.
    pub created_at: DateTime<Utc>,
    /// Last write time, maintained entirely by the database: `DEFAULT now()` on first
    /// insert and refreshed to `now()` by the conflict clause of `insert_batch`. Values
    /// set on the write path are ignored.
    pub updated_at: DateTime<Utc>,
    /// Full executed block and receipts persisted as JSONB.
    ///
    /// This column decodes into [`ShadowBlockPayload`] during sqlx row decoding, so a
    /// payload that does not match the struct fails the entire `fetch_all`, not just
    /// the offending row. Payload compatibility is therefore a hard requirement:
    /// `block` is `RecoveredBlock<BaseBlock>` and `receipts` is `Vec<BaseReceipt>`,
    /// both upstream reth/alloy types whose serde representation can change under a
    /// dependency bump. A mismatch stalls the metrics reader until the offending row
    /// is repaired or deleted. This is an accepted operational risk.
    #[sqlx(json)]
    pub payload: ShadowBlockPayload,
}

impl ShadowBlockRow {
    /// Position of this row in the update stream.
    #[must_use]
    pub fn cursor(&self) -> ShadowBlockCursor {
        ShadowBlockCursor {
            updated_at: self.updated_at,
            number: self.number,
            hash: self.hash.clone(),
        }
    }
}

/// Block payload persisted as JSONB.
///
/// The recovered block and its receipts are stored as the node's own consensus types, so
/// downstream consumers read fully typed values while the underlying column stays a single
/// JSONB blob that evolves with the types without a schema migration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShadowBlockPayload {
    /// Builder version string, injected by the writer before persistence.
    pub builder_version: String,
    /// Recovered block: sealed header, body, and recovered senders.
    pub block: RecoveredBlock<BaseBlock>,
    /// Execution receipts for the block, in transaction order.
    pub receipts: Vec<BaseReceipt>,
}
