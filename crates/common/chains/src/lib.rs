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

#[cfg(feature = "chain-hardforks")]
mod hardforks;
#[cfg(feature = "chain-hardforks")]
pub use hardforks::{
    BASE_DEVNET_0_SEPOLIA_DEV_0_UPGRADES, BASE_MAINNET_UPGRADES, BASE_SEPOLIA_UPGRADES,
    BASE_ZERONET_UPGRADES, ChainUpgradesExt, DEV_UPGRADES,
};
