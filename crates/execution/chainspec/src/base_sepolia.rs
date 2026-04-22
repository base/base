//! Chain specification for the Base Sepolia testnet network.

use alloc::{sync::Arc, vec};

use alloy_chains::Chain;
use alloy_primitives::{U256, b256};
use base_common_chains::{BASE_SEPOLIA_UPGRADES, BaseUpgrade, ChainConfig};
use reth_chainspec::{BaseFeeParams, BaseFeeParamsKind, ChainSpec, Hardfork};
use reth_ethereum_forks::EthereumHardfork;
use reth_primitives_traits::{SealedHeader, sync::LazyLock};

use crate::BaseChainSpec;

/// The Base Sepolia spec
pub static BASE_SEPOLIA: LazyLock<Arc<BaseChainSpec>> = LazyLock::new(|| {
    let genesis = serde_json::from_str(ChainConfig::sepolia().genesis_json)
        .expect("Can't deserialize Base Sepolia genesis json");
    let hardforks = BASE_SEPOLIA_UPGRADES.clone();
    BaseChainSpec {
        inner: ChainSpec {
            chain: Chain::base_sepolia(),
            genesis_header: SealedHeader::new(
                BaseChainSpec::make_genesis_header(&genesis, &hardforks),
                b256!("0x0dcc9e089e30b90ddfc55be9a37dd15bc551aeee999d2e2b51414c54eaf934e4"),
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
