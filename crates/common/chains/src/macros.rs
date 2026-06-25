//! Macros and helper traits for ergonomic chain config access.

use alloy_chains::Chain;
use base_common_genesis::RollupConfig;

use crate::ChainConfig;

/// Input accepted by the [`rollup_config!`] macro.
pub trait RollupConfigSource {
    /// Type returned after resolving the input.
    type Output;

    /// Resolves the input into a derived [`RollupConfig`].
    fn resolve_rollup_config(self) -> Self::Output;
}

impl RollupConfigSource for u64 {
    type Output = Option<RollupConfig>;

    fn resolve_rollup_config(self) -> Self::Output {
        ChainConfig::rollup_config_by_chain_id(self)
    }
}

impl RollupConfigSource for &u64 {
    type Output = Option<RollupConfig>;

    fn resolve_rollup_config(self) -> Self::Output {
        ChainConfig::rollup_config_by_chain_id(*self)
    }
}

impl RollupConfigSource for Chain {
    type Output = Option<RollupConfig>;

    fn resolve_rollup_config(self) -> Self::Output {
        ChainConfig::rollup_config_by_chain(&self)
    }
}

impl RollupConfigSource for &Chain {
    type Output = Option<RollupConfig>;

    fn resolve_rollup_config(self) -> Self::Output {
        ChainConfig::rollup_config_by_chain(self)
    }
}

impl RollupConfigSource for ChainConfig {
    type Output = RollupConfig;

    fn resolve_rollup_config(self) -> Self::Output {
        self.rollup_config()
    }
}

impl RollupConfigSource for &ChainConfig {
    type Output = RollupConfig;

    fn resolve_rollup_config(self) -> Self::Output {
        self.rollup_config()
    }
}

/// Resolves a [`RollupConfig`] from a Base [`ChainConfig`], [`Chain`], or L2 chain ID.
///
/// Chain config inputs resolve directly to [`RollupConfig`]. Chain ID and [`Chain`] inputs resolve
/// to `Option<RollupConfig>` because those inputs may not identify a built-in Base chain.
#[macro_export]
macro_rules! rollup_config {
    ($source:expr $(,)?) => {
        $crate::RollupConfigSource::resolve_rollup_config($source)
    };
}

#[cfg(test)]
mod tests {
    use alloy_chains::Chain;

    use crate::ChainConfig;

    #[test]
    fn rollup_config_macro_accepts_chain_id() {
        let config = crate::rollup_config!(8453).expect("Base mainnet config should resolve");

        assert_eq!(config.l2_chain_id.id(), ChainConfig::mainnet().chain_id);
    }

    #[test]
    fn rollup_config_macro_accepts_chain() {
        let chain = Chain::base_mainnet();
        let config = crate::rollup_config!(&chain).expect("Base mainnet config should resolve");

        assert_eq!(config.l2_chain_id.id(), ChainConfig::mainnet().chain_id);
    }

    #[test]
    fn rollup_config_macro_accepts_chain_config() {
        let config = crate::rollup_config!(ChainConfig::SEPOLIA);

        assert_eq!(config.l2_chain_id.id(), ChainConfig::sepolia().chain_id);
    }

    #[test]
    fn rollup_config_macro_returns_none_for_unknown_chain_id() {
        assert!(crate::rollup_config!(999_999_999).is_none());
    }
}
