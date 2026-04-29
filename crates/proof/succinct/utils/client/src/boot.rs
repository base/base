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
    /// Create from a [`BootInfo`] and intermediate state roots.
    pub fn new(
        boot_info: BootInfo,
        l2_pre_block_number: u64,
        intermediate_roots: Vec<B256>,
    ) -> Self {
        Self {
            l1Head: boot_info.l1_head,
            l2PreRoot: boot_info.agreed_l2_output_root,
            l2PostRoot: boot_info.claimed_l2_output_root,
            l2PreBlockNumber: l2_pre_block_number,
            l2BlockNumber: boot_info.claimed_l2_block_number,
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
    use alloy_primitives::b256;
    use base_common_chains::Registry;

    use super::*;

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
