//! Backend construction for Succinct ZK provers.

use std::{collections::HashMap, error::Error, fmt, future::Future, sync::Arc, time::Duration};

use base_proof_succinct_host_utils::fetcher::{OPSuccinctDataFetcher, RPCConfig};
use base_proof_zk_host::{ZkBackend, ZkProver};
use thiserror::Error;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

use crate::succinct::{
    ClusterZkProver, DryRunZkProver, MockZkProver, NetworkZkProver, OpSuccinctWitnessProvider,
    SuccinctClusterBackendConfig, SuccinctNetworkBackendConfig,
};

type BackendConfigs = (Vec<(ZkBackend, SuccinctZkBackendConfig)>, Option<SuccinctRpcConfig>);

/// Errors raised while building a Succinct ZK prover backend.
#[derive(Debug, Error)]
pub enum SuccinctZkProverBuildError {
    /// A configuration value is invalid.
    #[error("configuration error: {0}")]
    Config(String),
    /// A backend initialization step failed.
    #[error("{context}")]
    Operation {
        /// Failed initialization operation.
        context: &'static str,
        /// Underlying operation error.
        #[source]
        source: Box<dyn Error + Send + Sync + 'static>,
    },
    /// A configured backend failed to initialize.
    #[error("failed to initialize {backend} zk proving backend")]
    Backend {
        /// Backend that failed to initialize.
        backend: ZkBackend,
        /// Underlying initialization error.
        #[source]
        source: Box<dyn Error + Send + Sync + 'static>,
    },
}

impl SuccinctZkProverBuildError {
    /// Creates a configuration error.
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    /// Creates an operation error with source context.
    pub fn operation(context: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
        Self::Operation { context, source: Box::new(source) }
    }

    /// Creates an operation error from a boxed source.
    pub fn boxed_operation(
        context: &'static str,
        source: Box<dyn Error + Send + Sync + 'static>,
    ) -> Self {
        Self::Operation { context, source }
    }

    /// Adds the backend that failed to initialize.
    pub fn backend(backend: ZkBackend, source: Self) -> Self {
        Self::Backend { backend, source: Box::new(source) }
    }
}

/// Succinct backend implementation to build.
#[derive(Clone, Debug)]
pub enum SuccinctZkBackendConfig {
    /// Return placeholder proof bytes without an external backend.
    Mock,
    /// Generate a witness and run local SP1 execution without producing proof bytes.
    DryRun {
        /// Shared RPC settings.
        rpc: SuccinctRpcConfig,
        /// Cycle limit for local range program execution.
        range_cycle_limit: u64,
    },
    /// Submit proofs to an SP1 cluster.
    Cluster(SuccinctClusterBackendConfig),
    /// Submit proofs to the Succinct SP1 Network.
    Network(SuccinctNetworkBackendConfig),
}

/// Shared RPC settings for Succinct proving backends.
#[derive(Clone, Debug)]
pub struct SuccinctRpcConfig {
    /// Base consensus node RPC URL.
    pub base_consensus_rpc: Url,
    /// L1 execution node RPC URL.
    pub l1_rpc: Url,
    /// L1 beacon node RPC URL.
    pub l1_beacon_rpc: Url,
    /// L2 execution node RPC URL.
    pub l2_rpc: Url,
    /// Default sequence window for L1 head calculations.
    pub default_sequence_window: u64,
}

