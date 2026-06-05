use std::{env, fmt};

use base_common_precompiles::ActivationFeature;
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::NetworkConfig;

/// Command-line entry point for the Base activation registry helper.
#[derive(Debug, Parser)]
#[command(name = "base-activator")]
#[command(about = "Inspect Beryl precompiles and build activation registry calldata.")]
pub struct Cli {
    /// Command to execute.
    #[command(subcommand)]
    pub command: Commands,
}

/// Top-level activator commands.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// List Beryl precompiles and activation gates.
    List(ListCommand),
    /// Generate raw activation registry transaction calldata.
    Calldata(CalldataCommand),
    /// Check activation registry state across Base networks.
    Status(StatusCommand),
}

/// Arguments for listing the Beryl precompile inventory.
#[derive(Debug, Args)]
pub struct ListCommand {
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

/// Arguments for generating activation-registry calldata.
#[derive(Debug, Args)]
pub struct CalldataCommand {
    /// Activation registry method to encode.
    #[arg(value_enum)]
    pub action: CalldataAction,
    /// Feature controlled by the activation registry.
    #[arg(value_enum)]
    pub feature: FeatureName,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

/// Arguments for checking activation-registry state.
#[derive(Args)]
pub struct StatusCommand {
    /// Base Mainnet RPC URL. Falls back to `BASE_MAINNET_RPC_URL`, the default public RPC, then basectl config.
    #[arg(long)]
    pub mainnet_rpc_url: Option<String>,
    /// Base Sepolia RPC URL. Falls back to `BASE_SEPOLIA_RPC_URL`, the default public RPC, then basectl config.
    #[arg(long)]
    pub sepolia_rpc_url: Option<String>,
    /// Base Zeronet RPC URL. Falls back to `BASE_ZERONET_RPC_URL`, then basectl config.
    #[arg(long)]
    pub zeronet_rpc_url: Option<String>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

impl StatusCommand {
    /// Redacts an optional RPC URL for debug output.
    pub fn redacted_url(url: &Option<String>) -> Option<&'static str> {
        url.as_ref().map(|_| "<redacted>")
    }

    /// Resolves all required RPC URLs from flags or environment variables.
    pub fn networks(&self) -> Vec<NetworkConfig> {
        vec![
            NetworkConfig::mainnet(Self::rpc_url(
                self.mainnet_rpc_url.as_deref(),
                "BASE_MAINNET_RPC_URL",
            )),
            NetworkConfig::sepolia(Self::rpc_url(
                self.sepolia_rpc_url.as_deref(),
                "BASE_SEPOLIA_RPC_URL",
            )),
            NetworkConfig::zeronet(Self::rpc_url(
                self.zeronet_rpc_url.as_deref(),
                "BASE_ZERONET_RPC_URL",
            )),
        ]
    }

    /// Resolves one RPC URL from a command-line flag or environment variable.
    pub fn rpc_url(flag: Option<&str>, env_var: &'static str) -> Option<String> {
        if let Some(url) = flag {
            return Some(url.to_owned());
        }

        env::var(env_var).ok()
    }
}

impl fmt::Debug for StatusCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatusCommand")
            .field("mainnet_rpc_url", &Self::redacted_url(&self.mainnet_rpc_url))
            .field("sepolia_rpc_url", &Self::redacted_url(&self.sepolia_rpc_url))
            .field("zeronet_rpc_url", &Self::redacted_url(&self.zeronet_rpc_url))
            .field("format", &self.format)
            .finish()
    }
}

/// Activation registry calldata action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum CalldataAction {
    /// Encode `activate(bytes32)`.
    Activate,
    /// Encode `deactivate(bytes32)`.
    Deactivate,
}

impl CalldataAction {
    /// Returns the ABI method name.
    pub const fn method(self) -> &'static str {
        match self {
            Self::Activate => "activate",
            Self::Deactivate => "deactivate",
        }
    }
}

/// Activation-registry feature selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum FeatureName {
    /// Policy registry writes.
    PolicyRegistry,
    /// B-20 asset token creation.
    B20Asset,
    /// B-20 stablecoin token creation.
    B20Stablecoin,
}

impl FeatureName {
    /// Returns the corresponding activation feature.
    pub const fn activation_feature(self) -> ActivationFeature {
        match self {
            Self::PolicyRegistry => ActivationFeature::PolicyRegistry,
            Self::B20Asset => ActivationFeature::B20Asset,
            Self::B20Stablecoin => ActivationFeature::B20Stablecoin,
        }
    }

    /// Returns the CLI label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::PolicyRegistry => "policy-registry",
            Self::B20Asset => "b20-asset",
            Self::B20Stablecoin => "b20-stablecoin",
        }
    }
}

/// Output serialization format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum OutputFormat {
    /// Human-readable table output.
    #[default]
    Table,
    /// JSON output for scripts.
    Json,
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Table => "table",
            Self::Json => "json",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_names_map_to_precompile_feature_ids() {
        assert_eq!(
            FeatureName::PolicyRegistry.activation_feature(),
            ActivationFeature::PolicyRegistry
        );
        assert_eq!(FeatureName::B20Asset.activation_feature(), ActivationFeature::B20Asset);
        assert_eq!(
            FeatureName::B20Stablecoin.activation_feature(),
            ActivationFeature::B20Stablecoin
        );
    }

    #[test]
    fn status_command_debug_redacts_rpc_urls() {
        let command = StatusCommand {
            mainnet_rpc_url: Some("https://example.invalid/?token=secret".to_owned()),
            sepolia_rpc_url: None,
            zeronet_rpc_url: Some("https://zeronet.invalid/?token=secret".to_owned()),
            format: OutputFormat::Json,
        };

        let debug = format!("{command:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("token=secret"));
    }
}
