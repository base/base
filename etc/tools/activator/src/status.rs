use std::{
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use alloy_primitives::{B256, Bytes};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::TransactionInput;
use alloy_sol_types::SolCall;
use alloy_transport_http::{Client, Http};
use base_common_network::Base;
use base_common_precompiles::{ActivationRegistryStorage, IActivationRegistry};
use base_common_rpc_types::BaseTransactionRequest;
use eyre::{Context, Result};
use serde_yaml::Value;
use tokio::time::timeout;
use url::Url;

use crate::{
    ActivationState, FeatureCatalog, FeatureStatus, NetworkConfig, NetworkStatus, RpcCandidate,
    RpcSource, StatusReport,
};

/// Checks live activation registry state over JSON-RPC.
#[derive(Debug, Clone, Copy)]
pub struct StatusChecker;

impl StatusChecker {
    /// Per-RPC-candidate timeout.
    pub const CANDIDATE_TIMEOUT: Duration = Duration::from_secs(10);

    /// Checks every activation feature on every configured network.
    pub async fn check_all(networks: &[NetworkConfig]) -> Result<StatusReport> {
        let mut statuses = Vec::with_capacity(networks.len());
        for network in networks {
            statuses.push(Self::check_network(network).await?);
        }
        Ok(StatusReport { networks: statuses })
    }

    /// Checks every activation feature on one network.
    pub async fn check_network(network: &NetworkConfig) -> Result<NetworkStatus> {
        let candidates = Self::rpc_candidates(network);
        if candidates.is_empty() {
            return Ok(Self::network_error(
                network,
                None,
                None,
                format!(
                    "missing RPC URL; pass {}, set {}, or add rpc to basectl config {}",
                    network.rpc_url_flag,
                    network.rpc_url_env_var,
                    Self::basectl_config_hint(network)
                ),
            ));
        };

        let mut last_error = None;
        for candidate in candidates {
            match Self::check_candidate_with_timeout(network, &candidate).await {
                Ok(status) => return Ok(status),
                Err(error) => {
                    last_error = Some(error);
                }
            }
        }

        Ok(Self::network_error(
            network,
            None,
            None,
            last_error.unwrap_or_else(|| "all RPC candidates failed".to_owned()),
        ))
    }

    /// Checks one network with one RPC candidate and applies a bounded timeout.
    pub async fn check_candidate_with_timeout(
        network: &NetworkConfig,
        candidate: &RpcCandidate,
    ) -> std::result::Result<NetworkStatus, String> {
        timeout(Self::CANDIDATE_TIMEOUT, Self::check_candidate(network, candidate))
            .await
            .unwrap_or_else(|_| Err(Self::candidate_error(candidate, "timed out")))
    }

    /// Checks one network with one RPC candidate.
    pub async fn check_candidate(
        network: &NetworkConfig,
        candidate: &RpcCandidate,
    ) -> std::result::Result<NetworkStatus, String> {
        let provider = Self::provider(&candidate.url)
            .map_err(|_| Self::candidate_error(candidate, "invalid RPC URL"))?;
        let chain_id = provider
            .get_chain_id()
            .await
            .map_err(|_| Self::candidate_error(candidate, "chain ID request failed"))?;
        if chain_id != network.expected_chain_id {
            return Err(Self::candidate_error(
                candidate,
                &format!("chain ID mismatch: expected {}", network.expected_chain_id),
            ));
        }

        let mut features = Vec::new();
        for feature in FeatureCatalog::features() {
            let state = Self::check_feature_call(&provider, feature.id)
                .await
                .map_err(|error| Self::candidate_error(candidate, &error))?;
            features.push(FeatureStatus {
                feature: feature.feature,
                feature_id: feature.id,
                state,
            });
        }

        Ok(NetworkStatus {
            network: network.name,
            expected_chain_id: network.expected_chain_id,
            chain_id: Some(chain_id),
            beryl_timestamp: network.beryl_timestamp,
            rpc_source: Some(candidate.source),
            error: None,
            features,
        })
    }

    /// Builds a network-level error status.
    pub const fn network_error(
        network: &NetworkConfig,
        chain_id: Option<u64>,
        rpc_source: Option<RpcSource>,
        error: String,
    ) -> NetworkStatus {
        NetworkStatus {
            network: network.name,
            expected_chain_id: network.expected_chain_id,
            chain_id,
            beryl_timestamp: network.beryl_timestamp,
            rpc_source,
            error: Some(error),
            features: Vec::new(),
        }
    }

    /// Returns RPC URL candidates in priority order.
    pub fn rpc_candidates(network: &NetworkConfig) -> Vec<RpcCandidate> {
        let mut candidates = Vec::new();
        if let Some(url) = &network.rpc_url {
            Self::push_candidate(&mut candidates, RpcSource::Configured, url.clone());
        }
        if let Some(url) = network.default_rpc_url {
            Self::push_candidate(&mut candidates, RpcSource::Default, url.to_owned());
        }
        if let Some(url) = Self::basectl_rpc_url(network) {
            Self::push_candidate(&mut candidates, RpcSource::BasectlConfig, url);
        }
        candidates
    }

    /// Pushes a candidate, skipping duplicate URLs already present from higher-priority sources.
    pub fn push_candidate(candidates: &mut Vec<RpcCandidate>, source: RpcSource, url: String) {
        if candidates.iter().any(|candidate| candidate.url == url) {
            return;
        }
        candidates.push(RpcCandidate { source, url });
    }

    /// Reads the `rpc` field from basectl's user network config, if present.
    pub fn basectl_rpc_url(network: &NetworkConfig) -> Option<String> {
        let path = Self::basectl_config_path(network)?;
        Self::basectl_rpc_url_from_path(&path)
    }

    /// Reads the `rpc` field from one basectl user network config path.
    pub fn basectl_rpc_url_from_path(path: &Path) -> Option<String> {
        let contents = fs::read_to_string(path).ok()?;
        Self::basectl_rpc_url_from_yaml(&contents)
    }

    /// Reads the `rpc` field from basectl YAML contents.
    pub fn basectl_rpc_url_from_yaml(contents: &str) -> Option<String> {
        let value: Value = serde_yaml::from_str(contents).ok()?;
        value.get("rpc")?.as_str().filter(|url| !url.trim().is_empty()).map(str::to_owned)
    }

    /// Returns basectl's user network config path for this network.
    pub fn basectl_config_path(network: &NetworkConfig) -> Option<PathBuf> {
        let dir = Self::basectl_config_dir()?;
        let yaml = dir.join(format!("{}.yaml", network.basectl_config_name));
        if yaml.exists() {
            return Some(yaml);
        }
        let yml = dir.join(format!("{}.yml", network.basectl_config_name));
        if yml.exists() {
            return Some(yml);
        }
        None
    }

    /// Returns basectl's user network config directory.
    pub fn basectl_config_dir() -> Option<PathBuf> {
        env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".config").join("base").join("networks"))
    }

    /// Returns a non-secret basectl config hint for error messages.
    pub fn basectl_config_hint(network: &NetworkConfig) -> String {
        format!("~/.config/base/networks/{}.yaml", network.basectl_config_name)
    }

    /// Builds a non-secret candidate failure message.
    pub fn candidate_error(candidate: &RpcCandidate, reason: &str) -> String {
        format!("{} RPC {reason}", candidate.source.label())
    }

    /// Creates a Base JSON-RPC provider.
    pub fn provider(rpc_url: &str) -> Result<RootProvider<Base>> {
        let url: Url = rpc_url.parse().wrap_err("invalid RPC URL")?;
        let http = Http::<Client>::new(url);
        Ok(RootProvider::<Base>::new(RpcClient::new(http, false)))
    }

    /// Checks one activation feature.
    pub async fn check_feature(provider: &RootProvider<Base>, feature_id: B256) -> ActivationState {
        match Self::check_feature_call(provider, feature_id).await {
            Ok(state) => state,
            Err(error) => ActivationState::Error(error),
        }
    }

    /// Checks one activation feature, returning transport failures as candidate failures.
    pub async fn check_feature_call(
        provider: &RootProvider<Base>,
        feature_id: B256,
    ) -> std::result::Result<ActivationState, String> {
        let call = IActivationRegistry::isActivatedCall { feature: feature_id };
        let request = BaseTransactionRequest::default()
            .to(ActivationRegistryStorage::ADDRESS)
            .input(TransactionInput::new(Bytes::from(call.abi_encode())));

        let output = match provider.call(request).await {
            Ok(output) => output,
            Err(_) => return Err("activation registry call failed".to_owned()),
        };
        if output.is_empty() {
            return Ok(ActivationState::Unavailable);
        }

        Ok(match IActivationRegistry::isActivatedCall::abi_decode_returns(output.as_ref()) {
            Ok(true) => ActivationState::Active,
            Ok(false) => ActivationState::Inactive,
            Err(error) => ActivationState::Error(error.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rpc_urls_are_baked_in_for_public_networks() {
        assert_eq!(NetworkConfig::mainnet(None).default_rpc_url, Some("https://mainnet.base.org"));
        assert_eq!(NetworkConfig::sepolia(None).default_rpc_url, Some("https://sepolia.base.org"));
        assert_eq!(NetworkConfig::zeronet(None).default_rpc_url, None);
    }

    #[test]
    fn push_candidate_keeps_first_source_for_duplicate_urls() {
        let mut candidates = Vec::new();

        StatusChecker::push_candidate(
            &mut candidates,
            RpcSource::Configured,
            "https://mainnet.base.org".to_owned(),
        );
        StatusChecker::push_candidate(
            &mut candidates,
            RpcSource::Default,
            "https://mainnet.base.org".to_owned(),
        );

        assert_eq!(
            candidates,
            vec![RpcCandidate {
                source: RpcSource::Configured,
                url: "https://mainnet.base.org".to_owned(),
            }]
        );
    }

    #[test]
    fn basectl_rpc_url_from_yaml_reads_top_level_rpc() {
        let contents = r#"
name: custom-mainnet
rpc: https://example.invalid/base
pods:
  namespace: default
"#;

        assert_eq!(
            StatusChecker::basectl_rpc_url_from_yaml(contents),
            Some("https://example.invalid/base".to_owned())
        );
    }

    #[test]
    fn basectl_rpc_url_from_yaml_ignores_missing_or_empty_rpc() {
        assert_eq!(StatusChecker::basectl_rpc_url_from_yaml("pods: []"), None);
        assert_eq!(StatusChecker::basectl_rpc_url_from_yaml("rpc: ''"), None);
    }
}