/// Configuration for all Succinct proving backends available to one worker.
#[derive(Clone)]
pub struct SuccinctZkProversConfig {
    /// Enables the mock backend.
    pub enable_mock: bool,
    /// Base consensus node RPC URL.
    pub base_consensus_rpc: Option<Url>,
    /// L1 execution node RPC URL.
    pub l1_rpc: Option<Url>,
    /// L1 beacon node RPC URL.
    pub l1_beacon_rpc: Option<Url>,
    /// L2 execution node RPC URL.
    pub l2_rpc: Option<Url>,
    /// Default sequence window for L1 head calculations.
    pub default_sequence_window: u64,
    /// SP1 cluster gRPC endpoint.
    pub cluster_rpc: Option<String>,
    /// SP1 cluster proof timeout in hours.
    pub cluster_timeout_hours: u64,
    /// S3 artifact store bucket.
    pub s3_bucket: Option<String>,
    /// S3 artifact store region.
    pub s3_region: Option<String>,
    /// SP1 network requester private key or KMS key ARN.
    pub network_private_key: Option<String>,
    /// Whether the network requester key is an AWS KMS ARN.
    pub use_kms_requester: bool,
    /// SP1 network proof timeout in hours.
    pub network_timeout_hours: u64,
    /// Cycle limit for range proof requests.
    pub range_cycle_limit: u64,
    /// Gas limit for range proof requests.
    pub range_gas_limit: u64,
    /// Cycle limit for aggregation proof requests.
    pub aggregation_cycle_limit: u64,
    /// Gas limit for aggregation proof requests.
    pub aggregation_gas_limit: u64,
}

impl fmt::Debug for SuccinctZkProversConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SuccinctZkProversConfig")
            .field("enable_mock", &self.enable_mock)
            .field("cluster_configured", &self.cluster_rpc.is_some())
            .field("network_configured", &self.network_private_key.is_some())
            .field("use_kms_requester", &self.use_kms_requester)
            .finish_non_exhaustive()
    }
}

