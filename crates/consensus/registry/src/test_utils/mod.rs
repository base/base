//! Test-only module providing rollup configs derived from [`ChainConfig`].

use base_common_chains::ChainConfig;
use base_consensus_genesis::RollupConfig;
use spin::Lazy;

/// The [`RollupConfig`] for Base Mainnet, derived from [`ChainConfig::mainnet`].
pub static BASE_MAINNET_ROLLUP_CONFIG: Lazy<RollupConfig> =
    Lazy::new(|| RollupConfig::from(ChainConfig::mainnet()));

/// The [`RollupConfig`] for Base Sepolia, derived from [`ChainConfig::sepolia`].
pub static BASE_SEPOLIA_ROLLUP_CONFIG: Lazy<RollupConfig> =
    Lazy::new(|| RollupConfig::from(ChainConfig::sepolia()));
