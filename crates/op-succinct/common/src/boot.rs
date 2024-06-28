//! This module contains the prologue phase of the client program, pulling in the boot
//! information, which is passed to the zkVM a public inputs to be verified on chain.

use::kona_client::BootInfo;
use::kona_primitives::RollupConfig;
use alloy_primitives::{U256, B256};
use alloy_sol_types::{sol, SolValue};
use serde::{Serialize, Deserialize};

/// Boot information that is passed to the zkVM as public inputs.
/// This struct contains all information needed to generate BootInfo,
/// as the RollupConfig can be derived from the `chain_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootInfoWithoutRollupConfig {
    pub l1_head: B256,
    pub l2_output_root: B256,
    pub l2_claim: B256,
    pub l2_claim_block: u64,
    pub chain_id: u64,
}

impl From<BootInfoWithoutRollupConfig> for BootInfo {
    /// Convert the BootInfoWithoutRollupConfig into BootInfo by deriving the RollupConfig.
    fn from(boot_info_without_rollup_config: BootInfoWithoutRollupConfig) -> Self {
        let BootInfoWithoutRollupConfig { l1_head, l2_output_root, l2_claim, l2_claim_block, chain_id } = boot_info_without_rollup_config;
        let rollup_config = RollupConfig::from_l2_chain_id(chain_id).unwrap();

        Self { l1_head, l2_output_root, l2_claim, l2_claim_block, chain_id, rollup_config }
    }
}

impl BootInfoWithoutRollupConfig {
    /// ABI encode the boot info. This is used to commit to in the zkVM,
    /// so that we can verify on chain that the correct values were used in
    /// the proof.
    pub fn abi_encode(&self) -> Vec<u8> {
        sol! {
            struct PublicValuesStruct {
                bytes32 l1Head;
                bytes32 l2PreRoot;
                bytes32 l2PostRoot;
                uint256 l2BlockNumber;
                uint256 chainId;
            }
        }

        let public_values = PublicValuesStruct {
            l1Head: self.l1_head,
            l2PreRoot: self.l2_output_root,
            l2PostRoot: self.l2_claim,
            l2BlockNumber: U256::from(self.l2_claim_block),
            chainId: U256::from(self.chain_id),
        };

        public_values.abi_encode()
    }
}
