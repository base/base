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
    use alloy_consensus::{Header, Sealed};
    use alloy_primitives::B256;
    use base_protocol::{BlockInfo, L2BlockInfo};

    use super::*;

    fn make_l2_block_info(number: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo {
                hash: B256::repeat_byte(number as u8),
                number,
                parent_hash: B256::ZERO,
                timestamp: number * 2,
            },
            ..Default::default()
        }
    }

    #[test]
    fn new_stores_all_fields() {
        let l2_head = make_l2_block_info(7);
        let header_hash = B256::repeat_byte(0xAA);
        let header = Sealed::new_unchecked(Header::default(), header_hash);
        let output_root = B256::repeat_byte(0xBB);

        let cursor = TipCursor::new(l2_head, header, output_root);

        assert_eq!(cursor.l2_safe_head, l2_head);
        assert_eq!(cursor.l2_safe_head_output_root, output_root);
        assert_eq!(*cursor.l2_safe_head_header.hash(), header_hash);
    }

    #[test]
    fn accessors_return_references_to_fields() {
        let l2_head = make_l2_block_info(3);
        let header_hash = B256::repeat_byte(0xCC);
        let header = Sealed::new_unchecked(Header::default(), header_hash);
        let output_root = B256::repeat_byte(0xDD);

        let cursor = TipCursor::new(l2_head, header, output_root);

        assert_eq!(*cursor.l2_safe_head(), l2_head);
        assert_eq!(cursor.l2_safe_head_header().hash(), &header_hash);
        assert_eq!(*cursor.l2_safe_head_output_root(), output_root);
    }
}
