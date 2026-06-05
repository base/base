#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![no_std]

use alloy_genesis::ChainConfig as GenesisChainConfig;
use alloy_primitives::map::HashMap;
use spin::Lazy;

extern crate alloc;

mod config;
pub use config::{Bootnodes, ChainConfig};

mod upgrade;
pub use upgrade::BaseUpgrade;

mod upgrades;
pub use upgrades::Upgrades;

mod chain;
pub use chain::ChainUpgrades;

mod macros;
pub use macros::RollupConfigSource;

mod ethereum;
pub use ethereum::{Holesky, Hoodi, Mainnet, Sepolia};

mod base;
pub use base::{BaseMainnet, BaseSepolia};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

/// L1 chain configurations keyed by chain ID.
///
/// Covers the Ethereum networks that Base settles to, plus the Base networks
/// themselves, so they are recognized as parent ("L1") chains for an L3.
pub static L1_CONFIGS: Lazy<HashMap<u64, GenesisChainConfig>> =
    Lazy::new(|| ethereum::config::l1_configs().chain(base::config::l1_configs()).collect());
