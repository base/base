use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use alloy_chains::Chain;
use base_common_chains::ChainConfig;
use base_consensus_cli::ConsensusChainArgs;
use base_execution_chainspec::BaseChainSpec;
use eyre::WrapErr;
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};

/// Prefix for chain configuration environment variables.
pub(crate) const BASE_CHAIN_ENV_PREFIX: &str = "BASE_CHAIN_";

/// Chain selection for `--chain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChainArg {
    /// Use one of the built-in static chains, identified by its Base network
    /// selector (`mainnet`, `sepolia`, `zeronet`, `dev`). Legacy namespaced
    /// names (`base`, `base-sepolia`, `base-zeronet`) are accepted for backwards
    /// compatibility and normalized to the selector.
    BuiltIn(String),
    /// Load chain settings from a TOML file.
    File(PathBuf),
}

impl Default for ChainArg {
    fn default() -> Self {
        Self::BuiltIn("mainnet".to_owned())
    }
}

impl FromStr for ChainArg {
    type Err = std::convert::Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let selector = value.to_ascii_lowercase();
        // Accept the Base-centric selectors (`mainnet`/`sepolia`/`zeronet`/`dev`), the legacy
        // namespaced names (`base`/`base-sepolia`/`base-zeronet`/`dev`), and built-in L2 chain
        // IDs, normalizing all of them to the canonical selector. Anything unrecognized is
        // treated as a path to a TOML chain config file.
        let config = ChainConfig::by_any_name(&selector)
            .or_else(|| value.parse::<u64>().ok().and_then(ChainConfig::by_chain_id));
        Ok(config.and_then(ChainConfig::base_chain_selector).map_or_else(
            || Self::File(PathBuf::from(value)),
            |selector| Self::BuiltIn(selector.to_owned()),
        ))
    }
}

/// The resolved chain config used by the `base` binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResolvedChainConfig {
    /// Human-readable chain name.
    pub(crate) name: String,
    /// L2 chain ID.
    pub(crate) l2_chain_id: u64,
    /// L1 chain ID.
    pub(crate) l1_chain_id: u64,
}

impl ResolvedChainConfig {
    /// Returns the execution chainspec for this chain.
    pub(crate) fn execution_chain_spec(&self) -> eyre::Result<Arc<BaseChainSpec>> {
        let config = ChainConfig::by_chain_id(self.l2_chain_id).ok_or_else(|| {
            eyre::eyre!("no built-in execution chainspec for L2 chain ID {}", self.l2_chain_id)
        })?;
        Ok(Arc::new(BaseChainSpec::try_from(config)?))
    }

    /// Returns the consensus chain arguments for this chain.
    pub(crate) fn consensus_chain_args(&self) -> ConsensusChainArgs {
        ConsensusChainArgs { l2_chain_id: Chain::from(self.l2_chain_id) }
    }
}

/// Resolves a chain selection into a concrete config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChainResolver {
    /// The requested chain input.
    pub(crate) chain: ChainArg,
    /// Whether the chain was explicitly supplied through CLI args or env.
    pub(crate) chain_explicitly_set: bool,
}

impl ChainResolver {
    /// Creates a new chain resolver.
    pub(crate) fn new(chain: Option<ChainArg>) -> Self {
        let chain_explicitly_set = chain.is_some();
        Self { chain: chain.unwrap_or_default(), chain_explicitly_set }
    }

    /// Rejects top-level chain selectors for commands that manage their own chain configuration.
    pub(crate) fn reject_for_reth_command(&self, command: &str) -> eyre::Result<()> {
        if !self.chain_explicitly_set {
            return Ok(());
        }

        eyre::bail!(
            "`base --chain`/`BASE_CHAIN` only applies to integrated node commands; `{command}` manages its own chain configuration"
        );
    }

    /// Resolves the configured chain input.
    pub(crate) fn resolve(&self) -> eyre::Result<ResolvedChainConfig> {
        match &self.chain {
            ChainArg::BuiltIn(chain) => {
                let config = ChainConfig::from_base_chain(chain)
                    .ok_or_else(|| eyre::eyre!("unknown built-in chain `{chain}`"))?;
                let defaults = ResolvedChainConfig {
                    name: chain.clone(),
                    l2_chain_id: config.chain_id,
                    l1_chain_id: config.l1_chain_id,
                };
                let figment = Figment::from(Serialized::defaults(defaults))
                    .merge(Env::prefixed(BASE_CHAIN_ENV_PREFIX));
                Self::extract(figment, ChainArg::BuiltIn(chain.clone()))
            }
            ChainArg::File(path) => Self::resolve_file(path),
        }
    }

    /// Resolves a chain config from a TOML file.
    pub(crate) fn resolve_file(path: &Path) -> eyre::Result<ResolvedChainConfig> {
        let figment =
            Figment::new().merge(Toml::file(path)).merge(Env::prefixed(BASE_CHAIN_ENV_PREFIX));
        Self::extract(figment, ChainArg::File(path.to_path_buf()))
    }

