//! Rollup and L1 chain configuration registry.

use alloy_primitives::{Address, map::HashMap};
use base_consensus_genesis::{L1ChainConfig, RollupConfig};

use crate::L1Config;

lazy_static::lazy_static! {
    /// Private initializer that loads the chain configurations.
    static ref INIT: (
        HashMap<u64, RollupConfig>,
        HashMap<u64, base_consensus_genesis::ChainConfig>,
    ) = init_configs();

    /// Rollup configurations loaded from embedded TOML config files.
    static ref ROLLUP_CONFIGS: HashMap<u64, RollupConfig> = INIT.0.clone();

    /// L1 chain configurations built from known L1 genesis data.
    static ref L1_CONFIGS: HashMap<u64, L1ChainConfig> = L1Config::build_l1_configs();
}

/// A registry of chain configurations for Base networks.
///
/// Provides access to rollup configs, L1 chain configs, and the unsafe block signer
/// for supported chain IDs. All data is lazily initialized from embedded TOML config
/// files and known L1 genesis data.
#[derive(Debug)]
pub struct Registry;

impl Registry {
    /// Returns a [`RollupConfig`] for the given chain ID.
    pub fn rollup_config(chain_id: u64) -> Option<&'static RollupConfig> {
        ROLLUP_CONFIGS.get(&chain_id)
    }

    /// Returns a [`RollupConfig`] by its [`alloy_chains::Chain`] identifier.
    pub fn rollup_config_by_chain(chain: &alloy_chains::Chain) -> Option<&'static RollupConfig> {
        ROLLUP_CONFIGS.get(&chain.id())
    }

    /// Returns an [`L1ChainConfig`] for the given L1 chain ID.
    pub fn l1_config(chain_id: u64) -> Option<&'static L1ChainConfig> {
        L1_CONFIGS.get(&chain_id)
    }

    /// Returns the `unsafe_block_signer` address for the given chain ID.
    pub fn unsafe_block_signer(chain_id: u64) -> Option<Address> {
        INIT.1.get(&chain_id)?.roles.as_ref()?.unsafe_block_signer
    }
}

/// Initialize chain and rollup configurations from embedded TOML config files.
fn init_configs() -> (HashMap<u64, RollupConfig>, HashMap<u64, base_consensus_genesis::ChainConfig>)
{
    let configs: [(&str, &str); 3] = [
        ("base-mainnet", include_str!("../configs/base-mainnet.toml")),
        ("base-sepolia", include_str!("../configs/base-sepolia.toml")),
        ("base-devnet-0", include_str!("../configs/base-devnet-0.toml")),
    ];

    let mut rollup_configs = HashMap::default();
    let mut chain_configs = HashMap::default();

    for (name, toml_str) in configs {
        let mut chain_config: base_consensus_genesis::ChainConfig = toml::from_str(toml_str)
            .unwrap_or_else(|e| panic!("Failed to parse {name} config: {e}"));

        if let Some(a) = &mut chain_config.addresses {
            a.zero_proof_addresses();
        }

        let rollup = chain_config.as_rollup_config();
        rollup_configs.insert(chain_config.chain_id, rollup);
        chain_configs.insert(chain_config.chain_id, chain_config);
    }

    (rollup_configs, chain_configs)
}

#[cfg(test)]
mod tests {
    use alloy_chains::Chain as AlloyChain;
    use alloy_hardforks::{
        holesky::{HOLESKY_BPO1_TIMESTAMP, HOLESKY_BPO2_TIMESTAMP},
        sepolia::{SEPOLIA_BPO1_TIMESTAMP, SEPOLIA_BPO2_TIMESTAMP},
    };
    use base_alloy_hardforks::{BASE_MAINNET_JOVIAN_TIMESTAMP, BASE_SEPOLIA_JOVIAN_TIMESTAMP};

    use super::*;
    use crate::test_utils;

    #[test]
    fn test_hardcoded_rollup_configs() {
        let test_cases =
            [(8453, test_utils::BASE_MAINNET_CONFIG), (84532, test_utils::BASE_SEPOLIA_CONFIG)]
                .to_vec();

        for (chain_id, expected) in test_cases {
            let derived = Registry::rollup_config(chain_id).unwrap();
            assert_eq!(expected, *derived);
        }
    }

    #[test]
    fn test_rollup_config_by_chain() {
        const ALLOY_BASE: AlloyChain = AlloyChain::base_mainnet();

        let by_chain = Registry::rollup_config_by_chain(&ALLOY_BASE).unwrap();
        let by_id = Registry::rollup_config(8453).unwrap();

        assert_eq!(by_chain, by_id);
    }

    #[test]
    fn test_jovian_timestamps() {
        let base_mainnet = Registry::rollup_config(8453).unwrap();
        assert_eq!(base_mainnet.hardforks.jovian_time, Some(BASE_MAINNET_JOVIAN_TIMESTAMP));

        let base_sepolia = Registry::rollup_config(84532).unwrap();
        assert_eq!(base_sepolia.hardforks.jovian_time, Some(BASE_SEPOLIA_JOVIAN_TIMESTAMP));
    }

    #[test]
    fn test_bpo_timestamps() {
        let sepolia_config = Registry::l1_config(11155111).unwrap();
        assert_eq!(sepolia_config.bpo1_time, Some(SEPOLIA_BPO1_TIMESTAMP));
        assert_eq!(sepolia_config.bpo2_time, Some(SEPOLIA_BPO2_TIMESTAMP));

        let holesky_config = Registry::l1_config(17000).unwrap();
        assert_eq!(holesky_config.bpo1_time, Some(HOLESKY_BPO1_TIMESTAMP));
        assert_eq!(holesky_config.bpo2_time, Some(HOLESKY_BPO2_TIMESTAMP));
    }
}
