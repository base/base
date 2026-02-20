use alloc::vec::Vec;
use core::ops::Index;

use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
use alloy_primitives::U256;

use crate::{OpHardfork, OpHardforks};

/// A type allowing to configure activation [`ForkCondition`]s for a given list of
/// [`OpHardfork`]s.
///
/// Zips together [`EthereumHardfork`]s and [`OpHardfork`]s. Optimism hard forks, at least,
/// whenever Ethereum hard forks. When Ethereum hard forks, a new [`OpHardfork`] piggybacks on top
/// of the new [`EthereumHardfork`] to include (or to noop) the L1 changes on L2.
///
/// Optimism can also hard fork independently of Ethereum. The relation between Ethereum and
/// Optimism hard forks is described by predicate [`EthereumHardfork`] `=>` [`OpHardfork`], since
/// an OP chain can undergo an [`OpHardfork`] without an [`EthereumHardfork`], but not the other
/// way around.
#[derive(Debug, Clone)]
pub struct OpChainHardforks {
    /// Ordered list of OP hardfork activations.
    forks: Vec<(OpHardfork, ForkCondition)>,
}

impl OpChainHardforks {
    /// Creates a new [`OpChainHardforks`] with the given list of forks. The input list is sorted
    /// w.r.t. the hardcoded canonicity of [`OpHardfork`]s.
    pub fn new(forks: impl IntoIterator<Item = (OpHardfork, ForkCondition)>) -> Self {
        let mut forks = forks.into_iter().collect::<Vec<_>>();
        forks.sort();
        Self { forks }
    }

    /// Creates a new [`OpChainHardforks`] with Base mainnet configuration.
    pub fn base_mainnet() -> Self {
        Self::new(OpHardfork::base_mainnet())
    }

    /// Creates a new [`OpChainHardforks`] with Base Sepolia configuration.
    pub fn base_sepolia() -> Self {
        Self::new(OpHardfork::base_sepolia())
    }

    /// Creates a new [`OpChainHardforks`] with devnet configuration.
    pub fn devnet() -> Self {
        Self::new(OpHardfork::devnet())
    }
}

impl EthereumHardforks for OpChainHardforks {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        use EthereumHardfork::{Cancun, Prague, Shanghai};
        use OpHardfork::{Canyon, Ecotone, Isthmus};

        if self.forks.is_empty() {
            return ForkCondition::Never;
        }

        let forks_len = self.forks.len();
        // check index out of bounds
        match fork {
            Shanghai if forks_len <= Canyon.idx() => ForkCondition::Never,
            Cancun if forks_len <= Ecotone.idx() => ForkCondition::Never,
            Prague if forks_len <= Isthmus.idx() => ForkCondition::Never,
            _ => self[fork],
        }
    }
}

impl OpHardforks for OpChainHardforks {
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition {
        // check index out of bounds
        if self.forks.len() <= fork.idx() {
            return ForkCondition::Never;
        }
        self[fork]
    }
}

impl Index<OpHardfork> for OpChainHardforks {
    type Output = ForkCondition;

    fn index(&self, hf: OpHardfork) -> &Self::Output {
        use OpHardfork::{
            Bedrock, Canyon, Ecotone, Fjord, Granite, Holocene, Isthmus, Jovian, Regolith,
        };

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
        }
    }
}

impl Index<EthereumHardfork> for OpChainHardforks {
    type Output = ForkCondition;

