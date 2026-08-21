use base_common_consensus::{BaseBlock, BaseReceipt};
use chrono::{DateTime, Utc};
use reth_primitives_traits::RecoveredBlock;
use serde::{Deserialize, Serialize};

use crate::ShadowBlockCursor;

/// Persisted shadow block row.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ShadowBlockRow {
    /// Block number.
    pub number: i64,
    /// Raw block hash.
    pub hash: Vec<u8>,
    /// Whether block was reorged out.
    pub reorged_out: bool,
    /// Replacement block hash after reorg.
    pub canonical_hash: Option<Vec<u8>>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Database-maintained update time.
    pub updated_at: DateTime<Utc>,
    /// Typed JSONB payload.
    ///
    /// Incompatible serde changes fail the whole fetch and stall the reader; this risk is accepted.
    #[sqlx(json)]
    pub payload: ShadowBlockPayload,
}

impl ShadowBlockRow {
    /// Returns this row's stream position.
    #[must_use]
    pub fn cursor(&self) -> ShadowBlockCursor {
        ShadowBlockCursor {
            updated_at: self.updated_at,
            number: self.number,
            hash: self.hash.clone(),
        }
    }
}

/// Shadow block JSONB payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShadowBlockPayload {
    /// Writer-stamped builder version.
    pub builder_version: String,
    /// Recovered block.
    pub block: RecoveredBlock<BaseBlock>,
    /// Transaction-ordered receipts.
    pub receipts: Vec<BaseReceipt>,
}
