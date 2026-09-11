//! Test-only module providing rollup configs derived from [`ChainConfig`].

use spin::Lazy;

use crate::{ChainConfig, RollupConfig};

/// The [`RollupConfig`] for Base Mainnet, derived from [`ChainConfig::mainnet`].
pub static BASE_MAINNET_ROLLUP_CONFIG: Lazy<RollupConfig> =
    Lazy::new(|| ChainConfig::MAINNET.rollup_config());

/// The [`RollupConfig`] for Base Sepolia, derived from [`ChainConfig::sepolia`].
pub static BASE_SEPOLIA_ROLLUP_CONFIG: Lazy<RollupConfig> =
    Lazy::new(|| ChainConfig::SEPOLIA.rollup_config());