    fn index(&self, hf: EthereumHardfork) -> &Self::Output {
        use EthereumHardfork::{
            Amsterdam, ArrowGlacier, Berlin, Bpo1, Bpo2, Bpo3, Bpo4, Bpo5, Byzantium, Cancun,
            Constantinople, Dao, Frontier, GrayGlacier, Homestead, Istanbul, London, MuirGlacier,
            Osaka, Paris, Petersburg, Prague, Shanghai, SpuriousDragon, Tangerine,
        };
        use OpHardfork::{Bedrock, Canyon, Ecotone, Isthmus};

        match hf {
            // Dao Hardfork is not needed for OpChainHardforks
            Dao | Osaka | Bpo1 | Bpo2 | Bpo3 | Bpo4 | Bpo5 | Amsterdam => &ForkCondition::Never,
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
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_hardforks::EthereumHardfork;

    use super::*;
    use crate::*;

    #[test]
    fn base_mainnet_fork_conditions() {
        use OpHardfork::*;

        let base_mainnet_forks = OpChainHardforks::base_mainnet();
        assert_eq!(base_mainnet_forks[Bedrock], ForkCondition::Block(BASE_MAINNET_BEDROCK_BLOCK));
        assert_eq!(
            base_mainnet_forks[Regolith],
            ForkCondition::Timestamp(BASE_MAINNET_REGOLITH_TIMESTAMP)
        );
        assert_eq!(
            base_mainnet_forks[Canyon],
            ForkCondition::Timestamp(BASE_MAINNET_CANYON_TIMESTAMP)
        );
        assert_eq!(
            base_mainnet_forks[Ecotone],
            ForkCondition::Timestamp(BASE_MAINNET_ECOTONE_TIMESTAMP)
        );
        assert_eq!(
            base_mainnet_forks[Fjord],
            ForkCondition::Timestamp(BASE_MAINNET_FJORD_TIMESTAMP)
        );
        assert_eq!(
            base_mainnet_forks[Granite],
            ForkCondition::Timestamp(BASE_MAINNET_GRANITE_TIMESTAMP)
        );
        assert_eq!(
            base_mainnet_forks[Holocene],
            ForkCondition::Timestamp(BASE_MAINNET_HOLOCENE_TIMESTAMP)
        );
        assert_eq!(
            base_mainnet_forks[Isthmus],
            ForkCondition::Timestamp(BASE_MAINNET_ISTHMUS_TIMESTAMP)
        );
        assert_eq!(
            base_mainnet_forks[Jovian],
            ForkCondition::Timestamp(BASE_MAINNET_JOVIAN_TIMESTAMP)
        );
    }

    #[test]
    fn base_sepolia_fork_conditions() {
        use OpHardfork::*;

        let base_sepolia_forks = OpChainHardforks::base_sepolia();
        assert_eq!(base_sepolia_forks[Bedrock], ForkCondition::Block(BASE_SEPOLIA_BEDROCK_BLOCK));
        assert_eq!(
            base_sepolia_forks[Regolith],
            ForkCondition::Timestamp(BASE_SEPOLIA_REGOLITH_TIMESTAMP)
        );
        assert_eq!(
            base_sepolia_forks[Canyon],
            ForkCondition::Timestamp(BASE_SEPOLIA_CANYON_TIMESTAMP)
        );
        assert_eq!(
            base_sepolia_forks[Ecotone],
            ForkCondition::Timestamp(BASE_SEPOLIA_ECOTONE_TIMESTAMP)
        );
        assert_eq!(
            base_sepolia_forks[Fjord],
            ForkCondition::Timestamp(BASE_SEPOLIA_FJORD_TIMESTAMP)
        );
        assert_eq!(
            base_sepolia_forks[Granite],
            ForkCondition::Timestamp(BASE_SEPOLIA_GRANITE_TIMESTAMP)
        );
        assert_eq!(
            base_sepolia_forks[Holocene],
            ForkCondition::Timestamp(BASE_SEPOLIA_HOLOCENE_TIMESTAMP)
        );
        assert_eq!(
            base_sepolia_forks[Isthmus],
            ForkCondition::Timestamp(BASE_SEPOLIA_ISTHMUS_TIMESTAMP)
        );
        assert_eq!(
            base_sepolia_forks.op_fork_activation(Jovian),
            ForkCondition::Timestamp(BASE_SEPOLIA_JOVIAN_TIMESTAMP)
        );
    }

    #[test]
    fn is_jovian_active_at_timestamp() {
        let base_mainnet_forks = OpChainHardforks::base_mainnet();
        assert!(base_mainnet_forks.is_jovian_active_at_timestamp(BASE_MAINNET_JOVIAN_TIMESTAMP));
        assert!(
            !base_mainnet_forks.is_jovian_active_at_timestamp(BASE_MAINNET_JOVIAN_TIMESTAMP - 1)
        );
        assert!(
            base_mainnet_forks.is_jovian_active_at_timestamp(BASE_MAINNET_JOVIAN_TIMESTAMP + 1000)
        );

        let base_sepolia_forks = OpChainHardforks::base_sepolia();
        assert!(base_sepolia_forks.is_jovian_active_at_timestamp(BASE_SEPOLIA_JOVIAN_TIMESTAMP));
        assert!(
            !base_sepolia_forks.is_jovian_active_at_timestamp(BASE_SEPOLIA_JOVIAN_TIMESTAMP - 1)
        );
        assert!(
            base_sepolia_forks.is_jovian_active_at_timestamp(BASE_SEPOLIA_JOVIAN_TIMESTAMP + 1000)
        );
    }

    #[test]
    fn test_ethereum_fork_activation_consistency() {
        let base_mainnet_forks = OpChainHardforks::base_mainnet();
        for ethereum_hardfork in EthereumHardfork::VARIANTS {
            let _ = base_mainnet_forks.ethereum_fork_activation(*ethereum_hardfork);
        }
        for op_hardfork in OpHardfork::VARIANTS {
            let _ = base_mainnet_forks.op_fork_activation(*op_hardfork);
        }
    }
}
