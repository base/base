//! Module containing the chain config.

mod addresses;
pub use addresses::AddressList;

mod hardfork;
pub use hardfork::{HardForkConfig, HardforkConfig};

mod roles;
pub use roles::Roles;
