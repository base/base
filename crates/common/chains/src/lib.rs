#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![no_std]

extern crate alloc;

mod config;
pub use config::ChainConfig;

mod upgrade;
pub use upgrade::BaseUpgrade;

mod upgrades;
pub use upgrades::Upgrades;

mod chain;
pub use chain::ChainUpgrades;

mod derived;

mod registry;
pub use registry::Registry;

mod ethereum;
pub use ethereum::{Holesky, Hoodi, Mainnet, Sepolia};

mod l1_registry;
pub use l1_registry::l1_config;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