    /// Extracts the merged chain config.
    pub(crate) fn extract(figment: Figment, source: ChainArg) -> eyre::Result<ResolvedChainConfig> {
        figment.extract::<ResolvedChainConfig>().wrap_err_with(|| match &source {
            ChainArg::BuiltIn(chain) => {
                format!("failed to resolve chain config for built-in chain `{chain}`")
            }
            ChainArg::File(path) => {
                format!("failed to resolve chain config from {}", path.display())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use figment::Jail;

    use super::*;

    #[allow(clippy::result_large_err)]
    fn with_cleared_env(test: impl FnOnce(&mut Jail) -> figment::Result<()>) {
        Jail::expect_with(|jail| {
            jail.clear_env();
            test(jail)
        });
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn resolves_mainnet_builtin() {
        with_cleared_env(|_| {
            let resolved = ChainResolver::new(Some(ChainArg::BuiltIn("mainnet".to_owned())))
                .resolve()
                .unwrap();

            assert_eq!(resolved.name, "mainnet");
            assert_eq!(resolved.l2_chain_id, 8453);
            assert_eq!(resolved.l1_chain_id, 1);

            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn resolves_sepolia_builtin() {
        with_cleared_env(|_| {
            let resolved = ChainResolver::new(Some(ChainArg::BuiltIn("sepolia".to_owned())))
                .resolve()
                .unwrap();

            assert_eq!(resolved.name, "sepolia");
            assert_eq!(resolved.l2_chain_id, 84532);
            assert_eq!(resolved.l1_chain_id, 11155111);

            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn resolves_zeronet_builtin() {
        with_cleared_env(|_| {
            let resolved = ChainResolver::new(Some(ChainArg::BuiltIn("zeronet".to_owned())))
                .resolve()
                .unwrap();

            assert_eq!(resolved.name, "zeronet");
            assert_eq!(resolved.l2_chain_id, 763360);

            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn resolves_dev_builtin() {
        with_cleared_env(|_| {
            let resolved =
                ChainResolver::new(Some(ChainArg::BuiltIn("dev".to_owned()))).resolve().unwrap();

            assert_eq!(resolved.name, "dev");
            assert_eq!(resolved.l2_chain_id, 84538453);
            assert_eq!(resolved.l1_chain_id, 1337);

            Ok(())
        });
    }

    #[test]
    fn from_str_normalizes_legacy_names_to_selectors() {
        // Legacy namespaced names (the former `base-reth-node` `--chain` surface)
        // normalize to the Base-centric selectors.
        assert_eq!("base".parse::<ChainArg>().unwrap(), ChainArg::BuiltIn("mainnet".to_owned()));
        assert_eq!(
            "base-sepolia".parse::<ChainArg>().unwrap(),
            ChainArg::BuiltIn("sepolia".to_owned())
        );
        assert_eq!(
            "base-zeronet".parse::<ChainArg>().unwrap(),
            ChainArg::BuiltIn("zeronet".to_owned())
        );

        // Base-centric selectors keep resolving as before.
        assert_eq!("mainnet".parse::<ChainArg>().unwrap(), ChainArg::BuiltIn("mainnet".to_owned()));

        // Matching is case-insensitive.
        assert_eq!("BASE".parse::<ChainArg>().unwrap(), ChainArg::BuiltIn("mainnet".to_owned()));

        // Unrecognized values are still treated as TOML file paths.
        assert_eq!(
            "./chain.toml".parse::<ChainArg>().unwrap(),
            ChainArg::File(PathBuf::from("./chain.toml"))
        );
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn resolves_legacy_base_name_to_mainnet() {
        with_cleared_env(|_| {
            let chain: ChainArg = "base".parse().unwrap();
            let resolved = ChainResolver::new(Some(chain)).resolve().unwrap();

            assert_eq!(resolved.name, "mainnet");
            assert_eq!(resolved.l2_chain_id, 8453);
            assert_eq!(resolved.l1_chain_id, 1);

            Ok(())
        });
    }

    #[test]
    fn parses_l2_chain_ids_as_builtin_selectors() {
        assert_eq!("8453".parse::<ChainArg>().unwrap(), ChainArg::BuiltIn("mainnet".to_owned()));
        assert_eq!("84532".parse::<ChainArg>().unwrap(), ChainArg::BuiltIn("sepolia".to_owned()));
        assert_eq!("999".parse::<ChainArg>().unwrap(), ChainArg::File(PathBuf::from("999")));
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn resolves_custom_toml_file() {
        with_cleared_env(|jail| {
            let path = jail.directory().join("chain.toml");
            jail.create_file(
                &path,
                "name = \"custom-chain\"\nl2_chain_id = 999\nl1_chain_id = 11155111\n",
            )?;

            let resolved = ChainResolver::resolve_file(&path).unwrap();

            assert_eq!(resolved.name, "custom-chain");
            assert_eq!(resolved.l2_chain_id, 999);
            assert_eq!(resolved.l1_chain_id, 11155111);

            Ok(())
        });
    }
}
