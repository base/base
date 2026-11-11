use alloy_primitives::{Address, B256};
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
        bytes32 scheduleId;
    }
}
