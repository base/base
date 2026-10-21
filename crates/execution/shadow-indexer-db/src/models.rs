use base_common_consensus::{BaseBlock, BaseReceipt};
use chrono::{DateTime, Utc};
use reth_primitives_traits::RecoveredBlock;
use serde::{Deserialize, Serialize};

/// Persisted shadow block row.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ShadowBlockRow {
    /// Block number.
    pub number: i64,
    /// Block hash, as `0x`-prefixed lowercase hex. See [`crate::ShadowHash`].
    pub hash: String,
    /// Replacement block hash at this height, absent until that block is canonical.
    pub canonical_hash: Option<String>,
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

/// Canonical block at a height, used to resolve rows the chain discarded there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowCanonicalRef {
    /// Block number.
    pub number: i64,
    /// Block hash, as `0x`-prefixed lowercase hex. See [`crate::ShadowHash`].
    pub hash: String,
}

/// A unit of work applied to `shadow_blocks`, carried in the order the `ExEx` produced it.
///
/// Ordering is load-bearing: a canonical ref resolves whichever candidate is stored at its height
/// when it is applied, so reordering it past a later candidate pins the wrong replacement hash.
#[derive(Clone, Debug)]
pub enum ShadowWrite {
    /// A reorged-out or reverted block to persist. Boxed to keep the enum, and the channel slot it
    /// travels in, small: a row carries a full block and its receipts.
    Reorged(Box<ShadowBlockRow>),
    /// A canonical block that resolves rows stored at its height.
    Canonical(ShadowCanonicalRef),
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
