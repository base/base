use alloc::{boxed::Box, vec};

use alloy_primitives::U256;
use base_common_chains::{BaseUpgrade, ChainUpgrades};
use reth_ethereum_forks::{ChainHardforks, EthereumHardfork, ForkCondition, Hardfork};
/// Extension trait to convert alloy's [`ChainUpgrades`] into reth's fork schedule.
pub trait ChainUpgradesExt {
    /// Expands Base upgrades into a full reth fork schedule including implied Ethereum entries.
    ///
    /// Pre-Bedrock Ethereum upgrades are set to block 0. Paired Ethereum upgrades
    /// use their Base counterpart's timestamp:
    /// Shanghai=Canyon, Cancun=Ecotone, Prague=Isthmus, Osaka=Azul.
    fn to_chain_upgrades(&self) -> ChainHardforks;
}

impl ChainUpgradesExt for ChainUpgrades {
    fn to_chain_upgrades(&self) -> ChainHardforks {
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

        let azul = self[BaseUpgrade::Azul];
        if !matches!(azul, ForkCondition::Never) {
            forks.push((EthereumHardfork::Osaka.boxed(), azul));
            forks.push((BaseUpgrade::Azul.boxed(), azul));
        }

        let beryl = self[BaseUpgrade::Beryl];
        if !matches!(beryl, ForkCondition::Never) {
            forks.push((BaseUpgrade::Beryl.boxed(), beryl));
        }

        let cobalt = self[BaseUpgrade::Cobalt];
        if !matches!(cobalt, ForkCondition::Never) {
            forks.push((BaseUpgrade::Cobalt.boxed(), cobalt));
        }

        ChainHardforks::new(forks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn azul_expands_to_osaka() {
        let upgrades = ChainUpgrades::new(BaseUpgrade::devnet().into_iter().map(|(fork, cond)| {
            if fork == BaseUpgrade::Azul {
                (fork, ForkCondition::Timestamp(1_000_000))
            } else {
                (fork, cond)
            }
        }))
        .to_chain_upgrades();
        assert_eq!(upgrades.get(BaseUpgrade::Azul), Some(ForkCondition::Timestamp(1_000_000)));
        assert_eq!(upgrades.get(EthereumHardfork::Osaka), upgrades.get(BaseUpgrade::Azul));
    }
}
