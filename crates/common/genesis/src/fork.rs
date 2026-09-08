use alloy_hardforks::EthereumHardfork;

use crate::BaseUpgrade;

/// A typed execution rule identifier. Ethereum rules derive their activation from Base upgrades.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionFork {
    /// A Base upgrade.
    Base(BaseUpgrade),
    /// An Ethereum execution rule.
    Ethereum(EthereumHardfork),
}

impl From<BaseUpgrade> for ExecutionFork {
    fn from(fork: BaseUpgrade) -> Self {
        Self::Base(fork)
    }
}
impl From<EthereumHardfork> for ExecutionFork {
    fn from(fork: EthereumHardfork) -> Self {
        Self::Ethereum(fork)
    }
}
impl From<&ExecutionFork> for ExecutionFork {
    fn from(fork: &ExecutionFork) -> Self {
        *fork
    }
}
impl core::fmt::Display for ExecutionFork {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Base(upgrade) => f.write_str(upgrade.name()),
            Self::Ethereum(upgrade) => f.write_str(upgrade.name()),
        }
    }
}
impl ExecutionFork {
    /// Returns the corresponding Base upgrade, when this rule belongs to the execution ladder.
    pub fn base_upgrade(self) -> Option<BaseUpgrade> {
        match self {
            Self::Base(upgrade) => Some(upgrade),
            Self::Ethereum(upgrade) => BaseUpgrade::from_ethereum_hardfork(upgrade),
        }
    }
}
