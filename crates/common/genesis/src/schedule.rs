use core::ops::Index;

// Production imports for upgrade implementations
use EthereumHardfork::{
    Amsterdam, ArrowGlacier, Berlin, Bpo1, Bpo2, Bpo3, Bpo4, Bpo5, Byzantium, Constantinople, Dao,
    Frontier, GrayGlacier, Homestead, Istanbul, London, MuirGlacier, Paris, Petersburg,
    SpuriousDragon, Tangerine,
};
use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};

use crate::{BaseUpgrade, ExecutionFork, RuntimeUpgradeRegistry, UpgradeActivation, UpgradeConfig};

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainUpgrades {
    /// Conditions indexed by the canonical Base upgrade enum.
    pub forks: [ForkCondition; BaseUpgrade::VARIANTS.len()],
}

impl ChainUpgrades {
    /// Creates a typed schedule. Unspecified upgrades are never activated.
    pub fn new(forks: impl IntoIterator<Item = (BaseUpgrade, ForkCondition)>) -> Self {
        let mut schedule =
            Self::from_conditions([ForkCondition::Never; BaseUpgrade::VARIANTS.len()]);
        for (upgrade, condition) in forks {
            schedule.insert(upgrade, condition);
        }
        schedule
    }

    /// Constructs a compile-time schedule in [`BaseUpgrade::VARIANTS`] order.
    pub const fn from_conditions(forks: [ForkCondition; BaseUpgrade::VARIANTS.len()]) -> Self {
        Self { forks }
    }

    /// Iterates over the configured Base execution ladder.
    pub fn iter(&self) -> impl Iterator<Item = (BaseUpgrade, ForkCondition)> + '_ {
        BaseUpgrade::EXECUTION_VARIANTS
            .into_iter()
            .map(|fork| (fork, self[fork]))
            .chain(core::iter::once((BaseUpgrade::Zenith, self[BaseUpgrade::Zenith])))
    }

    /// Looks up a typed execution rule.
    pub fn fork(&self, fork: impl Into<ExecutionFork>) -> ForkCondition {
        match fork.into() {
            ExecutionFork::Base(fork) => self[fork],
            ExecutionFork::Ethereum(fork) => self[fork],
        }
    }

    /// Updates a Base upgrade, including the corresponding Ethereum rule.
    pub fn insert(&mut self, fork: BaseUpgrade, condition: ForkCondition) {
        self.forks[fork as usize] = condition;
    }

    /// Removes a configured Base activation.
    pub fn remove(&mut self, fork: &BaseUpgrade) {
        self.insert(*fork, ForkCondition::Never);
    }

    /// Looks up a scheduled execution rule.
    pub fn get(&self, fork: impl Into<ExecutionFork>) -> Option<ForkCondition> {
        let condition = self.fork(fork);
        (condition != ForkCondition::Never).then_some(condition)
    }

    /// Iterates over derived Ethereum rules and Base upgrades in protocol order.
    pub fn forks_iter(&self) -> impl Iterator<Item = (ExecutionFork, ForkCondition)> + '_ {
        EthereumHardfork::VARIANTS
            .iter()
            .copied()
            .take_while(|fork| *fork != EthereumHardfork::Shanghai)
            .map(|fork| (ExecutionFork::Ethereum(fork), self[fork]))
            .chain(self.iter().flat_map(|(fork, condition)| {
                fork.execution_hardfork()
                    .map(|eth| (ExecutionFork::Ethereum(eth), condition))
                    .into_iter()
                    .chain(core::iter::once((ExecutionFork::Base(fork), condition)))
            }))
            .filter(|(_, condition)| *condition != ForkCondition::Never)
    }

    /// Resolves one execution rule using the current runtime activation registry.
    pub fn activation(&self, chain_id: u64, fork: impl Into<ExecutionFork>) -> ForkCondition {
        let fork = fork.into();
        if let Some(upgrade) =
            fork.base_upgrade().filter(|upgrade| BaseUpgrade::CONTRACT_VARIANTS.contains(upgrade))
        {
            if let Some(activation) = RuntimeUpgradeRegistry::activation(chain_id, upgrade) {
                return match activation {
                    UpgradeActivation::Never => ForkCondition::Never,
                    UpgradeActivation::Timestamp(timestamp) => ForkCondition::Timestamp(timestamp),
                };
            }
        }
        self.fork(fork)
    }

    /// Takes one consistent snapshot of the configured schedule and runtime overrides.
    pub fn runtime(&self, chain_id: u64) -> Self {
        let mut schedule = self.clone();
        if let Some(overrides) = RuntimeUpgradeRegistry::overrides(chain_id) {
            for (upgrade, activation) in overrides.activations {
                if BaseUpgrade::CONTRACT_VARIANTS.contains(&upgrade) {
                    schedule.insert(
                        upgrade,
                        match activation {
                            UpgradeActivation::Never => ForkCondition::Never,
                            UpgradeActivation::Timestamp(timestamp) => {
                                ForkCondition::Timestamp(timestamp)
                            }
                        },
                    );
                }
            }
        }
        schedule
    }
}

impl EthereumHardforks for ChainUpgrades {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self[fork]
    }
}

impl Index<BaseUpgrade> for ChainUpgrades {
    type Output = ForkCondition;

    fn index(&self, hf: BaseUpgrade) -> &Self::Output {
        &self.forks[hf as usize]
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
            Paris => &ForkCondition::ZERO_BLOCK,
            _ => unreachable!(),
        }
    }
}

impl Default for ChainUpgrades {
    fn default() -> Self {
        Self::new([])
    }
}

impl From<&UpgradeConfig> for ChainUpgrades {
    fn from(config: &UpgradeConfig) -> Self {
        let mut schedule = Self::new(BaseUpgrade::VARIANTS.iter().copied().map(|fork| {
            (
                fork,
                config.activation_timestamp(fork).map(ForkCondition::Timestamp).unwrap_or_default(),
            )
        }));
        schedule.insert(BaseUpgrade::Bedrock, ForkCondition::Block(0));
        // Older rollup boundary documents may omit earlier upgrades. Normalize that legacy
        // cascade once, before explicit runtime overrides (including Never) are applied.
        let mut next = ForkCondition::Never;
        for fork in [
            BaseUpgrade::Jovian,
            BaseUpgrade::Isthmus,
            BaseUpgrade::Holocene,
            BaseUpgrade::Granite,
            BaseUpgrade::Fjord,
            BaseUpgrade::Ecotone,
            BaseUpgrade::Canyon,
            BaseUpgrade::Regolith,
        ] {
            if schedule[fork] == ForkCondition::Never {
                schedule.insert(fork, next);
            }
            next = schedule[fork];
        }
        schedule
    }
}
