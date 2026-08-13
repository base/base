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
    #[sqlx(json)]
    pub payload: ShadowBlockPayload,
}

/// Reader-side row shape. `payload` stays raw so one malformed payload cannot fail
/// the whole fetch.
///
/// `ShadowBlockRow` decodes `payload` into `ShadowBlockPayload` during sqlx row
/// decoding (`#[sqlx(json)]`). A payload whose JSON does not match the struct — e.g.
/// after a type change on either side — aborts the entire `fetch_all`, so the reader
/// would never see the offending row, could not count it, and could not advance past
/// it. That wedges the cursor permanently. Reading the column as `serde_json::Value`
/// (always succeeds for valid JSONB) and deserializing per row moves the failure to
/// where it can be handled.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ShadowBlockRawRow {
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
    /// Last write time, maintained by the database.
    pub updated_at: DateTime<Utc>,
    /// Full persisted payload kept as raw JSON for per-row decoding.
    #[sqlx(json)]
    pub payload: serde_json::Value,
}

impl ShadowBlockRawRow {
    /// Position of this row in the update stream.
    #[must_use]
    pub fn cursor(&self) -> ShadowBlockCursor {
        ShadowBlockCursor {
            updated_at: self.updated_at,
            number: self.number,
            hash: self.hash.clone(),
        }
    }

    /// Deserializes the raw payload. Failure is per-row and recoverable.
    ///
    /// # Errors
    ///
    /// Returns an error if the payload does not match `ShadowBlockPayload`.
    pub fn decode_payload(&self) -> Result<ShadowBlockPayload, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
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