impl SuccinctZkProversConfig {
    /// Builds every configured prover unless cancellation is requested first.
    pub async fn build_until_cancelled(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Option<HashMap<ZkBackend, Arc<dyn ZkProver>>>, SuccinctZkProverBuildError> {
        let (configs, rpc) = self.backend_configs()?;
        let witness_provider = if let Some(rpc) = rpc {
            let Some(provider) =
                Box::pin(SuccinctZkProverBuilder::build_witness_provider(rpc, cancel)).await?
            else {
                return Ok(None);
            };
            Some(provider)
        } else {
            None
        };

        let mut tasks = JoinSet::new();
        for (backend, config) in configs {
            let witness_provider = witness_provider.clone();
            let cancel = cancel.clone();
            tasks.spawn(async move {
                let mut builder = SuccinctZkProverBuilder::new(config);
                if let Some(provider) = witness_provider {
                    builder = builder.with_witness_provider(provider);
                }
                let result = Box::pin(builder.build_until_cancelled(&cancel))
                    .await
                    .map_err(|source| SuccinctZkProverBuildError::backend(backend, source))?;
                Ok::<_, SuccinctZkProverBuildError>((backend, result))
            });
        }

        let mut provers = HashMap::new();
        while let Some(result) = tasks.join_next().await {
            let result = result
                .map_err(|source| {
                    SuccinctZkProverBuildError::operation(
                        "zk backend initialization task failed",
                        source,
                    )
                })
                .and_then(std::convert::identity);
            let (backend, prover) = match result {
                Ok(result) => result,
                Err(error) => {
                    tasks.shutdown().await;
                    return Err(error);
                }
            };
            let Some(prover) = prover else {
                tasks.shutdown().await;
                return Ok(None);
            };
            provers.insert(backend, prover);
        }

        Ok(Some(provers))
    }

    fn optional_string(value: Option<&str>) -> Option<String> {
        value.map(str::trim).filter(|value| !value.is_empty()).map(ToOwned::to_owned)
    }

    fn rpc_config(&self) -> Result<Option<SuccinctRpcConfig>, SuccinctZkProverBuildError> {
        match (&self.base_consensus_rpc, &self.l1_rpc, &self.l1_beacon_rpc, &self.l2_rpc) {
            (None, None, None, None) => Ok(None),
            (Some(base_consensus_rpc), Some(l1_rpc), Some(l1_beacon_rpc), Some(l2_rpc)) => {
                Ok(Some(SuccinctRpcConfig {
                    base_consensus_rpc: base_consensus_rpc.clone(),
                    l1_rpc: l1_rpc.clone(),
                    l1_beacon_rpc: l1_beacon_rpc.clone(),
                    l2_rpc: l2_rpc.clone(),
                    default_sequence_window: self.default_sequence_window,
                }))
            }
            _ => Err(SuccinctZkProverBuildError::config(
                "BASE_CONSENSUS_ADDRESS, L1_NODE_ADDRESS, L1_BEACON_ADDRESS, and L2_NODE_ADDRESS must all be set to enable dry-run, cluster, or network backends",
            )),
        }
    }

    fn duration_from_hours(
        hours: u64,
        name: &'static str,
    ) -> Result<Duration, SuccinctZkProverBuildError> {
        let seconds = hours
            .checked_mul(3600)
            .ok_or_else(|| SuccinctZkProverBuildError::config(format!("{name} is too large")))?;
        Ok(Duration::from_secs(seconds))
    }

    fn cluster_requires_rpc(&self) -> bool {
        self.cluster_rpc.as_deref().is_some_and(|value| !value.trim().is_empty())
    }

    fn network_requires_rpc(&self) -> bool {
        self.network_private_key.as_deref().is_some_and(|value| !value.trim().is_empty())
    }

    fn cluster_config(
        &self,
        rpc: Option<&SuccinctRpcConfig>,
    ) -> Result<Option<SuccinctClusterBackendConfig>, SuccinctZkProverBuildError> {
        let cluster_rpc = Self::optional_string(self.cluster_rpc.as_deref());
        let s3_bucket = Self::optional_string(self.s3_bucket.as_deref());
        let s3_region = Self::optional_string(self.s3_region.as_deref());
        match (cluster_rpc, s3_bucket, s3_region, rpc) {
            (None, _, _, _) => Ok(None),
            (Some(_), None, _, _) => {
                Err(SuccinctZkProverBuildError::config("cluster backend requires CLI_S3_BUCKET"))
            }
            (Some(_), _, None, _) => {
                Err(SuccinctZkProverBuildError::config("cluster backend requires CLI_S3_REGION"))
            }
            (Some(_), Some(_), Some(_), None) => {
                Err(SuccinctZkProverBuildError::config("cluster backend requires all RPC URLs"))
            }
            (Some(cluster_rpc), Some(s3_bucket), Some(s3_region), Some(rpc)) => {
                Ok(Some(SuccinctClusterBackendConfig {
                    rpc: rpc.clone(),
                    cluster_rpc,
                    s3_bucket,
                    s3_region,
                    timeout: Self::duration_from_hours(
                        self.cluster_timeout_hours,
                        "SP1_CLUSTER_TIMEOUT_HOURS",
                    )?,
                    range_cycle_limit: self.range_cycle_limit,
                    range_gas_limit: self.range_gas_limit,
                    aggregation_cycle_limit: self.aggregation_cycle_limit,
                    aggregation_gas_limit: self.aggregation_gas_limit,
                }))
            }
        }
    }

    fn network_config(
        &self,
        rpc: Option<&SuccinctRpcConfig>,
    ) -> Result<Option<SuccinctNetworkBackendConfig>, SuccinctZkProverBuildError> {
        match (
            Self::optional_string(self.network_private_key.as_deref()),
            self.use_kms_requester,
            rpc,
        ) {
            (None, false, _) => Ok(None),
            (None, true, _) => Err(SuccinctZkProverBuildError::config(
                "USE_KMS_REQUESTER requires NETWORK_PRIVATE_KEY",
            )),
            (Some(_), _, None) => {
                Err(SuccinctZkProverBuildError::config("network backend requires all RPC URLs"))
            }
            (Some(network_private_key), use_kms_requester, Some(rpc)) => {
                Ok(Some(SuccinctNetworkBackendConfig {
                    rpc: rpc.clone(),
                    network_private_key,
                    use_kms_requester,
                    timeout: Self::duration_from_hours(
                        self.network_timeout_hours,
                        "SP1_NETWORK_TIMEOUT_HOURS",
                    )?,
                    range_cycle_limit: self.range_cycle_limit,
                    range_gas_limit: self.range_gas_limit,
                    aggregation_cycle_limit: self.aggregation_cycle_limit,
                    aggregation_gas_limit: self.aggregation_gas_limit,
                }))
            }
        }
    }

    fn backend_configs(&self) -> Result<BackendConfigs, SuccinctZkProverBuildError> {
        let (rpc, ignored_rpc_error) = match self.rpc_config() {
            Ok(rpc) => (rpc, None),
            Err(error)
                if self.enable_mock
                    && !self.cluster_requires_rpc()
                    && !self.network_requires_rpc() =>
            {
                (None, Some(error))
            }
            Err(error) => return Err(error),
        };
        let cluster = self.cluster_config(rpc.as_ref())?;
        let network = self.network_config(rpc.as_ref())?;
        if let Some(error) = ignored_rpc_error {
            warn!(
                error = %error,
                "ignoring incomplete RPC configuration for mock-only host"
            );
        }
        let mut configs = Vec::new();
        if self.enable_mock {
            configs.push((ZkBackend::Mock, SuccinctZkBackendConfig::Mock));
        }

        if let Some(rpc) = rpc.clone() {
            configs.push((
                ZkBackend::DryRun,
                SuccinctZkBackendConfig::DryRun { rpc, range_cycle_limit: self.range_cycle_limit },
            ));
        }

        if let Some(config) = cluster {
            configs.push((ZkBackend::Cluster, SuccinctZkBackendConfig::Cluster(config)));
        }

        if let Some(config) = network {
            configs.push((ZkBackend::Network, SuccinctZkBackendConfig::Network(config)));
        }

        if configs.is_empty() {
            return Err(SuccinctZkProverBuildError::config(
                "no ZK backend enabled; configure RPC URLs or explicitly enable the mock backend",
            ));
        }

        Ok((configs, rpc))
    }
}

/// Builds concrete Succinct ZK prover backends from config.
#[derive(Clone, Debug)]
pub struct SuccinctZkProverBuilder {
    config: SuccinctZkBackendConfig,
    witness_provider: Option<OpSuccinctWitnessProvider>,
}

impl SuccinctZkProverBuilder {
    /// Creates a builder for a Succinct ZK prover backend.
    pub const fn new(config: SuccinctZkBackendConfig) -> Self {
        Self { config, witness_provider: None }
    }

