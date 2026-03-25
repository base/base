//! Traits for the safe head database.

use base_protocol::{BlockInfo, L2BlockInfo};

use crate::{SafeDBError, SafeHeadResponse};

/// Write interface called by the derivation actor on safe head changes.
pub trait SafeHeadListener: Send + Sync + std::fmt::Debug {
    /// Records that `safe_head` was derived as safe using data up to `l1_block`.
    fn safe_head_updated(
        &self,
        safe_head: L2BlockInfo,
        l1_block: BlockInfo,
    ) -> Result<(), SafeDBError>;

    /// Truncates entries to reflect a safe head reset (reorg handling).
    fn safe_head_reset(&self, reset_safe_head: L2BlockInfo) -> Result<(), SafeDBError>;
}

/// Read interface called by the RPC layer to query historical safe heads.
pub trait SafeDBReader: Send + Sync + std::fmt::Debug {
    /// Returns the safe head at or before the given L1 block number.
    fn safe_head_at_l1(&self, l1_block_num: u64) -> Result<SafeHeadResponse, SafeDBError>;
}
