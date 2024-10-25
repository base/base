use alloy_primitives::B256;
use alloy_sol_types::sol;
use serde::{Deserialize, Serialize};

use crate::boot::BootInfoStruct;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregationInputs {
    pub boot_infos: Vec<BootInfoStruct>,
    pub latest_l1_checkpoint_head: B256,
    pub multi_block_vkey: [u32; 8],
}

sol! {
    #[derive(Debug, Serialize, Deserialize)]
    struct AggregationOutputs {
        bytes32 l1Head;
        bytes32 l2PreRoot;
        bytes32 l2PostRoot;
        uint64 l2BlockNumber;
        bytes32 rollupConfigHash;
        bytes32 multiBlockVKey;
    }
}

/// Convert a u32 array to a u8 array. Useful for converting the range vkey to a B256.
pub fn u32_to_u8(input: [u32; 8]) -> [u8; 32] {
    let mut output = [0u8; 32];
    for (i, &value) in input.iter().enumerate() {
        let bytes = value.to_be_bytes();
        output[i * 4..(i + 1) * 4].copy_from_slice(&bytes);
    }
    output
}
