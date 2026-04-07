//! Chain specification for the Base Mainnet network.

use alloc::{sync::Arc, vec};

use alloy_chains::Chain;
use alloy_primitives::{U256, b256};
use base_alloy_chains::{BaseChainConfig, BaseUpgrade};
use base_execution_upgrades::BASE_MAINNET_UPGRADES;
use reth_chainspec::{BaseFeeParams, BaseFeeParamsKind, ChainSpec};
use reth_ethereum_forks::{EthereumHardfork, Hardfork};
use reth_primitives_traits::{SealedHeader, sync::LazyLock};

use crate::OpChainSpec;

/// The Base mainnet spec
pub static BASE_MAINNET: LazyLock<Arc<OpChainSpec>> = LazyLock::new(|| {
    let genesis = serde_json::from_str(BaseChainConfig::mainnet().genesis_json)
        .expect("Can't deserialize Base genesis json");
    let hardforks = BASE_MAINNET_UPGRADES.clone();
    OpChainSpec {
        inner: ChainSpec {
            chain: Chain::base_mainnet(),
            genesis_header: SealedHeader::new(
                OpChainSpec::make_genesis_header(&genesis, &hardforks),
                b256!("0xf712aa9241cc24369b143cf6dce85f0902a9731e70d66818a3a5845b296c73dd"),
            ),
            genesis,
            paris_block_and_final_difficulty: Some((0, U256::from(0))),
            hardforks,
            base_fee_params: BaseFeeParamsKind::Variable(
                vec![
                    (EthereumHardfork::London.boxed(), BaseFeeParams::optimism()),
                    (BaseUpgrade::Canyon.boxed(), BaseFeeParams::optimism_canyon()),
                ]
                .into(),
            ),
            ..Default::default()
        },
    }
    .into()
});
