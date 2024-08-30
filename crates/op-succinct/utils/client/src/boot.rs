//! This module contains the prologue phase of the client program, pulling in the boot
//! information, which is passed to the zkVM a public inputs to be verified on chain.

use alloy_primitives::B256;
use alloy_sol_types::{sol, SolValue};
use anyhow::Result;
use kona_client::BootInfo;
use kona_primitives::RollupConfig;
use serde::{Deserialize, Serialize};

// ABI encoding of BootInfo is 5 * 32 bytes.
pub const BOOT_INFO_SIZE: usize = 5 * 32;

/// Boot information that is committed to the zkVM as public inputs.
/// This struct contains all information needed to generate BootInfo,
/// as the RollupConfig can be derived from the `chain_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawBootInfo {
    /// From [`BootInfo::l1_head`].
    pub l1_head: B256,
    /// From [`BootInfo::l2_output_root`].
    pub l2_output_root: B256,
    /// From [`BootInfo::l2_claim`].
    pub l2_claim: B256,
    /// From [`BootInfo::l2_claim_block`].
    pub l2_claim_block: u64,
    /// From [`BootInfo::chain_id`].
    pub chain_id: u64,
}

impl From<RawBootInfo> for BootInfo {
    /// Convert the BootInfoWithoutRollupConfig into BootInfo by deriving the RollupConfig.
    fn from(boot_info_without_rollup_config: RawBootInfo) -> Self {
        let RawBootInfo { l1_head, l2_output_root, l2_claim, l2_claim_block, chain_id } =
            boot_info_without_rollup_config;
        let rollup_config = RollupConfig::from_l2_chain_id(chain_id).unwrap();

        Self { l1_head, l2_output_root, l2_claim, l2_claim_block, chain_id, rollup_config }
    }
}

sol! {
    struct RawBootInfoStruct {
        bytes32 l1Head;
        bytes32 l2PreRoot;
        bytes32 l2PostRoot;
        uint64 l2BlockNumber;
        uint64 chainId;
    }
}

impl RawBootInfo {
    /// ABI encode the boot info. This is used to commit to in the zkVM,
    /// so that we can verify on chain that the correct values were used in
    /// the proof.
    pub fn abi_encode(&self) -> Vec<u8> {
        RawBootInfoStruct {
            l1Head: self.l1_head,
            l2PreRoot: self.l2_output_root,
            l2PostRoot: self.l2_claim,
            l2BlockNumber: self.l2_claim_block,
            chainId: self.chain_id,
        }
        .abi_encode()
    }

    pub fn abi_decode(bytes: &[u8]) -> Result<Self> {
        let boot_info = RawBootInfoStruct::abi_decode(bytes, true)?;
        Ok(Self {
            l1_head: boot_info.l1Head,
            l2_output_root: boot_info.l2PreRoot,
            l2_claim: boot_info.l2PostRoot,
            l2_claim_block: boot_info.l2BlockNumber,
            chain_id: boot_info.chainId,
        })
    }
}