    /// Reuses an already initialized witness provider.
    ///
    /// The provider must use the same RPC endpoints as this builder's backend config.
    #[must_use]
    pub fn with_witness_provider(mut self, witness_provider: OpSuccinctWitnessProvider) -> Self {
        self.witness_provider = Some(witness_provider);
        self
    }

    /// Builds the configured prover unless cancellation is requested first.
    pub async fn build_until_cancelled(
        self,
        cancel: &CancellationToken,
    ) -> Result<Option<Arc<dyn ZkProver>>, SuccinctZkProverBuildError> {
        if cancel.is_cancelled() {
            return Ok(None);
        }

        match self.config {
            SuccinctZkBackendConfig::Mock => Ok(Some(Arc::new(MockZkProver))),
            SuccinctZkBackendConfig::DryRun { rpc, range_cycle_limit } => {
                DryRunZkProver::build_until_cancelled(
                    rpc,
                    range_cycle_limit,
                    self.witness_provider,
                    cancel,
                )
                .await
            }
            SuccinctZkBackendConfig::Cluster(config) => {
                ClusterZkProver::build_until_cancelled(config, self.witness_provider, cancel).await
            }
            SuccinctZkBackendConfig::Network(config) => {
                NetworkZkProver::build_until_cancelled(config, self.witness_provider, cancel).await
            }
        }
    }

    /// Builds a Succinct witness provider.
    pub async fn build_witness_provider(
        rpc: SuccinctRpcConfig,
        cancel: &CancellationToken,
    ) -> Result<Option<OpSuccinctWitnessProvider>, SuccinctZkProverBuildError> {
        let rpc_config = RPCConfig {
            l1_rpc: rpc.l1_rpc,
            l1_beacon_rpc: Some(rpc.l1_beacon_rpc),
            l2_rpc: rpc.l2_rpc,
            l2_node_rpc: rpc.base_consensus_rpc,
        };
        let Some(fetcher) = Self::complete_unless_cancelled(
            cancel,
            async {
                OPSuccinctDataFetcher::from_rpc_config_with_rollup_config(rpc_config).await.map_err(
                    |error| {
                        SuccinctZkProverBuildError::boxed_operation(
                            "failed to create OPSuccinctDataFetcher",
                            error.into_boxed_dyn_error(),
                        )
                    },
                )
            },
            "op_succinct_data_fetcher",
        )
        .await?
        else {
            return Ok(None);
        };
        let fetcher = Arc::new(fetcher);

        Ok(Some(OpSuccinctWitnessProvider::new(fetcher)))
    }

