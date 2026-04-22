//! Test-only module providing rollup configs derived from [`ChainConfig`].

use base_common_genesis::RollupConfig;
use spin::Lazy;

use crate::ChainConfig;

/// The [`RollupConfig`] for Base Mainnet, derived from [`ChainConfig::mainnet`].
pub static BASE_MAINNET_ROLLUP_CONFIG: Lazy<RollupConfig> =
    Lazy::new(|| ChainConfig::mainnet().rollup_config());

/// The [`RollupConfig`] for Base Sepolia, derived from [`ChainConfig::sepolia`].
pub static BASE_SEPOLIA_ROLLUP_CONFIG: Lazy<RollupConfig> =
    Lazy::new(|| ChainConfig::sepolia().rollup_config());
