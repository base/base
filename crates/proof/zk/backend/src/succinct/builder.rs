//! Backend construction for Succinct ZK provers.

use std::{error::Error, future::Future, sync::Arc};

use base_proof_succinct_host_utils::fetcher::{OPSuccinctDataFetcher, RPCConfig};
use base_proof_zk_host::ZkProver;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::info;
use url::Url;

use crate::succinct::{
    ClusterZkProver, DryRunZkProver, MockZkProver, NetworkZkProver, OpSuccinctWitnessProvider,
    SuccinctClusterBackendConfig, SuccinctNetworkBackendConfig,
};

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

/// Builds concrete Succinct ZK prover backends from config.
#[derive(Clone, Debug)]
pub struct SuccinctZkProverBuilder {
    config: SuccinctZkBackendConfig,
}

impl SuccinctZkProverBuilder {
    /// Creates a builder for a Succinct ZK prover backend.
    pub const fn new(config: SuccinctZkBackendConfig) -> Self {
        Self { config }
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
                DryRunZkProver::build_until_cancelled(rpc, range_cycle_limit, cancel).await
            }
            SuccinctZkBackendConfig::Cluster(config) => {
                ClusterZkProver::build_until_cancelled(config, cancel).await
            }
            SuccinctZkBackendConfig::Network(config) => {
                NetworkZkProver::build_until_cancelled(config, cancel).await
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