    /// Completes an initialization operation unless cancellation is requested.
    pub async fn complete_unless_cancelled<F, T>(
        cancel: &CancellationToken,
        operation: F,
        operation_name: &'static str,
    ) -> Result<Option<T>, SuccinctZkProverBuildError>
    where
        F: Future<Output = Result<T, SuccinctZkProverBuildError>> + Send,
    {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                info!(
                    operation = %operation_name,
                    "cancelled Succinct backend initialization operation"
                );
                Ok(None)
            }
            result = operation => result.map(Some),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SuccinctZkProversConfig {
        SuccinctZkProversConfig {
            enable_mock: false,
            base_consensus_rpc: None,
            l1_rpc: None,
            l1_beacon_rpc: None,
            l2_rpc: None,
            default_sequence_window: 50,
            cluster_rpc: None,
            cluster_timeout_hours: 24,
            s3_bucket: None,
            s3_region: None,
            network_private_key: None,
            use_kms_requester: false,
            network_timeout_hours: 24,
            range_cycle_limit: 1_000_000_000_000,
            range_gas_limit: 1_000_000_000_000,
            aggregation_cycle_limit: 1_000_000_000_000,
            aggregation_gas_limit: 1_000_000_000_000,
        }
    }

    fn set_rpc_config(config: &mut SuccinctZkProversConfig) {
        config.base_consensus_rpc = Some(Url::parse("http://base-consensus").unwrap());
        config.l1_rpc = Some(Url::parse("http://l1").unwrap());
        config.l1_beacon_rpc = Some(Url::parse("http://l1-beacon").unwrap());
        config.l2_rpc = Some(Url::parse("http://l2").unwrap());
    }

    #[test]
    fn backend_enablement_is_presence_based() {
        let mut config = config();
        assert_eq!(
            config.backend_configs().unwrap_err().to_string(),
            "configuration error: no ZK backend enabled; configure RPC URLs or explicitly enable the mock backend"
        );

        config.use_kms_requester = true;
        assert!(config.backend_configs().is_err());
        config.use_kms_requester = false;

        config.network_private_key = Some("network-key".to_owned());
        assert!(config.backend_configs().is_err());
        config.network_private_key = None;

        config.cluster_rpc = Some("http://cluster".to_owned());
        assert!(config.backend_configs().is_err());
        config.cluster_rpc = None;

        config.enable_mock = true;
        let (configs, _) = config.backend_configs().unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].0, ZkBackend::Mock);

        config.enable_mock = false;
        set_rpc_config(&mut config);
        let (configs, _) = config.backend_configs().unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].0, ZkBackend::DryRun);

        config.base_consensus_rpc = None;
        assert!(config.backend_configs().is_err());

        config.enable_mock = true;
        let (configs, _) = config.backend_configs().unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].0, ZkBackend::Mock);

        config.s3_bucket = Some("bucket".to_owned());
        config.s3_region = Some("region".to_owned());
        let (configs, _) = config.backend_configs().unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].0, ZkBackend::Mock);

        set_rpc_config(&mut config);
        config.cluster_rpc = Some("http://cluster".to_owned());
        config.network_private_key = Some("network-key".to_owned());
        let (configs, _) = config.backend_configs().unwrap();
        assert_eq!(
            configs.iter().map(|(backend, _)| *backend).collect::<Vec<_>>(),
            vec![ZkBackend::Mock, ZkBackend::DryRun, ZkBackend::Cluster, ZkBackend::Network]
        );
    }
}
