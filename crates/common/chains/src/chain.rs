use core::ops::Index;

// Production imports for upgrade implementations
use EthereumHardfork::{
    Amsterdam, ArrowGlacier, Berlin, Bpo1, Bpo2, Bpo3, Bpo4, Bpo5, Byzantium, Constantinople, Dao,
    Frontier, GrayGlacier, Homestead, Istanbul, London, MuirGlacier, Paris, Petersburg,
    SpuriousDragon, Tangerine,
};
use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
use alloy_primitives::U256;
use base_common_genesis::BaseUpgrade;

use crate::{BaseUpgradeExt, Upgrades};

/// Number of upgrades in the Base execution fork ladder
/// ([`BaseUpgrade::EXECUTION_VARIANTS`]).
const EXECUTION_FORK_COUNT: usize = BaseUpgrade::EXECUTION_VARIANTS.len();

/// A type allowing to configure activation [`ForkCondition`]s for a given list of
/// [`BaseUpgrade`]s.
///
/// Zips together [`EthereumHardfork`]s and [`BaseUpgrade`]s. Base upgrades whenever Ethereum
/// upgrades. When Ethereum upgrades, a new [`BaseUpgrade`] piggybacks on top of the new
/// [`EthereumHardfork`] to include (or to noop) the L1 changes on L2.
///
/// Base can also upgrade independently of Ethereum. The relation between Ethereum and Base
/// upgrades is described by predicate [`EthereumHardfork`] `=>` [`BaseUpgrade`], since a Base
/// chain can undergo a [`BaseUpgrade`] without an [`EthereumHardfork`], but not the other way
/// around.
#[derive(Debug, Clone)]
pub struct ChainUpgrades {
    /// Activation conditions for the execution fork ladder, indexed by
    /// [`BaseUpgrade::execution_idx`]. Upgrades absent from the input default to
    /// [`ForkCondition::Never`].
    forks: [ForkCondition; EXECUTION_FORK_COUNT],
}

impl ChainUpgrades {
    /// Creates a new [`ChainUpgrades`] from the given list of forks.
    ///
    /// Only execution-ladder upgrades ([`BaseUpgrade::EXECUTION_VARIANTS`]) are stored; any
    /// contract-only upgrades (e.g. `Delta`, `PectraBlobSchedule`) in the input are ignored.
    /// When an upgrade appears more than once, the last entry wins.
    pub fn new(forks: impl IntoIterator<Item = (BaseUpgrade, ForkCondition)>) -> Self {
        let mut conditions = [ForkCondition::Never; EXECUTION_FORK_COUNT];
        for (upgrade, condition) in forks {
            if let Some(idx) = upgrade.execution_idx() {
                conditions[idx] = condition;
            }
        }
        Self { forks: conditions }
    }

    /// Creates a new [`ChainUpgrades`] with Base mainnet configuration.
    pub fn mainnet() -> Self {
        Self::new(BaseUpgrade::mainnet())
    }

    /// Creates a new [`ChainUpgrades`] with Base Sepolia configuration.
    pub fn sepolia() -> Self {
        Self::new(BaseUpgrade::sepolia())
    }

    /// Creates a new [`ChainUpgrades`] with devnet configuration.
    pub fn devnet() -> Self {
        Self::new(BaseUpgrade::devnet())
    }

    /// Creates a new [`ChainUpgrades`] with Base Zeronet configuration.
    pub fn zeronet() -> Self {
        Self::new(BaseUpgrade::zeronet())
    }
}

impl EthereumHardforks for ChainUpgrades {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self[fork]
    }
}

impl Upgrades for ChainUpgrades {
    fn upgrade_activation(&self, fork: BaseUpgrade) -> ForkCondition {
        self[fork]
    }
}

impl Index<BaseUpgrade> for ChainUpgrades {
    type Output = ForkCondition;

    fn index(&self, hf: BaseUpgrade) -> &Self::Output {
        // Contract-only upgrades are absent from the execution fork ladder.
        hf.execution_idx().map_or(&ForkCondition::Never, |idx| &self.forks[idx])
    }
}

impl Index<EthereumHardfork> for ChainUpgrades {
    type Output = ForkCondition;

