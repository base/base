use alloc::vec::Vec;
use core::ops::Index;

use BaseUpgrade::{
    Bedrock, Canyon, Ecotone, Fjord, Granite, Holocene, Isthmus, Jovian, Regolith, V1,
};
// Production imports for hardfork implementations
use EthereumHardfork::{
    Amsterdam, ArrowGlacier, Berlin, Bpo1, Bpo2, Bpo3, Bpo4, Bpo5, Byzantium, Cancun,
    Constantinople, Dao, Frontier, GrayGlacier, Homestead, Istanbul, London, MuirGlacier, Osaka,
    Paris, Petersburg, Prague, Shanghai, SpuriousDragon, Tangerine,
};
use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
use alloy_primitives::U256;

use crate::{BaseUpgrade, BaseUpgrades};

/// A type allowing to configure activation [`ForkCondition`]s for a given list of
/// [`BaseUpgrade`]s.
///
/// Zips together [`EthereumHardfork`]s and [`BaseUpgrade`]s. Base hard forks whenever Ethereum
/// hard forks. When Ethereum hard forks, a new [`BaseUpgrade`] piggybacks on top of the new
/// [`EthereumHardfork`] to include (or to noop) the L1 changes on L2.
///
/// Base can also hard fork independently of Ethereum. The relation between Ethereum and Base
/// hard forks is described by predicate [`EthereumHardfork`] `=>` [`BaseUpgrade`], since a Base
/// chain can undergo a [`BaseUpgrade`] without an [`EthereumHardfork`], but not the other way
/// around.
#[derive(Debug, Clone)]
pub struct BaseChainUpgrades {
    /// Ordered list of hardfork activations.
    forks: Vec<(BaseUpgrade, ForkCondition)>,
}

impl BaseChainUpgrades {
    /// Creates a new [`BaseChainUpgrades`] with the given list of forks. The input list is sorted
    /// w.r.t. the hardcoded canonicity of [`BaseUpgrade`]s.
    pub fn new(forks: impl IntoIterator<Item = (BaseUpgrade, ForkCondition)>) -> Self {
        let mut forks = forks.into_iter().collect::<Vec<_>>();
        forks.sort();
        Self { forks }
    }

    /// Creates a new [`BaseChainUpgrades`] with Base mainnet configuration.
    pub fn mainnet() -> Self {
        Self::new(BaseUpgrade::mainnet())
    }

    /// Creates a new [`BaseChainUpgrades`] with Base Sepolia configuration.
    pub fn sepolia() -> Self {
        Self::new(BaseUpgrade::sepolia())
    }

    /// Creates a new [`BaseChainUpgrades`] with devnet configuration.
    pub fn devnet() -> Self {
        Self::new(BaseUpgrade::devnet())
    }

    /// Creates a new [`BaseChainUpgrades`] with Base devnet-0-sepolia-dev-0 configuration.
    pub fn base_devnet_0_sepolia_dev_0() -> Self {
        Self::new(BaseUpgrade::base_devnet_0_sepolia_dev_0())
    }

    /// Creates a new [`BaseChainUpgrades`] with Base Zeronet configuration.
    pub fn zeronet() -> Self {
        Self::new(BaseUpgrade::zeronet())
    }
}

impl EthereumHardforks for BaseChainUpgrades {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        if self.forks.is_empty() {
            return ForkCondition::Never;
        }

        let forks_len = self.forks.len();
        // check index out of bounds
        match fork {
            Shanghai if forks_len <= Canyon.idx() => ForkCondition::Never,
            Cancun if forks_len <= Ecotone.idx() => ForkCondition::Never,
            Prague if forks_len <= Isthmus.idx() => ForkCondition::Never,
            Osaka if forks_len <= V1.idx() => ForkCondition::Never,
            _ => self[fork],
        }
    }
}

impl BaseUpgrades for BaseChainUpgrades {
    fn upgrade_activation(&self, fork: BaseUpgrade) -> ForkCondition {
        // check index out of bounds
        if self.forks.len() <= fork.idx() {
            return ForkCondition::Never;
        }
        self[fork]
    }
}

impl Index<BaseUpgrade> for BaseChainUpgrades {
    type Output = ForkCondition;

    fn index(&self, hf: BaseUpgrade) -> &Self::Output {
        match hf {
            Bedrock => &self.forks[Bedrock.idx()].1,
            Regolith => &self.forks[Regolith.idx()].1,
            Canyon => &self.forks[Canyon.idx()].1,
            Ecotone => &self.forks[Ecotone.idx()].1,
            Fjord => &self.forks[Fjord.idx()].1,
            Granite => &self.forks[Granite.idx()].1,
            Holocene => &self.forks[Holocene.idx()].1,
            Isthmus => &self.forks[Isthmus.idx()].1,
            Jovian => &self.forks[Jovian.idx()].1,
            V1 => &self.forks[V1.idx()].1,
        }
    }
}

