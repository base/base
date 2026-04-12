//! Test-only module providing rollup configs derived from [`BaseChainConfig`].

use base_common_chains::BaseChainConfig;
use base_consensus_genesis::RollupConfig;
use spin::Lazy;

/// The [`RollupConfig`] for Base Mainnet, derived from [`BaseChainConfig::mainnet`].
pub static BASE_MAINNET_ROLLUP_CONFIG: Lazy<RollupConfig> =
    Lazy::new(|| RollupConfig::from(BaseChainConfig::mainnet()));

/// The [`RollupConfig`] for Base Sepolia, derived from [`BaseChainConfig::sepolia`].
pub static BASE_SEPOLIA_ROLLUP_CONFIG: Lazy<RollupConfig> =
    Lazy::new(|| RollupConfig::from(BaseChainConfig::sepolia()));
