use std::fmt;

use alloy_primitives::{Address, B256};
use base_common_chains::{BaseUpgrade, ChainConfig};
use base_common_precompiles::{
    ActivationRegistryStorage, B20FactoryStorage, B20Variant, PolicyRegistryStorage,
};

use crate::FeatureName;

/// Location for a Beryl precompile surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrecompileLocation {
    /// A singleton precompile address.
    Address(Address),
    /// A dynamic address family identified by an 11-byte prefix.
    AddressPrefix([u8; 11]),
}

/// Describes a precompile surface installed at Beryl.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrecompileInfo {
    /// Human-readable precompile name.
    pub name: &'static str,
    /// Address or address family for this precompile.
    pub location: PrecompileLocation,
    /// Base upgrade that installs this precompile surface.
    pub installed_at: BaseUpgrade,
    /// Optional activation feature that gates meaningful writes or creation.
    pub activation_feature: Option<FeatureName>,
    /// Short operational note.
    pub note: &'static str,
}

/// Known activation-registry feature metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureInfo {
    /// CLI feature selector.
    pub feature: FeatureName,
    /// Activation registry feature ID.
    pub id: B256,
}

/// RPC configuration for one Base network.
#[derive(Clone, PartialEq, Eq)]
pub struct NetworkConfig {
    /// Network display name.
    pub name: &'static str,
    /// User-configured RPC URL. This is never printed because it may contain credentials.
    pub rpc_url: Option<String>,
    /// Flag used to configure this network's RPC URL.
    pub rpc_url_flag: &'static str,
    /// Environment variable used to configure this network's RPC URL.
    pub rpc_url_env_var: &'static str,
    /// Built-in public RPC URL.
    pub default_rpc_url: Option<&'static str>,
    /// Basectl network config name used for fallback lookup.
    pub basectl_config_name: &'static str,
    /// Expected L2 chain ID.
    pub expected_chain_id: u64,
    /// Configured Beryl timestamp for this network, if scheduled.
    pub beryl_timestamp: Option<u64>,
}

impl NetworkConfig {
    /// Redacts the configured RPC URL for debug output.
    pub fn redacted_rpc_url(&self) -> Option<&'static str> {
        self.rpc_url.as_ref().map(|_| "<redacted>")
    }

    /// Creates a Base Mainnet status target.
    pub const fn mainnet(rpc_url: Option<String>) -> Self {
        Self::from_chain_config(
            "base-mainnet",
            rpc_url,
            "--mainnet-rpc-url",
            "BASE_MAINNET_RPC_URL",
            Some("https://mainnet.base.org"),
            "mainnet",
            ChainConfig::mainnet(),
        )
    }

    /// Creates a Base Sepolia status target.
    pub const fn sepolia(rpc_url: Option<String>) -> Self {
        Self::from_chain_config(
            "base-sepolia",
            rpc_url,
            "--sepolia-rpc-url",
            "BASE_SEPOLIA_RPC_URL",
            Some("https://sepolia.base.org"),
            "sepolia",
            ChainConfig::sepolia(),
        )
    }

    /// Creates a Base Zeronet status target.
    pub const fn zeronet(rpc_url: Option<String>) -> Self {
        Self::from_chain_config(
            "base-zeronet",
            rpc_url,
            "--zeronet-rpc-url",
            "BASE_ZERONET_RPC_URL",
            None,
            "zeronet",
            ChainConfig::zeronet(),
        )
    }

    /// Creates a status target from a chain config.
    pub const fn from_chain_config(
        name: &'static str,
        rpc_url: Option<String>,
        rpc_url_flag: &'static str,
        rpc_url_env_var: &'static str,
        default_rpc_url: Option<&'static str>,
        basectl_config_name: &'static str,
        chain_config: &'static ChainConfig,
    ) -> Self {
        Self {
            name,
            rpc_url,
            rpc_url_flag,
            rpc_url_env_var,
            default_rpc_url,
            basectl_config_name,
            expected_chain_id: chain_config.chain_id,
            beryl_timestamp: chain_config.beryl_timestamp,
        }
    }
}

impl fmt::Debug for NetworkConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetworkConfig")
            .field("name", &self.name)
            .field("rpc_url", &self.redacted_rpc_url())
            .field("rpc_url_flag", &self.rpc_url_flag)
            .field("rpc_url_env_var", &self.rpc_url_env_var)
            .field("default_rpc_url", &self.default_rpc_url)
            .field("basectl_config_name", &self.basectl_config_name)
            .field("expected_chain_id", &self.expected_chain_id)
            .field("beryl_timestamp", &self.beryl_timestamp)
            .finish()
    }
}

/// Activation state returned by a live registry check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationState {
    /// The feature is active.
    Active,
    /// The feature is inactive.
    Inactive,
    /// The registry is not available at the queried block or endpoint.
    Unavailable,
    /// The RPC returned an error or invalid ABI data.
    Error(String),
}

/// Activation state for one feature on one network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureStatus {
    /// Feature that was checked.
    pub feature: FeatureName,
    /// Feature ID sent to the registry.
    pub feature_id: B256,
    /// Observed state.
    pub state: ActivationState,
}

