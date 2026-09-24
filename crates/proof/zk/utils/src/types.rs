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

impl AggregationInputs {
    /// Validate and concatenate the intermediate roots committed by each range.
    ///
    /// Every range must end on the same checkpoint cadence. Otherwise concatenating roots
    /// could assign them to different block heights than the on-chain verifier expects.
    pub fn validated_intermediate_roots(&self) -> Result<Bytes, &'static str> {
        let first = self.boot_infos.first().ok_or("aggregation requires at least one range")?;
        let interval = first.intermediateBlockInterval;
        if interval == 0 {
            return Err("intermediate block interval must be nonzero");
        }
        if self.boot_infos.iter().any(|boot| boot.intermediateBlockInterval != interval) {
            return Err("intermediate block intervals must match");
        }

        for boot in &self.boot_infos {
            let span = boot
                .l2BlockNumber
                .checked_sub(boot.l2PreBlockNumber)
                .ok_or("range end precedes range start")?;
            if span % interval != 0 {
                return Err("range span must be a multiple of the intermediate block interval");
            }
            let expected_len =
                (span / interval).checked_mul(32).ok_or("intermediate root count overflow")?;
            if u64::try_from(boot.intermediateRoots.len()).ok() != Some(expected_len) {
                return Err("intermediate root count does not match range span");
            }
        }

        Ok(self
            .boot_infos
            .iter()
            .flat_map(|boot| boot.intermediateRoots.iter().copied())
            .collect::<Vec<u8>>()
            .into())
    }
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
        bytes32 scheduleId;
    }
}

impl AggregationOutputs {
    /// Decode from `abi.encodePacked` bytes (the inverse of `abi_encode_packed`).
    ///
    /// Layout (packed):
    ///   address  (20) | bytes32 (32) | bytes32 (32) | uint64 (8) |
    ///   bytes32  (32) | uint64  (8)  | bytes (var)  | bytes32 (32) | bytes32 (32)
    ///   | bytes32 (32)
    ///
    /// Fixed prefix = 132 bytes, fixed suffix = 96 bytes.
    pub fn decode_packed(data: &[u8]) -> Result<Self, &'static str> {
        const PREFIX: usize = 20 + 32 + 32 + 8 + 32 + 8; // 132
        const SUFFIX: usize = 32 + 32 + 32; // 96
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
        off += 32;

        let schedule_id = B256::from_slice(&data[off..off + 32]);

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
            scheduleId: schedule_id,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn boot(start: u64, end: u64, interval: u64, roots: &[u8]) -> BootInfoStruct {
        BootInfoStruct {
            l1Head: B256::ZERO,
            l2PreRoot: B256::ZERO,
            l2PostRoot: B256::ZERO,
            l2PreBlockNumber: start,
            l2BlockNumber: end,
            rollupConfigHash: B256::ZERO,
            scheduleId: B256::ZERO,
            intermediateBlockInterval: interval,
            intermediateRoots: Bytes::copy_from_slice(roots),
        }
    }

    fn inputs(boot_infos: Vec<BootInfoStruct>) -> AggregationInputs {
        AggregationInputs {
            boot_infos,
            latest_l1_checkpoint_head: B256::ZERO,
            multi_block_vkey: [0; 8],
            prover_address: Address::ZERO,
        }
    }

    #[test]
    fn validates_300_block_ranges_and_preserves_root_order() {
        let first = [1; 32];
        let second = [2; 32];
        let roots = inputs(vec![boot(0, 300, 300, &first), boot(300, 600, 300, &second)])
            .validated_intermediate_roots()
            .unwrap();

        assert_eq!(roots, [&first[..], &second[..]].concat());
    }

    #[test]
    fn validates_30_block_ranges() {
        assert!(inputs(vec![boot(0, 60, 30, &[3; 64])]).validated_intermediate_roots().is_ok());
    }

    #[test]
    fn rejects_mixed_intervals_even_when_total_root_count_matches() {
        // Both ranges have valid root counts. Together they span 600 blocks with two roots,
        // but the first root is not at the 300-block checkpoint a verifier would expect.
        assert!(
            inputs(vec![boot(37, 187, 150, &[1; 32]), boot(187, 637, 450, &[2; 32])])
                .validated_intermediate_roots()
                .is_err()
        );
    }

    #[test]
    fn rejects_truncated_and_fractional_ranges() {
        assert!(inputs(vec![boot(0, 300, 300, &[])]).validated_intermediate_roots().is_err());
        assert!(inputs(vec![boot(0, 300, 300, &[1; 31])]).validated_intermediate_roots().is_err());
        assert!(inputs(vec![boot(0, 301, 300, &[1; 32])]).validated_intermediate_roots().is_err());
    }

    #[test]
    fn rejects_zero_interval() {
        assert!(inputs(vec![boot(0, 0, 0, &[])]).validated_intermediate_roots().is_err());
    }
}
