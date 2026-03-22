#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![no_std]

extern crate alloc;

mod config;
pub use config::BaseChainConfig;

mod hardfork;
pub use hardfork::BaseUpgrade;

mod hardforks;
pub use hardforks::BaseUpgrades;

mod chain;
pub use chain::BaseChainUpgrades;
