//! Traits for the safe head database.

use async_trait::async_trait;
use base_protocol::{BlockInfo, L2BlockInfo};

use crate::{SafeDBError, SafeHeadResponse};

/// Write interface called by the derivation actor on safe head changes.
#[async_trait]
pub trait SafeHeadListener: Send + Sync + std::fmt::Debug {
    /// Records that `safe_head` became safe as a result of processing `l1_block`.
    ///
    /// `l1_block` is the **L1 inclusion block** — the L1 block whose data (calldata or blobs)
    /// contained the batch that produced `safe_head`. This is distinct from
    /// `safe_head.l1_origin`, which is the L1 epoch the L2 block references. The inclusion
    /// block is equal to or later than the epoch origin, and is the correct key for answering
    /// "after L1 block N was processed, what was the safe L2 head?"
    async fn safe_head_updated(
        &self,
        safe_head: L2BlockInfo,
        l1_block: BlockInfo,
    ) -> Result<(), SafeDBError>;

    /// Truncates entries to reflect a safe head reset (reorg handling).
    async fn safe_head_reset(&self, reset_safe_head: L2BlockInfo) -> Result<(), SafeDBError>;
}

/// Read interface called by the RPC layer to query historical safe heads.
#[async_trait]
pub trait SafeDBReader: Send + Sync + std::fmt::Debug {
    /// Returns the safe head at or before the given L1 block number.
    async fn safe_head_at_l1(&self, l1_block_num: u64) -> Result<SafeHeadResponse, SafeDBError>;
}
