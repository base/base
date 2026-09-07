//! Abstraction over configuration object for [`super::EthBlockExecutor`].

use alloy_eips::eip6110::MAINNET_DEPOSIT_CONTRACT_ADDRESS;
use alloy_hardforks::{EthereumChainHardforks, EthereumHardfork, EthereumHardforks, ForkCondition};
use alloy_primitives::{address, Address};

/// A configuration object for [`super::EthBlockExecutor`]
#[auto_impl::auto_impl(&, Arc)]
pub trait EthExecutorSpec: EthereumHardforks {
    /// Address of deposit contract emitting deposit events.
    ///
    /// Used by [`super::eip6110::parse_deposits_from_receipts`].
    fn deposit_contract_address(&self) -> Option<Address>;
}

/// Basic Ethereum specification.
#[derive(Debug, Clone)]
pub struct EthSpec {
    hardforks: EthereumChainHardforks,
    deposit_contract_address: Option<Address>,
}

impl EthSpec {
    /// Creates [`EthSpec`] for Ethereum mainnet.
    pub fn mainnet() -> Self {
        Self {
            hardforks: EthereumChainHardforks::mainnet(),
            deposit_contract_address: Some(MAINNET_DEPOSIT_CONTRACT_ADDRESS),
        }
    }

    /// Creates [`EthSpec`] for Ethereum Sepolia.
    pub fn sepolia() -> Self {
        Self {
            hardforks: EthereumChainHardforks::sepolia(),
            deposit_contract_address: Some(address!("0x7f02c3e3c98b133055b8b348b2ac625669ed295d")),
        }
    }

    /// Creates [`EthSpec`] for Ethereum Holesky.
    pub fn holesky() -> Self {
        Self {
            hardforks: EthereumChainHardforks::holesky(),
            deposit_contract_address: Some(address!("0x4242424242424242424242424242424242424242")),
        }
    }
}

impl EthereumHardforks for EthSpec {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.hardforks.ethereum_fork_activation(fork)
    }
}

impl EthExecutorSpec for EthSpec {
    fn deposit_contract_address(&self) -> Option<Address> {
        self.deposit_contract_address
    }
}
