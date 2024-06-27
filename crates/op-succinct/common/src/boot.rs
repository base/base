//! This module contains the prologue phase of the client program, pulling in the boot
//! information, which is passed to the zkVM a public inputs to be verified on chain.

use::kona_client::BootInfo;
use::kona_primitives::RollupConfig;
use alloy_primitives::B256;
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootInfoWithoutRollupConfig {
    pub l1_head: B256,
    pub l2_output_root: B256,
    pub l2_claim: B256,
    pub l2_claim_block: u64,
    pub chain_id: u64,
}

impl From<BootInfoWithoutRollupConfig> for BootInfo {
    fn from(boot_info_without_rollup_config: BootInfoWithoutRollupConfig) -> Self {
        let BootInfoWithoutRollupConfig { l1_head, l2_output_root, l2_claim, l2_claim_block, chain_id } = boot_info_without_rollup_config;
        let rollup_config = RollupConfig::from_l2_chain_id(chain_id).unwrap();

        Self { l1_head, l2_output_root, l2_claim, l2_claim_block, chain_id, rollup_config }
    }
}
