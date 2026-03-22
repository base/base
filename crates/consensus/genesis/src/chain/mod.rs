//! Module containing the chain config.

mod addresses;
pub use addresses::AddressList;

mod config;
pub use config::{ChainConfig, L1ChainConfig};

mod hardfork;
pub use hardfork::{BaseHardforkConfig, HardForkConfig};

mod roles;
pub use roles::Roles;
