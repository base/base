//! This module contains the prologue phase of the client program, pulling in the boot
//! information, which is passed to the zkVM as public inputs to be verified on-chain.

use alloy_primitives::{B256, Bytes};
use alloy_sol_types::sol;
use base_common_genesis::RollupConfig;
use base_proof::BootInfo;
use base_proof_primitives::PerChainConfig;
use serde::{Deserialize, Serialize};

/// Hash the rollup config using the canonical [`PerChainConfig`] binary encoding and keccak256.
///
/// This is stable across hardfork additions: only the core chain identity fields are hashed,
/// so adding a new fork timestamp to [`RollupConfig`] does not change the hash.
pub fn hash_rollup_config(config: &RollupConfig) -> B256 {
    let mut per_chain =
        PerChainConfig::from_rollup_config(config).expect("rollup config missing system_config");
    per_chain.force_defaults();
    per_chain.hash()
}

sol! {
    #[derive(Debug, Serialize, Deserialize)]
    struct BootInfoStruct {
        bytes32 l1Head;
        bytes32 l2PreRoot;
        bytes32 l2PostRoot;
        uint64 l2PreBlockNumber;
        uint64 l2BlockNumber;
        bytes32 rollupConfigHash;
        bytes intermediateRoots;
    }
}

impl BootInfoStruct {
    /// Create from a [`BootInfo`], the derived L2 block number, and intermediate state roots.
    pub fn new(
        boot_info: BootInfo,
        l2_pre_block_number: u64,
        l2_block_number: u64,
        intermediate_roots: Vec<B256>,
    ) -> Self {
        // Defense-in-depth: witness execution validates this before constructing the committed
        // public inputs, and this assert preserves that invariant for any direct caller.
        assert_eq!(
            l2_block_number, boot_info.claimed_l2_block_number,
            "derived L2 block number must match claimed L2 block number"
        );

        Self {
            l1Head: boot_info.l1_head,
            l2PreRoot: boot_info.agreed_l2_output_root,
            l2PostRoot: boot_info.claimed_l2_output_root,
            l2PreBlockNumber: l2_pre_block_number,
            l2BlockNumber: l2_block_number,
            rollupConfigHash: hash_rollup_config(&boot_info.rollup_config),
            intermediateRoots: Bytes::from(
                intermediate_roots
                    .iter()
                    .flat_map(|root| root.as_slice())
                    .copied()
                    .collect::<Vec<u8>>(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, b256};
    use base_common_chains::Registry;

    use super::*;

    fn boot_info(claimed_l2_block_number: u64) -> BootInfo {
        let rollup_config =
            Registry::rollup_config(8453).expect("Base mainnet config should exist").clone();
        let l1_config = base_common_chains::L1_CONFIGS
            .get(&rollup_config.l1_chain_id)
            .expect("Base mainnet L1 config should exist")
            .clone();

        BootInfo {
            l1_head: B256::repeat_byte(0x11),
            agreed_l2_output_root: B256::repeat_byte(0x22),
            claimed_l2_output_root: B256::repeat_byte(0x33),
            claimed_l2_block_number,
            chain_id: rollup_config.l2_chain_id.id(),
            rollup_config,
            l1_config,
            proposer: Address::ZERO,
            intermediate_block_interval: 0,
            l1_head_number: 0,
        }
    }

    #[test]
    fn boot_info_struct_uses_derived_l2_block_number() {
        let boot = boot_info(20);
        let boot_info_struct = BootInfoStruct::new(boot, 10, 20, vec![B256::repeat_byte(0x44)]);

        assert_eq!(boot_info_struct.l2PreBlockNumber, 10);
        assert_eq!(boot_info_struct.l2BlockNumber, 20);
        assert_eq!(
            boot_info_struct.intermediateRoots,
            Bytes::from(B256::repeat_byte(0x44).to_vec())
        );
    }

    #[test]
    #[should_panic(expected = "derived L2 block number must match claimed L2 block number")]
    fn boot_info_struct_rejects_mismatched_derived_l2_block_number() {
        let boot = boot_info(20);

        let _ = BootInfoStruct::new(boot, 10, 19, Vec::new());
    }

    /// Verify that `hash_rollup_config` produces the same value as the nitro-enclave's
    /// `PerChainConfig::hash()` for each supported chain. These expected values are the
    /// `CONFIG_HASH`_* constants hardcoded in base-proof-tee-nitro-enclave/src/server.rs.
    #[test]
    fn test_config_hash_matches_nitro_enclave() {
        let cases: &[(u64, B256)] = &[
            (8453, b256!("1607709d90d40904f790574404e2ad614eac858f6162faa0ec34c6bf5e5f3c57")),
            (84532, b256!("12e9c45f19f9817c6d4385fad29e7a70c355502cf0883e76a9a7e478a85d1360")),
        ];

        for &(chain_id, expected) in cases {
            let rollup = Registry::rollup_config(chain_id)
                .unwrap_or_else(|| panic!("missing rollup config for chain {chain_id}"));
            let got = hash_rollup_config(rollup);
            assert_eq!(got, expected, "config hash mismatch for chain {chain_id}");
        }
    }
}
