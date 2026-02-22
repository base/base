//! Module containing the chain config.

/// Base Mainnet chain ID.
pub const BASE_MAINNET_CHAIN_ID: u64 = 8453;

/// Base Sepolia chain ID.
pub const BASE_SEPOLIA_CHAIN_ID: u64 = 84532;

mod addresses;
pub use addresses::AddressList;

mod config;
pub use config::{ChainConfig, L1ChainConfig};

mod hardfork;
pub use hardfork::HardForkConfig;

mod roles;
pub use roles::Roles;
