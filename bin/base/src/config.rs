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

/// CLI input for the root `--chain` flag.
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
        // Accept both the Base-centric selectors (`mainnet`/`sepolia`/`zeronet`/
        // `dev`) and the legacy namespaced names (`base`/`base-sepolia`/
        // `base-zeronet`/`dev`) used by the former split `base-reth-node`,
        // normalizing either to the canonical selector so the rest of the
        // pipeline only ever sees a selector. Anything unrecognized is treated as
        // a path to a TOML chain config file.
        Ok(ChainConfig::from_base_chain(&selector)
            .or_else(|| ChainConfig::by_name(&selector))
            .and_then(ChainConfig::base_chain_selector)
            .map_or_else(
                || Self::File(PathBuf::from(value)),
                |selector| Self::BuiltIn(selector.to_owned()),
            ))
    }
}

/// The concrete source of a resolved chain config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ResolvedChainSource {
    /// The config came from a built-in static chain, identified by its Base
    /// network selector.
    BuiltIn(String),
    /// The config came from a TOML file.
    File(PathBuf),
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
    /// Where this config came from.
    pub(crate) source: ResolvedChainSource,
}

/// The subset of chain settings merged from built-ins, TOML, and env.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResolvedChainValues {
    /// Human-readable chain name.
    pub(crate) name: String,
    /// L2 chain ID.
    pub(crate) l2_chain_id: u64,
    /// L1 chain ID.
    pub(crate) l1_chain_id: u64,
}

impl ResolvedChainValues {
    /// Creates resolved values from a built-in Base network selector.
    ///
    /// Returns `None` if the selector is not a known built-in chain.
    pub(crate) fn from_builtin(selector: &str) -> Option<Self> {
        let config = ChainConfig::from_base_chain(selector)?;
        Some(Self {
            name: selector.to_owned(),
            l2_chain_id: config.chain_id,
            l1_chain_id: config.l1_chain_id,
        })
    }
}

impl ResolvedChainConfig {
    /// Creates a resolved config from merged values and an explicit source.
    pub(crate) fn new(values: ResolvedChainValues, source: ResolvedChainSource) -> Self {
        Self {
            name: values.name,
            l2_chain_id: values.l2_chain_id,
            l1_chain_id: values.l1_chain_id,
            source,
        }
    }

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
                let defaults = ResolvedChainValues::from_builtin(chain)
                    .ok_or_else(|| eyre::eyre!("unknown built-in chain `{chain}`"))?;
                let figment = Figment::from(Serialized::defaults(defaults))
                    .merge(Env::prefixed(BASE_CHAIN_ENV_PREFIX));
                Self::extract(figment, ResolvedChainSource::BuiltIn(chain.clone()))
            }
            ChainArg::File(path) => Self::resolve_file(path),
        }
    }

    /// Resolves a chain config from a TOML file.
    pub(crate) fn resolve_file(path: &Path) -> eyre::Result<ResolvedChainConfig> {
        let figment =
            Figment::new().merge(Toml::file(path)).merge(Env::prefixed(BASE_CHAIN_ENV_PREFIX));
        Self::extract(figment, ResolvedChainSource::File(path.to_path_buf()))
    }

    /// Extracts the merged chain values into the public resolved config.
    pub(crate) fn extract(
        figment: Figment,
        source: ResolvedChainSource,
    ) -> eyre::Result<ResolvedChainConfig> {
        let values = figment.extract::<ResolvedChainValues>().wrap_err_with(|| match &source {
            ResolvedChainSource::BuiltIn(chain) => {
                format!("failed to resolve chain config for built-in chain `{chain}`")
            }
            ResolvedChainSource::File(path) => {
                format!("failed to resolve chain config from {}", path.display())
            }
        })?;

        Ok(ResolvedChainConfig::new(values, source))
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
            assert_eq!(resolved.source, ResolvedChainSource::BuiltIn("mainnet".to_owned()));

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
            assert_eq!(resolved.source, ResolvedChainSource::BuiltIn("zeronet".to_owned()));

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
            assert_eq!(resolved.source, ResolvedChainSource::BuiltIn("dev".to_owned()));

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
            assert_eq!(resolved.source, ResolvedChainSource::BuiltIn("mainnet".to_owned()));

            Ok(())
        });
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
            assert_eq!(resolved.source, ResolvedChainSource::File(path));

            Ok(())
        });
    }
}
