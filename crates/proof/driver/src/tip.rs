//! Contains the tip cursor for the derivation driver.
//!
//! This module provides the [`TipCursor`] which encapsulates the L2 safe head state
//! including block information, header, and output root for a specific derivation tip.

use alloy_consensus::{Header, Sealed};
use alloy_primitives::B256;
use base_protocol::L2BlockInfo;

/// A cursor that encapsulates the L2 safe head state at a specific derivation tip.
///
/// The [`TipCursor`] represents a snapshot of the L2 chain state at a particular point
/// in the derivation process. It contains all the essential information needed to
/// represent an L2 safe head including the block metadata, sealed header, and output root.
#[derive(Debug, Clone)]
pub struct TipCursor {
    /// The L2 block information for the safe head.
    pub l2_safe_head: L2BlockInfo,
    /// The sealed header of the L2 safe head block.
    pub l2_safe_head_header: Sealed<Header>,
    /// The output root computed for the L2 safe head state.
    pub l2_safe_head_output_root: B256,
}

impl TipCursor {
    /// Creates a new tip cursor with the specified L2 safe head components.
    pub const fn new(
        l2_safe_head: L2BlockInfo,
        l2_safe_head_header: Sealed<Header>,
        l2_safe_head_output_root: B256,
    ) -> Self {
        Self { l2_safe_head, l2_safe_head_header, l2_safe_head_output_root }
    }

    /// Returns a reference to the L2 safe head block information.
    pub const fn l2_safe_head(&self) -> &L2BlockInfo {
        &self.l2_safe_head
    }

    /// Returns a reference to the sealed header of the L2 safe head.
    pub const fn l2_safe_head_header(&self) -> &Sealed<Header> {
        &self.l2_safe_head_header
    }

    /// Returns a reference to the output root of the L2 safe head.
    pub const fn l2_safe_head_output_root(&self) -> &B256 {
        &self.l2_safe_head_output_root
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Header, Sealable};
    use alloy_eips::BlockNumHash;
    use alloy_primitives::B256;
    use base_protocol::{BlockInfo, L2BlockInfo};

    use super::*;

    fn make_tip(number: u64) -> TipCursor {
        let block_info = BlockInfo {
            hash: B256::repeat_byte(number as u8),
            number,
            parent_hash: B256::ZERO,
            timestamp: number * 2,
        };
        let l2_info = L2BlockInfo {
            block_info,
            l1_origin: BlockNumHash { number, hash: B256::ZERO },
            seq_num: 0,
        };
        let header = Header { number, ..Default::default() }.seal_slow();
        let output_root = B256::repeat_byte(number as u8);
        TipCursor::new(l2_info, header, output_root)
    }

    #[test]
    fn accessors_return_correct_values() {
        let tip = make_tip(42);

        assert_eq!(tip.l2_safe_head().block_info.number, 42);
        assert_eq!(tip.l2_safe_head_header().number, 42);
        assert_eq!(*tip.l2_safe_head_output_root(), B256::repeat_byte(42));
    }

    #[test]
    fn clone_produces_independent_copy() {
        let tip = make_tip(5);
        let cloned = tip.clone();

        assert_eq!(
            tip.l2_safe_head().block_info.number,
            cloned.l2_safe_head().block_info.number
        );
        assert_eq!(tip.l2_safe_head_output_root(), cloned.l2_safe_head_output_root());
    }
}
