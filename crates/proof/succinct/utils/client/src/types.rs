use alloy_primitives::{Address, B256, Bytes};
use alloy_sol_types::sol;
use serde::{Deserialize, Serialize};

use crate::boot::BootInfoStruct;

/// Inputs to the aggregation program.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregationInputs {
    /// Per-range boot info structs.
    pub boot_infos: Vec<BootInfoStruct>,
    /// L1 block hash anchoring all ranges.
    pub latest_l1_checkpoint_head: B256,
    /// Verification key for the range program.
    pub multi_block_vkey: [u32; 8],
    /// On-chain address of the prover.
    pub prover_address: Address,
}

sol! {
    #[derive(Debug, Serialize, Deserialize)]
    struct AggregationOutputs {
        address proverAddress;
        bytes32 l1Head;
        bytes32 l2PreRoot;
        uint64 startingL2SequenceNumber;
        bytes32 l2PostRoot;
        uint64 endingL2SequenceNumber;
        bytes intermediateRoots;
        bytes32 rollupConfigHash;
        bytes32 imageHash;
    }
}

impl AggregationOutputs {
    /// Decode from `abi.encodePacked` bytes (the inverse of `abi_encode_packed`).
    ///
    /// Layout (packed):
    ///   address  (20) | bytes32 (32) | bytes32 (32) | uint64 (8) |
    ///   bytes32  (32) | uint64  (8)  | bytes (var)  | bytes32 (32) | bytes32 (32)
    ///
    /// Fixed prefix = 132 bytes, fixed suffix = 64 bytes.
    pub fn decode_packed(data: &[u8]) -> Result<Self, &'static str> {
        const PREFIX: usize = 20 + 32 + 32 + 8 + 32 + 8; // 132
        const SUFFIX: usize = 32 + 32; // 64
        if data.len() < PREFIX + SUFFIX {
            return Err("data too short for packed AggregationOutputs");
        }

        let mut off = 0;

        let prover_address = Address::from_slice(&data[off..off + 20]);
        off += 20;

        let l1_head = B256::from_slice(&data[off..off + 32]);
        off += 32;

        let l2_pre_root = B256::from_slice(&data[off..off + 32]);
        off += 32;

        let starting_seq = u64::from_be_bytes(
            data[off..off + 8].try_into().map_err(|_| "bad slice for starting_seq")?,
        );
        off += 8;

        let l2_post_root = B256::from_slice(&data[off..off + 32]);
        off += 32;

        let ending_seq = u64::from_be_bytes(
            data[off..off + 8].try_into().map_err(|_| "bad slice for ending_seq")?,
        );
        off += 8;

        let roots_len = data.len() - PREFIX - SUFFIX;
        let intermediate_roots = Bytes::copy_from_slice(&data[off..off + roots_len]);
        off += roots_len;

        let rollup_config_hash = B256::from_slice(&data[off..off + 32]);
        off += 32;

        let image_hash = B256::from_slice(&data[off..off + 32]);

        Ok(Self {
            proverAddress: prover_address,
            l1Head: l1_head,
            l2PreRoot: l2_pre_root,
            startingL2SequenceNumber: starting_seq,
            l2PostRoot: l2_post_root,
            endingL2SequenceNumber: ending_seq,
            intermediateRoots: intermediate_roots,
            rollupConfigHash: rollup_config_hash,
            imageHash: image_hash,
        })
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
