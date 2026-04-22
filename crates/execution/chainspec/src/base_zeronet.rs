//! Chain specification for the Base zeronet network.

use alloc::{sync::Arc, vec};

use alloy_chains::Chain;
use alloy_primitives::{U256, b256};
use base_common_chains::{BASE_ZERONET_UPGRADES, BaseUpgrade, ChainConfig};
use reth_chainspec::{BaseFeeParams, BaseFeeParamsKind, ChainSpec, Hardfork};
use reth_ethereum_forks::EthereumHardfork;
use reth_primitives_traits::{SealedHeader, sync::LazyLock};

use crate::BaseChainSpec;

/// The Base zeronet spec
pub static BASE_ZERONET: LazyLock<Arc<BaseChainSpec>> = LazyLock::new(|| {
    let genesis = serde_json::from_str(ChainConfig::zeronet().genesis_json)
        .expect("Can't deserialize Base zeronet genesis json");
    let hardforks = BASE_ZERONET_UPGRADES.clone();
    BaseChainSpec {
        inner: ChainSpec {
            chain: Chain::from_id(763360),
            genesis_header: SealedHeader::new(
                BaseChainSpec::make_genesis_header(&genesis, &hardforks),
                b256!("0x1842d6ef4c40e2a4794458e167f6d327269df919b626979111c37ad3a96047bf"),
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
            prune_delete_limit: 10000,
            ..Default::default()
        },
    }
    .into()
});