impl Index<EthereumHardfork> for BaseChainUpgrades {
    type Output = ForkCondition;

    fn index(&self, hf: EthereumHardfork) -> &Self::Output {
        match hf {
            // Dao Hardfork is not needed for BaseChainUpgrades
            Dao | Bpo1 | Bpo2 | Bpo3 | Bpo4 | Bpo5 | Amsterdam => &ForkCondition::Never,
            Frontier | Homestead | Tangerine | SpuriousDragon | Byzantium | Constantinople
            | Petersburg | Istanbul | MuirGlacier | Berlin => &ForkCondition::ZERO_BLOCK,
            London | ArrowGlacier | GrayGlacier => &self[Bedrock],
            Paris => &ForkCondition::TTD {
                activation_block_number: 0,
                fork_block: Some(0),
                total_difficulty: U256::ZERO,
            },
            Shanghai => &self[Canyon],
            Cancun => &self[Ecotone],
            Prague => &self[Isthmus],
            Osaka => &self[V1],
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use BaseUpgrade::{
        Bedrock, Canyon, Ecotone, Fjord, Granite, Holocene, Isthmus, Jovian, Regolith, V1,
    };
    use alloy_hardforks::EthereumHardfork;

    use super::*;
    use crate::BaseChainConfig;

    #[test]
    fn base_mainnet_fork_conditions() {
        let base_mainnet_forks = BaseChainUpgrades::mainnet();
        assert_eq!(
            base_mainnet_forks[Bedrock],
            ForkCondition::Block(BaseChainConfig::mainnet().bedrock_block)
        );
        assert_eq!(
            base_mainnet_forks[Regolith],
            ForkCondition::Timestamp(BaseChainConfig::mainnet().regolith_timestamp)
        );
        assert_eq!(
            base_mainnet_forks[Canyon],
            ForkCondition::Timestamp(BaseChainConfig::mainnet().canyon_timestamp)
        );
        assert_eq!(
            base_mainnet_forks[Ecotone],
            ForkCondition::Timestamp(BaseChainConfig::mainnet().ecotone_timestamp)
        );
        assert_eq!(
            base_mainnet_forks[Fjord],
            ForkCondition::Timestamp(BaseChainConfig::mainnet().fjord_timestamp)
        );
        assert_eq!(
            base_mainnet_forks[Granite],
            ForkCondition::Timestamp(BaseChainConfig::mainnet().granite_timestamp)
        );
        assert_eq!(
            base_mainnet_forks[Holocene],
            ForkCondition::Timestamp(BaseChainConfig::mainnet().holocene_timestamp)
        );
        assert_eq!(
            base_mainnet_forks[Isthmus],
            ForkCondition::Timestamp(BaseChainConfig::mainnet().isthmus_timestamp)
        );
        assert_eq!(
            base_mainnet_forks[Jovian],
            ForkCondition::Timestamp(BaseChainConfig::mainnet().jovian_timestamp)
        );
        assert_eq!(base_mainnet_forks[V1], ForkCondition::Never);
    }

    #[test]
    fn base_sepolia_fork_conditions() {
        let base_sepolia_forks = BaseChainUpgrades::sepolia();
        assert_eq!(
            base_sepolia_forks[Bedrock],
            ForkCondition::Block(BaseChainConfig::sepolia().bedrock_block)
        );
        assert_eq!(
            base_sepolia_forks[Regolith],
            ForkCondition::Timestamp(BaseChainConfig::sepolia().regolith_timestamp)
        );
        assert_eq!(
            base_sepolia_forks[Canyon],
            ForkCondition::Timestamp(BaseChainConfig::sepolia().canyon_timestamp)
        );
        assert_eq!(
            base_sepolia_forks[Ecotone],
            ForkCondition::Timestamp(BaseChainConfig::sepolia().ecotone_timestamp)
        );
        assert_eq!(
            base_sepolia_forks[Fjord],
            ForkCondition::Timestamp(BaseChainConfig::sepolia().fjord_timestamp)
        );
        assert_eq!(
            base_sepolia_forks[Granite],
            ForkCondition::Timestamp(BaseChainConfig::sepolia().granite_timestamp)
        );
        assert_eq!(
            base_sepolia_forks[Holocene],
            ForkCondition::Timestamp(BaseChainConfig::sepolia().holocene_timestamp)
        );
        assert_eq!(
            base_sepolia_forks[Isthmus],
            ForkCondition::Timestamp(BaseChainConfig::sepolia().isthmus_timestamp)
        );
        assert_eq!(
            base_sepolia_forks.upgrade_activation(Jovian),
            ForkCondition::Timestamp(BaseChainConfig::sepolia().jovian_timestamp)
        );
        assert_eq!(
            base_sepolia_forks[V1],
            ForkCondition::Timestamp(BaseChainConfig::sepolia().base_v1_timestamp.unwrap())
        );
    }

    #[test]
    fn is_jovian_active_at_timestamp() {
        let base_mainnet_forks = BaseChainUpgrades::mainnet();
        assert!(
            base_mainnet_forks
                .is_jovian_active_at_timestamp(BaseChainConfig::mainnet().jovian_timestamp)
        );
        assert!(
            !base_mainnet_forks
                .is_jovian_active_at_timestamp(BaseChainConfig::mainnet().jovian_timestamp - 1)
        );
        assert!(
            base_mainnet_forks
                .is_jovian_active_at_timestamp(BaseChainConfig::mainnet().jovian_timestamp + 1000)
        );

        let base_sepolia_forks = BaseChainUpgrades::sepolia();
        assert!(
            base_sepolia_forks
                .is_jovian_active_at_timestamp(BaseChainConfig::sepolia().jovian_timestamp)
        );
        assert!(
            !base_sepolia_forks
                .is_jovian_active_at_timestamp(BaseChainConfig::sepolia().jovian_timestamp - 1)
        );
        assert!(
            base_sepolia_forks
                .is_jovian_active_at_timestamp(BaseChainConfig::sepolia().jovian_timestamp + 1000)
        );
    }

    #[test]
    fn is_base_v1_active_at_timestamp() {
        // V1 is not scheduled on mainnet yet (ForkCondition::Never)
        let base_mainnet_forks = BaseChainUpgrades::mainnet();
        assert!(!base_mainnet_forks.is_base_v1_active_at_timestamp(0));
        assert!(!base_mainnet_forks.is_base_v1_active_at_timestamp(u64::MAX));

        // V1 is scheduled on sepolia at 1776708000
        let base_sepolia_forks = BaseChainUpgrades::sepolia();
        assert!(!base_sepolia_forks.is_base_v1_active_at_timestamp(0));
        assert!(!base_sepolia_forks.is_base_v1_active_at_timestamp(1_776_707_999));
        assert!(base_sepolia_forks.is_base_v1_active_at_timestamp(1_776_708_000));
        assert!(base_sepolia_forks.is_base_v1_active_at_timestamp(u64::MAX));

        // V1 is active at genesis on devnet (ForkCondition::ZERO_TIMESTAMP)
        let devnet_forks = BaseChainUpgrades::devnet();
        assert!(devnet_forks.is_base_v1_active_at_timestamp(0));

        // V1 is scheduled on devnet-0-sepolia-dev-0 at 1774890000
        let devnet0_forks = BaseChainUpgrades::base_devnet_0_sepolia_dev_0();
        assert!(!devnet0_forks.is_base_v1_active_at_timestamp(0));
        assert!(!devnet0_forks.is_base_v1_active_at_timestamp(1_774_889_999));
        assert!(devnet0_forks.is_base_v1_active_at_timestamp(1_774_890_000));
        assert!(devnet0_forks.is_base_v1_active_at_timestamp(u64::MAX));

        // V1 is scheduled on zeronet at 1775152800
        let zeronet_forks = BaseChainUpgrades::zeronet();
        assert!(!zeronet_forks.is_base_v1_active_at_timestamp(0));
        assert!(!zeronet_forks.is_base_v1_active_at_timestamp(1_775_152_799));
        assert!(zeronet_forks.is_base_v1_active_at_timestamp(1_775_152_800));
        assert!(zeronet_forks.is_base_v1_active_at_timestamp(u64::MAX));
    }

    #[test]
    fn osaka_tracks_base_v1_activation() {
        let base_mainnet_forks = BaseChainUpgrades::mainnet();
        assert_eq!(
            base_mainnet_forks.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Never
        );

        let base_sepolia_forks = BaseChainUpgrades::sepolia();
        assert_eq!(
            base_sepolia_forks.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Timestamp(1_776_708_000)
        );

        let devnet_forks = BaseChainUpgrades::devnet();
        assert_eq!(
            devnet_forks.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::ZERO_TIMESTAMP
        );

        let devnet0_forks = BaseChainUpgrades::base_devnet_0_sepolia_dev_0();
        assert_eq!(
            devnet0_forks.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Timestamp(1_774_890_000)
        );

        let zeronet_forks = BaseChainUpgrades::zeronet();
        assert_eq!(
            zeronet_forks.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Timestamp(1_775_152_800)
        );
    }

    #[test]
    fn test_ethereum_fork_activation_consistency() {
        let base_mainnet_forks = BaseChainUpgrades::mainnet();
        for ethereum_hardfork in EthereumHardfork::VARIANTS {
            let _ = base_mainnet_forks.ethereum_fork_activation(*ethereum_hardfork);
        }
        for base_hardfork in BaseUpgrade::VARIANTS {
            let _ = base_mainnet_forks.upgrade_activation(*base_hardfork);
        }
    }
}
