use alloc::vec;

use alloy_primitives::U256;
use base_alloy_chains::{BaseChainUpgrades, BaseUpgrade};
use reth_ethereum_forks::{ChainHardforks, EthereumHardfork, ForkCondition, Hardfork};
use spin::Lazy;

/// Extension trait to convert alloy's [`BaseChainUpgrades`] into reth's [`ChainHardforks`].
pub trait BaseChainUpgradesExt {
    /// Expands Base hardforks into a full [`ChainHardforks`] including implied Ethereum entries.
    ///
    /// Pre-Bedrock Ethereum hardforks are set to block 0. Paired Ethereum hardforks
    /// use their Base counterpart's timestamp:
    /// Shanghai=Canyon, Cancun=Ecotone, Prague=Isthmus, Osaka=V1.
    fn to_chain_hardforks(&self) -> ChainHardforks;
}

impl BaseChainUpgradesExt for BaseChainUpgrades {
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

        forks.push((BaseUpgrade::Bedrock.boxed(), self[BaseUpgrade::Bedrock]));
        forks.push((BaseUpgrade::Regolith.boxed(), self[BaseUpgrade::Regolith]));

        let canyon = self[BaseUpgrade::Canyon];
        forks.push((EthereumHardfork::Shanghai.boxed(), canyon));
        forks.push((BaseUpgrade::Canyon.boxed(), canyon));

        let ecotone = self[BaseUpgrade::Ecotone];
        forks.push((EthereumHardfork::Cancun.boxed(), ecotone));
        forks.push((BaseUpgrade::Ecotone.boxed(), ecotone));

        forks.push((BaseUpgrade::Fjord.boxed(), self[BaseUpgrade::Fjord]));
        forks.push((BaseUpgrade::Granite.boxed(), self[BaseUpgrade::Granite]));
        forks.push((BaseUpgrade::Holocene.boxed(), self[BaseUpgrade::Holocene]));

        let isthmus = self[BaseUpgrade::Isthmus];
        if !matches!(isthmus, ForkCondition::Never) {
            forks.push((EthereumHardfork::Prague.boxed(), isthmus));
            forks.push((BaseUpgrade::Isthmus.boxed(), isthmus));
        }

        let jovian = self[BaseUpgrade::Jovian];
        if !matches!(jovian, ForkCondition::Never) {
            forks.push((BaseUpgrade::Jovian.boxed(), jovian));
        }

        let base_v1 = self[BaseUpgrade::V1];
        if !matches!(base_v1, ForkCondition::Never) {
            forks.push((EthereumHardfork::Osaka.boxed(), base_v1));
            forks.push((BaseUpgrade::V1.boxed(), base_v1));
        }

        ChainHardforks::new(forks)
    }
}

/// Dev hardforks.
pub static DEV_HARDFORKS: Lazy<ChainHardforks> =
    Lazy::new(|| BaseChainUpgrades::devnet().to_chain_hardforks());

/// Base Sepolia list of hardforks.
pub static BASE_SEPOLIA_HARDFORKS: Lazy<ChainHardforks> =
    Lazy::new(|| BaseChainUpgrades::sepolia().to_chain_hardforks());

/// Base mainnet list of hardforks.
pub static BASE_MAINNET_HARDFORKS: Lazy<ChainHardforks> =
    Lazy::new(|| BaseChainUpgrades::mainnet().to_chain_hardforks());

/// Base devnet-0-sepolia-dev-0 list of hardforks.
pub static BASE_DEVNET_0_SEPOLIA_DEV_0_HARDFORKS: Lazy<ChainHardforks> =
    Lazy::new(|| BaseChainUpgrades::base_devnet_0_sepolia_dev_0().to_chain_hardforks());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_v1_expands_to_osaka() {
        let hardforks =
            BaseChainUpgrades::new(BaseUpgrade::devnet().into_iter().map(|(fork, cond)| {
                if fork == BaseUpgrade::V1 {
                    (fork, ForkCondition::Timestamp(1_000_000))
                } else {
                    (fork, cond)
                }
            }))
            .to_chain_hardforks();
        assert_eq!(hardforks.get(BaseUpgrade::V1), Some(ForkCondition::Timestamp(1_000_000)));
        assert_eq!(hardforks.get(EthereumHardfork::Osaka), hardforks.get(BaseUpgrade::V1));
    }
}
