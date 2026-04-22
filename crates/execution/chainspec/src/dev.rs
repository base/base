//! Chain specification in dev mode for custom chain.

use alloc::sync::Arc;

use alloy_chains::Chain;
use alloy_primitives::U256;
use base_common_chains::{ChainConfig, DEV_UPGRADES};
use reth_chainspec::{BaseFeeParams, BaseFeeParamsKind, ChainSpec};
use reth_primitives_traits::{SealedHeader, sync::LazyLock};

use crate::BaseChainSpec;

/// Base dev testnet specification
///
/// Includes 20 prefunded accounts with `10_000` ETH each derived from mnemonic "test test test test
/// test test test test test test test junk".
pub static BASE_DEV: LazyLock<Arc<BaseChainSpec>> = LazyLock::new(|| {
    let genesis = serde_json::from_str(ChainConfig::devnet().genesis_json)
        .expect("Can't deserialize Dev testnet genesis json");
    let hardforks = DEV_UPGRADES.clone();
    let genesis_header =
        SealedHeader::seal_slow(BaseChainSpec::make_genesis_header(&genesis, &hardforks));
    BaseChainSpec {
        inner: ChainSpec {
            chain: Chain::dev(),
            genesis_header,
            genesis,
            paris_block_and_final_difficulty: Some((0, U256::from(0))),
            hardforks,
            base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::ethereum()),
            ..Default::default()
        },
    }
    .into()
});
