//! Chain specification for the Base devnet-0-sepolia-dev-0 network.

use alloc::{sync::Arc, vec};

use alloy_chains::Chain;
use alloy_primitives::{U256, b256};
use base_common_chains::{BASE_DEVNET_0_SEPOLIA_DEV_0_UPGRADES, BaseUpgrade, ChainConfig};
use reth_chainspec::{BaseFeeParams, BaseFeeParamsKind, ChainSpec, Hardfork};
use reth_ethereum_forks::EthereumHardfork;
use reth_primitives_traits::{SealedHeader, sync::LazyLock};

use crate::BaseChainSpec;

/// The Base devnet-0-sepolia-dev-0 spec
pub static BASE_DEVNET_0_SEPOLIA_DEV_0: LazyLock<Arc<BaseChainSpec>> = LazyLock::new(|| {
    let genesis = serde_json::from_str(ChainConfig::alpha().genesis_json)
        .expect("Can't deserialize Base devnet-0-sepolia-dev-0 genesis json");
    let hardforks = BASE_DEVNET_0_SEPOLIA_DEV_0_UPGRADES.clone();
    BaseChainSpec {
        inner: ChainSpec {
            chain: Chain::from_id(11763072),
            genesis_header: SealedHeader::new(
                BaseChainSpec::make_genesis_header(&genesis, &hardforks),
                b256!("0x1ab91449a7c65b8cd6c06f13e2e7ea2d10b6f9cbf5def79f362f2e7e501d2928"),
            ),
            genesis,
            paris_block_and_final_difficulty: Some((0, U256::from(0))),
            hardforks,
            base_fee_params: BaseFeeParamsKind::Variable(
                vec![
                    (EthereumHardfork::London.boxed(), BaseFeeParams::base_sepolia()),
                    (BaseUpgrade::Canyon.boxed(), BaseFeeParams::base_sepolia_canyon()),
                ]
                .into(),
            ),
            prune_delete_limit: 10000,
            ..Default::default()
        },
    }
    .into()
});
