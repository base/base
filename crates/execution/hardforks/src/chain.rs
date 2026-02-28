use alloc::vec;

use alloy_primitives::U256;
use base_alloy_hardforks::{OpChainHardforks, OpHardfork};
use reth_ethereum_forks::{ChainHardforks, EthereumHardfork, ForkCondition, Hardfork};
use spin::Lazy;

/// Extension trait to convert alloy's [`OpChainHardforks`] into reth's [`ChainHardforks`].
pub trait OpChainHardforksExt {
    /// Expands OP hardforks into a full [`ChainHardforks`] including implied Ethereum entries.
    ///
    /// Pre-Bedrock Ethereum hardforks are set to block 0. Paired Ethereum hardforks
    /// use their OP counterpart's timestamp: Shanghai=Canyon, Cancun=Ecotone, Prague=Isthmus.
    fn to_chain_hardforks(&self) -> ChainHardforks;
}

impl OpChainHardforksExt for OpChainHardforks {
    fn to_chain_hardforks(&self) -> ChainHardforks {
        let mut forks: vec::Vec<(Box<dyn Hardfork>, ForkCondition)> = vec![
            (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::London.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::ArrowGlacier.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::GrayGlacier.boxed(), ForkCondition::Block(0)),
            (
                EthereumHardfork::Paris.boxed(),
                ForkCondition::TTD {
                    activation_block_number: 0,
                    fork_block: Some(0),
                    total_difficulty: U256::ZERO,
                },
            ),
        ];

        forks.push((OpHardfork::Bedrock.boxed(), self[OpHardfork::Bedrock]));
        forks.push((OpHardfork::Regolith.boxed(), self[OpHardfork::Regolith]));

        let canyon = self[OpHardfork::Canyon];
        forks.push((EthereumHardfork::Shanghai.boxed(), canyon));
        forks.push((OpHardfork::Canyon.boxed(), canyon));

        let ecotone = self[OpHardfork::Ecotone];
        forks.push((EthereumHardfork::Cancun.boxed(), ecotone));
        forks.push((OpHardfork::Ecotone.boxed(), ecotone));

        forks.push((OpHardfork::Fjord.boxed(), self[OpHardfork::Fjord]));
        forks.push((OpHardfork::Granite.boxed(), self[OpHardfork::Granite]));
        forks.push((OpHardfork::Holocene.boxed(), self[OpHardfork::Holocene]));

        let isthmus = self[OpHardfork::Isthmus];
        forks.push((EthereumHardfork::Prague.boxed(), isthmus));
        forks.push((OpHardfork::Isthmus.boxed(), isthmus));

        forks.push((OpHardfork::Jovian.boxed(), self[OpHardfork::Jovian]));

        ChainHardforks::new(forks)
    }
}

/// Dev hardforks.
pub static DEV_HARDFORKS: Lazy<ChainHardforks> =
    Lazy::new(|| OpChainHardforks::devnet().to_chain_hardforks());

/// Base Sepolia list of hardforks.
pub static BASE_SEPOLIA_HARDFORKS: Lazy<ChainHardforks> =
    Lazy::new(|| OpChainHardforks::base_sepolia().to_chain_hardforks());

/// Base mainnet list of hardforks.
pub static BASE_MAINNET_HARDFORKS: Lazy<ChainHardforks> =
    Lazy::new(|| OpChainHardforks::base_mainnet().to_chain_hardforks());