/// Activation status for one network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkStatus {
    /// Network display name.
    pub network: &'static str,
    /// Expected chain ID from `base-common-chains`.
    pub expected_chain_id: u64,
    /// Chain ID returned by the RPC endpoint, if available.
    pub chain_id: Option<u64>,
    /// Configured Beryl timestamp for this network, if scheduled.
    pub beryl_timestamp: Option<u64>,
    /// Source of the RPC URL that produced this status.
    pub rpc_source: Option<RpcSource>,
    /// Network-level error. When set, feature checks were skipped.
    pub error: Option<String>,
    /// Per-feature activation states.
    pub features: Vec<FeatureStatus>,
}

/// Activation status report for all requested networks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    /// Per-network activation status.
    pub networks: Vec<NetworkStatus>,
}

/// Non-secret label for the origin of an RPC URL candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcSource {
    /// Command-line flag or environment variable.
    Configured,
    /// Built-in public RPC URL.
    Default,
    /// User basectl config under `~/.config/base/networks`.
    BasectlConfig,
}

impl RpcSource {
    /// Returns a stable display label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Default => "default",
            Self::BasectlConfig => "basectl-config",
        }
    }
}

/// One RPC URL candidate. The URL itself must not be printed.
#[derive(Clone, PartialEq, Eq)]
pub struct RpcCandidate {
    /// Non-secret source label.
    pub source: RpcSource,
    /// RPC URL. This may contain credentials and should not be displayed.
    pub url: String,
}

impl fmt::Debug for RpcCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcCandidate")
            .field("source", &self.source)
            .field("url", &"<redacted>")
            .finish()
    }
}

/// Provides the activation feature inventory.
#[derive(Debug, Clone, Copy)]
pub struct FeatureCatalog;

impl FeatureCatalog {
    /// Returns the activation-registry features controlled by this tool.
    pub const fn features() -> [FeatureInfo; 3] {
        [
            Self::feature(FeatureName::PolicyRegistry),
            Self::feature(FeatureName::B20Asset),
            Self::feature(FeatureName::B20Stablecoin),
        ]
    }

    /// Returns metadata for one activation-registry feature.
    pub const fn feature(feature: FeatureName) -> FeatureInfo {
        FeatureInfo { feature, id: feature.activation_feature().id() }
    }
}

/// Provides the Beryl precompile inventory.
#[derive(Debug, Clone, Copy)]
pub struct PrecompileCatalog;

impl PrecompileCatalog {
    /// Returns the known Beryl precompile surfaces.
    pub fn beryl() -> Vec<PrecompileInfo> {
        vec![
            PrecompileInfo {
                name: "activation-registry",
                location: PrecompileLocation::Address(ActivationRegistryStorage::ADDRESS),
                installed_at: BaseUpgrade::Beryl,
                activation_feature: None,
                note: "controls runtime activation flags",
            },
            PrecompileInfo {
                name: "policy-registry",
                location: PrecompileLocation::Address(PolicyRegistryStorage::ADDRESS),
                installed_at: BaseUpgrade::Beryl,
                activation_feature: Some(FeatureName::PolicyRegistry),
                note: "view calls are open; writes require activation",
            },
            PrecompileInfo {
                name: "b20-factory",
                location: PrecompileLocation::Address(B20FactoryStorage::ADDRESS),
                installed_at: BaseUpgrade::Beryl,
                activation_feature: None,
                note: "token creation checks the selected variant feature",
            },
            PrecompileInfo {
                name: "b20-asset",
                location: PrecompileLocation::AddressPrefix(B20Variant::Asset.address_prefix()),
                installed_at: BaseUpgrade::Beryl,
                activation_feature: Some(FeatureName::B20Asset),
                note: "dynamic token precompile address family",
            },
            PrecompileInfo {
                name: "b20-stablecoin",
                location: PrecompileLocation::AddressPrefix(
                    B20Variant::Stablecoin.address_prefix(),
                ),
                installed_at: BaseUpgrade::Beryl,
                activation_feature: Some(FeatureName::B20Stablecoin),
                note: "dynamic token precompile address family",
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use base_common_precompiles::{
        ActivationFeature, ActivationRegistryStorage, B20FactoryStorage, PolicyRegistryStorage,
    };

    use super::*;

    #[test]
    fn inventory_uses_exported_precompile_addresses() {
        let items = PrecompileCatalog::beryl();

        assert!(items.iter().any(|item| {
            item.location == PrecompileLocation::Address(ActivationRegistryStorage::ADDRESS)
        }));
        assert!(items.iter().any(|item| {
            item.location == PrecompileLocation::Address(PolicyRegistryStorage::ADDRESS)
        }));
        assert!(
            items.iter().any(
                |item| item.location == PrecompileLocation::Address(B20FactoryStorage::ADDRESS)
            )
        );
    }

    #[test]
    fn feature_catalog_uses_exported_feature_ids() {
        let features = FeatureCatalog::features();

        assert!(features.iter().any(|feature| feature.id == ActivationFeature::B20Asset.id()));
        assert!(features.iter().any(|feature| feature.id == ActivationFeature::B20Stablecoin.id()));
        assert!(
            features.iter().any(|feature| feature.id == ActivationFeature::PolicyRegistry.id())
        );
    }

    #[test]
    fn network_config_debug_redacts_rpc_url() {
        let config =
            NetworkConfig::mainnet(Some("https://example.invalid/?token=secret".to_owned()));

        let debug = format!("{config:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("token=secret"));
    }

    #[test]
    fn rpc_candidate_debug_redacts_rpc_url() {
        let candidate = RpcCandidate {
            source: RpcSource::Configured,
            url: "https://example.invalid/?token=secret".to_owned(),
        };

        let debug = format!("{candidate:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("token=secret"));
    }
}