    fn index(&self, hf: EthereumHardfork) -> &Self::Output {
        if let Some(base_upgrade) = BaseUpgrade::from_ethereum_hardfork(hf) {
            return &self[base_upgrade];
        }

        match hf {
            // Dao Upgrade is not needed for ChainUpgrades
            Dao | Bpo1 | Bpo2 | Bpo3 | Bpo4 | Bpo5 | Amsterdam => &ForkCondition::Never,
            Frontier | Homestead | Tangerine | SpuriousDragon | Byzantium | Constantinople
            | Petersburg | Istanbul | MuirGlacier | Berlin => &ForkCondition::ZERO_BLOCK,
            London | ArrowGlacier | GrayGlacier => &self[BaseUpgrade::Bedrock],
            Paris => &ForkCondition::TTD {
                activation_block_number: 0,
                fork_block: Some(0),
                total_difficulty: U256::ZERO,
            },
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use BaseUpgrade::{
        Azul, Bedrock, Beryl, Canyon, Cobalt, Ecotone, Fjord, Granite, Holocene, Isthmus, Jovian,
        Regolith,
    };
    use alloy_hardforks::EthereumHardfork;

    use super::*;
    use crate::ChainConfig;

    #[test]
    fn base_mainnet_fork_conditions() {
        let base_mainnet_forks = ChainUpgrades::mainnet();
        assert_eq!(
            base_mainnet_forks[Bedrock],
            ForkCondition::Block(ChainConfig::mainnet().bedrock_block)
        );
        assert_eq!(
            base_mainnet_forks[Regolith],
            ForkCondition::Timestamp(ChainConfig::mainnet().regolith_timestamp)
        );
        assert_eq!(
            base_mainnet_forks[Canyon],
            ForkCondition::Timestamp(ChainConfig::mainnet().canyon_timestamp)
        );
        assert_eq!(
            base_mainnet_forks[Ecotone],
            ForkCondition::Timestamp(ChainConfig::mainnet().ecotone_timestamp)
        );
        assert_eq!(
            base_mainnet_forks[Fjord],
            ForkCondition::Timestamp(ChainConfig::mainnet().fjord_timestamp)
        );
        assert_eq!(
            base_mainnet_forks[Granite],
            ForkCondition::Timestamp(ChainConfig::mainnet().granite_timestamp)
        );
        assert_eq!(
            base_mainnet_forks[Holocene],
            ForkCondition::Timestamp(ChainConfig::mainnet().holocene_timestamp)
        );
        assert_eq!(
            base_mainnet_forks[Isthmus],
            ForkCondition::Timestamp(ChainConfig::mainnet().isthmus_timestamp)
        );
        assert_eq!(
            base_mainnet_forks[Jovian],
            ForkCondition::Timestamp(ChainConfig::mainnet().jovian_timestamp)
        );
        assert_eq!(
            base_mainnet_forks[Azul],
            ForkCondition::Timestamp(ChainConfig::mainnet().azul_timestamp.unwrap())
        );
        assert_eq!(
            base_mainnet_forks[Beryl],
            ForkCondition::Timestamp(ChainConfig::mainnet().beryl_timestamp.unwrap())
        );
        assert_eq!(base_mainnet_forks[Cobalt], ForkCondition::Never);
    }

    #[test]
    fn base_sepolia_fork_conditions() {
        let base_sepolia_forks = ChainUpgrades::sepolia();
        assert_eq!(
            base_sepolia_forks[Bedrock],
            ForkCondition::Block(ChainConfig::sepolia().bedrock_block)
        );
        assert_eq!(
            base_sepolia_forks[Regolith],
            ForkCondition::Timestamp(ChainConfig::sepolia().regolith_timestamp)
        );
        assert_eq!(
            base_sepolia_forks[Canyon],
            ForkCondition::Timestamp(ChainConfig::sepolia().canyon_timestamp)
        );
        assert_eq!(
            base_sepolia_forks[Ecotone],
            ForkCondition::Timestamp(ChainConfig::sepolia().ecotone_timestamp)
        );
        assert_eq!(
            base_sepolia_forks[Fjord],
            ForkCondition::Timestamp(ChainConfig::sepolia().fjord_timestamp)
        );
        assert_eq!(
            base_sepolia_forks[Granite],
            ForkCondition::Timestamp(ChainConfig::sepolia().granite_timestamp)
        );
        assert_eq!(
            base_sepolia_forks[Holocene],
            ForkCondition::Timestamp(ChainConfig::sepolia().holocene_timestamp)
        );
        assert_eq!(
            base_sepolia_forks[Isthmus],
            ForkCondition::Timestamp(ChainConfig::sepolia().isthmus_timestamp)
        );
        assert_eq!(
            base_sepolia_forks.upgrade_activation(Jovian),
            ForkCondition::Timestamp(ChainConfig::sepolia().jovian_timestamp)
        );
        assert_eq!(
            base_sepolia_forks[Azul],
            ForkCondition::Timestamp(ChainConfig::sepolia().azul_timestamp.unwrap())
        );
        assert_eq!(
            base_sepolia_forks[Beryl],
            ForkCondition::Timestamp(ChainConfig::sepolia().beryl_timestamp.unwrap())
        );
        assert_eq!(base_sepolia_forks[Cobalt], ForkCondition::Never);
    }

    #[test]
    fn is_jovian_active_at_timestamp() {
        let base_mainnet_forks = ChainUpgrades::mainnet();
        assert!(
            base_mainnet_forks
                .is_jovian_active_at_timestamp(ChainConfig::mainnet().jovian_timestamp)
        );
        assert!(
            !base_mainnet_forks
                .is_jovian_active_at_timestamp(ChainConfig::mainnet().jovian_timestamp - 1)
        );
        assert!(
            base_mainnet_forks
                .is_jovian_active_at_timestamp(ChainConfig::mainnet().jovian_timestamp + 1000)
        );

        let base_sepolia_forks = ChainUpgrades::sepolia();
        assert!(
            base_sepolia_forks
                .is_jovian_active_at_timestamp(ChainConfig::sepolia().jovian_timestamp)
        );
        assert!(
            !base_sepolia_forks
                .is_jovian_active_at_timestamp(ChainConfig::sepolia().jovian_timestamp - 1)
        );
        assert!(
            base_sepolia_forks
                .is_jovian_active_at_timestamp(ChainConfig::sepolia().jovian_timestamp + 1000)
        );
    }

    #[test]
    fn is_azul_active_at_timestamp() {
        // Azul is scheduled on mainnet at 1779991200
        let base_mainnet_forks = ChainUpgrades::mainnet();
        assert!(!base_mainnet_forks.is_azul_active_at_timestamp(0));
        assert!(!base_mainnet_forks.is_azul_active_at_timestamp(1_779_991_199));
        assert!(base_mainnet_forks.is_azul_active_at_timestamp(1_779_991_200));
        assert!(base_mainnet_forks.is_azul_active_at_timestamp(u64::MAX));

        // Azul is scheduled on sepolia at 1776708000
        let base_sepolia_forks = ChainUpgrades::sepolia();
        assert!(!base_sepolia_forks.is_azul_active_at_timestamp(0));
        assert!(!base_sepolia_forks.is_azul_active_at_timestamp(1_776_707_999));
        assert!(base_sepolia_forks.is_azul_active_at_timestamp(1_776_708_000));
        assert!(base_sepolia_forks.is_azul_active_at_timestamp(u64::MAX));

        // Azul is active at genesis on devnet (ForkCondition::ZERO_TIMESTAMP)
        let devnet_forks = ChainUpgrades::devnet();
        assert!(devnet_forks.is_azul_active_at_timestamp(0));

        // Azul is scheduled on zeronet at 1775152800
        let zeronet_forks = ChainUpgrades::zeronet();
        assert!(!zeronet_forks.is_azul_active_at_timestamp(0));
        assert!(!zeronet_forks.is_azul_active_at_timestamp(1_775_152_799));
        assert!(zeronet_forks.is_azul_active_at_timestamp(1_775_152_800));
        assert!(zeronet_forks.is_azul_active_at_timestamp(u64::MAX));
    }

    #[test]
    fn is_beryl_active_at_timestamp() {
        let base_mainnet_forks = ChainUpgrades::mainnet();
        assert!(!base_mainnet_forks.is_beryl_active_at_timestamp(0));
        assert!(!base_mainnet_forks.is_beryl_active_at_timestamp(1_782_410_399));
        assert!(base_mainnet_forks.is_beryl_active_at_timestamp(1_782_410_400));
        assert!(base_mainnet_forks.is_beryl_active_at_timestamp(u64::MAX));

        let base_sepolia_forks = ChainUpgrades::sepolia();
        assert!(!base_sepolia_forks.is_beryl_active_at_timestamp(0));
        assert!(!base_sepolia_forks.is_beryl_active_at_timestamp(1_781_805_599));
        assert!(base_sepolia_forks.is_beryl_active_at_timestamp(1_781_805_600));
        assert!(base_sepolia_forks.is_beryl_active_at_timestamp(u64::MAX));

        let zeronet_forks = ChainUpgrades::zeronet();
        assert!(!zeronet_forks.is_beryl_active_at_timestamp(0));
        assert!(!zeronet_forks.is_beryl_active_at_timestamp(1_780_678_799));
        assert!(zeronet_forks.is_beryl_active_at_timestamp(1_780_678_800));
        assert!(zeronet_forks.is_beryl_active_at_timestamp(u64::MAX));
    }

    #[test]
    fn osaka_tracks_base_azul_activation() {
        let base_mainnet_forks = ChainUpgrades::mainnet();
        assert_eq!(
            base_mainnet_forks.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Timestamp(1_779_991_200)
        );

        let base_sepolia_forks = ChainUpgrades::sepolia();
        assert_eq!(
            base_sepolia_forks.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Timestamp(1_776_708_000)
        );

        let devnet_forks = ChainUpgrades::devnet();
        assert_eq!(
            devnet_forks.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::ZERO_TIMESTAMP
        );

        let zeronet_forks = ChainUpgrades::zeronet();
        assert_eq!(
            zeronet_forks.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Timestamp(1_775_152_800)
        );
    }

    #[test]
    fn test_ethereum_fork_activation_consistency() {
        let base_mainnet_forks = ChainUpgrades::mainnet();
        for ethereum_upgrade in EthereumHardfork::VARIANTS {
            let _ = base_mainnet_forks.ethereum_fork_activation(*ethereum_upgrade);
        }
        for base_upgrade in BaseUpgrade::VARIANTS {
            let _ = base_mainnet_forks.upgrade_activation(*base_upgrade);
        }
    }
}
