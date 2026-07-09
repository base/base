use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use base_protocol::{BlockInfo, L2BlockInfo};

/// Helper to create a test `L2BlockInfo` at a specific block number
pub fn test_block_info(number: u64) -> L2BlockInfo {
    L2BlockInfo {
        block_info: BlockInfo {
            number,
            hash: B256::with_last_byte(number as u8),
            parent_hash: B256::with_last_byte(number.saturating_sub(1) as u8),
            timestamp: number * 2,
        },
        l1_origin: BlockNumHash::default(),
        seq_num: 0,
    }
}
