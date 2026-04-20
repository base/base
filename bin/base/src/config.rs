use std::{
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

use base_common_chains::ChainConfig as BuiltInChainConfig;
use eyre::WrapErr;
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};

/// Prefix for chain configuration environment variables.
pub(crate) const BASE_CHAIN_ENV_PREFIX: &str = "BASE_CHAIN_";

/// A built-in chain supported by the `base` binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BuiltInChain {
    /// Base mainnet.
    Mainnet,
    /// Base sepolia.
    Sepolia,
    /// Base zeronet.
    Zeronet,
}

impl BuiltInChain {
    /// Returns the canonical CLI name for this chain.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Sepolia => "sepolia",
            Self::Zeronet => "zeronet",
        }
    }

    /// Returns the built-in chain config backing this selection.
    pub(crate) const fn chain_config(self) -> &'static BuiltInChainConfig {
        match self {
            Self::Mainnet => BuiltInChainConfig::mainnet(),
            Self::Sepolia => BuiltInChainConfig::sepolia(),
            Self::Zeronet => BuiltInChainConfig::zeronet(),
        }
    }
}

impl fmt::Display for BuiltInChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BuiltInChain {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "mainnet" => Ok(Self::Mainnet),
            "sepolia" => Ok(Self::Sepolia),
            "zeronet" => Ok(Self::Zeronet),
            _ => Err(format!(
                "unsupported built-in chain `{value}`; expected one of mainnet, sepolia, zeronet"
            )),
        }
    }
}

/// CLI input for the root `--chain` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChainArg {
    /// Use one of the built-in static chains.
    BuiltIn(BuiltInChain),
    /// Load chain settings from a TOML file.
    File(PathBuf),
}

impl Default for ChainArg {
    fn default() -> Self {
        Self::BuiltIn(BuiltInChain::Mainnet)
    }
}

impl FromStr for ChainArg {
    type Err = std::convert::Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(BuiltInChain::from_str(value)
            .map_or_else(|_| Self::File(PathBuf::from(value)), Self::BuiltIn))
    }
}

/// The concrete source of a resolved chain config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ResolvedChainSource {
    /// The config came from a built-in static chain.
    BuiltIn(BuiltInChain),
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
    /// Creates resolved values from a built-in chain.
    pub(crate) fn from_builtin(chain: BuiltInChain) -> Self {
        let config = chain.chain_config();
        Self {
            name: chain.as_str().to_owned(),
            l2_chain_id: config.chain_id,
            l1_chain_id: config.l1_chain_id,
        }
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
}

/// Resolves a chain selection into a concrete config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChainResolver {
    /// The requested chain input.
    pub(crate) chain: ChainArg,
}

impl ChainResolver {
    /// Creates a new chain resolver.
    pub(crate) const fn new(chain: ChainArg) -> Self {
        Self { chain }
    }

    /// Resolves the configured chain input.
    pub(crate) fn resolve(&self) -> eyre::Result<ResolvedChainConfig> {
        match &self.chain {
            ChainArg::BuiltIn(chain) => {
                let figment =
                    Figment::from(Serialized::defaults(ResolvedChainValues::from_builtin(*chain)))
                        .merge(Env::prefixed(BASE_CHAIN_ENV_PREFIX));
                Self::extract(figment, ResolvedChainSource::BuiltIn(*chain))
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

    fn with_cleared_env(test: impl FnOnce(&mut Jail) -> figment::Result<()>) {
        Jail::expect_with(|jail| {
            jail.clear_env();
            test(jail)
        });
    }

    #[test]
    fn resolves_mainnet_builtin() {
        with_cleared_env(|_| {
            let resolved =
                ChainResolver::new(ChainArg::BuiltIn(BuiltInChain::Mainnet)).resolve().unwrap();

            assert_eq!(resolved.name, "mainnet");
            assert_eq!(resolved.l2_chain_id, 8453);
            assert_eq!(resolved.l1_chain_id, 1);
            assert_eq!(resolved.source, ResolvedChainSource::BuiltIn(BuiltInChain::Mainnet));

            Ok(())
        });
    }

    #[test]
    fn resolves_sepolia_builtin() {
        with_cleared_env(|_| {
            let resolved =
                ChainResolver::new(ChainArg::BuiltIn(BuiltInChain::Sepolia)).resolve().unwrap();

            assert_eq!(resolved.name, "sepolia");
            assert_eq!(resolved.l2_chain_id, 84532);
            assert_eq!(resolved.l1_chain_id, 11155111);

            Ok(())
        });
    }

    #[test]
    fn resolves_zeronet_builtin() {
        with_cleared_env(|_| {
            let resolved =
                ChainResolver::new(ChainArg::BuiltIn(BuiltInChain::Zeronet)).resolve().unwrap();

            assert_eq!(resolved.name, "zeronet");
            assert_eq!(resolved.l2_chain_id, 763360);
            assert_eq!(resolved.source, ResolvedChainSource::BuiltIn(BuiltInChain::Zeronet));

            Ok(())
        });
    }

    #[test]
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
