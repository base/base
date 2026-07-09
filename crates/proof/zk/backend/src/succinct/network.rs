//! SP1 prover network [`ZkProver`] backend.
//!
//! This backend ports the old SP1 network range-proof path into the new
//! stateless worker shape. `submit` generates the range witness and submits it to
//! the network, `poll` checks the network proof request status, and `download`
//! fetches and serializes the completed proof for `submitProof`.

use std::{fmt, sync::Arc, time::Duration};

use alloy_primitives::B256;
use async_trait::async_trait;
use base_proof_succinct_client_utils::client::DEFAULT_INTERMEDIATE_ROOT_INTERVAL;
use base_proof_zk_host::{ZkProver, ZkProverError, ZkSessionState};
use base_prover_service_protocol::{ProofResult, SessionType, ZkProofRequest, ZkProofResult, ZkVm};
use sp1_sdk::{
    HashableKey, NetworkProver, ProveRequest, Prover, ProverClient, ProvingKey,
    SP1ProofWithPublicValues, SP1ProvingKey,
    network::{
        NetworkMode,
        proto::{
            GetProofRequestStatusResponse,
            types::{FulfillmentStatus, FulfillmentStrategy},
        },
        signer::NetworkSigner,
    },
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::succinct::{
    L1HeadSource, OpSuccinctWitnessProvider, SuccinctRpcConfig, SuccinctZkProverBuildError,
    SuccinctZkProverBuilder, WitnessParams,
};

macro_rules! backend_error {
    ($($arg:tt)*) => {
        ZkProverError::Backend(std::io::Error::other(format!($($arg)*)).into())
    };
}

/// SP1 Network backend settings.
#[derive(Clone)]
pub struct SuccinctNetworkBackendConfig {
    /// Shared RPC settings.
    pub rpc: SuccinctRpcConfig,
    /// SP1 network requester private key, or KMS key ARN when `use_kms_requester` is true.
    pub network_private_key: String,
    /// Use the requester key as an AWS KMS ARN instead of a local private key.
    pub use_kms_requester: bool,
    /// Proof timeout.
    pub timeout: Duration,
    /// Cycle limit for range proof requests.
    pub range_cycle_limit: u64,
    /// Gas limit for range proof requests.
    pub range_gas_limit: u64,
}

impl fmt::Debug for SuccinctNetworkBackendConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SuccinctNetworkBackendConfig")
            .field("rpc", &self.rpc)
            .field("network_private_key", &"<redacted>")
            .field("use_kms_requester", &self.use_kms_requester)
            .field("timeout", &self.timeout)
            .field("range_cycle_limit", &self.range_cycle_limit)
            .field("range_gas_limit", &self.range_gas_limit)
            .finish()
    }
}

/// Configuration for [`NetworkZkProver`].
#[derive(Clone)]
pub struct NetworkZkProverConfig {
    /// Base consensus node RPC URL.
    pub base_consensus_url: String,
    /// L1 execution node RPC URL.
    pub l1_node_url: String,
    /// Default sequence window for L1 head calculations.
    pub default_sequence_window: u64,
    /// Pre-built SP1 network prover.
    pub network_prover: Arc<NetworkProver>,
    /// Range program proving key.
    pub range_pk: Arc<SP1ProvingKey>,
    /// Proof timeout.
    pub timeout: Duration,
    /// Cycle limit for range proof requests.
    pub range_cycle_limit: u64,
    /// Gas limit for range proof requests.
    pub range_gas_limit: u64,
}

impl fmt::Debug for NetworkZkProverConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let range_pk = self.range_pk.verifying_key().bytes32();

        f.debug_struct("NetworkZkProverConfig")
            .field("base_consensus_url", &self.base_consensus_url)
            .field("l1_node_url", &self.l1_node_url)
            .field("default_sequence_window", &self.default_sequence_window)
            .field("range_pk", &range_pk)
            .field("timeout", &self.timeout)
            .field("range_cycle_limit", &self.range_cycle_limit)
            .field("range_gas_limit", &self.range_gas_limit)
            .finish_non_exhaustive()
    }
}

/// [`ZkProver`] backed by the SP1 prover network.
#[derive(Clone)]
pub struct NetworkZkProver {
    provider: OpSuccinctWitnessProvider,
    config: NetworkZkProverConfig,
}

impl std::fmt::Debug for NetworkZkProver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkZkProver").finish_non_exhaustive()
    }
}

impl NetworkZkProver {
    /// Create a network prover with a witness provider and network config.
    pub const fn new(provider: OpSuccinctWitnessProvider, config: NetworkZkProverConfig) -> Self {
        Self { provider, config }
    }

    /// Builds an SP1 Network backend.
    pub async fn build_until_cancelled(
        config: SuccinctNetworkBackendConfig,
        cancel: &CancellationToken,
    ) -> Result<Option<Arc<dyn ZkProver>>, SuccinctZkProverBuildError> {
        let SuccinctNetworkBackendConfig {
            rpc,
            network_private_key,
            use_kms_requester,
            timeout,
            range_cycle_limit,
            range_gas_limit,
        } = config;
        let base_consensus_url = rpc.base_consensus_rpc.as_str().to_owned();
        let l1_node_url = rpc.l1_rpc.as_str().to_owned();
        let default_sequence_window = rpc.default_sequence_window;

        info!(backend = "network", "using Succinct SP1 Network backend");
        info!("computing range proving key");
        let Some(range_pk) = SuccinctZkProverBuilder::complete_unless_cancelled(
            cancel,
            async {
                base_proof_succinct_proof_utils::cluster_setup_range_key().await.map_err(|error| {
                    SuccinctZkProverBuildError::boxed_operation(
                        "failed to compute range proving key",
                        error.into_boxed_dyn_error(),
                    )
                })
            },
            "range_proving_key",
        )
        .await?
        else {
            return Ok(None);
        };
        info!("range proving key computed successfully");

        let Some(provider) = SuccinctZkProverBuilder::build_witness_provider(rpc, cancel).await?
        else {
            return Ok(None);
        };

        // This worker always submits public SP1 Network auction requests; reserved and hosted
        // capacity are intentionally not exposed as deployment knobs.
        let fulfillment_strategy = FulfillmentStrategy::Auction;
        let network_mode = NetworkMode::Mainnet;
        let Some(network_signer) =
            Self::network_signer(network_private_key, use_kms_requester, cancel).await?
        else {
            return Ok(None);
        };

        info!(
            network_mode = ?network_mode,
            fulfillment_strategy = ?fulfillment_strategy,
            "creating SP1 Network prover"
        );
        let Some(network_prover) = SuccinctZkProverBuilder::complete_unless_cancelled(
            cancel,
            async move {
                Ok(ProverClient::builder()
                    .network_for(network_mode)
                    .signer(network_signer)
                    .build()
                    .await)
            },
            "sp1_network_prover",
        )
        .await?
        else {
            return Ok(None);
        };
        let network_prover = Arc::new(network_prover);

        let prover_config = NetworkZkProverConfig {
            base_consensus_url,
            l1_node_url,
            default_sequence_window,
            network_prover,
            range_pk: range_pk.into(),
            timeout,
            range_cycle_limit,
            range_gas_limit,
        };

        Ok(Some(Arc::new(Self::new(provider, prover_config))))
    }

    /// Builds an SP1 Network signer.
    pub async fn network_signer(
        key: String,
        use_kms_requester: bool,
        cancel: &CancellationToken,
    ) -> Result<Option<NetworkSigner>, SuccinctZkProverBuildError> {
        if use_kms_requester {
            SuccinctZkProverBuilder::complete_unless_cancelled(
                cancel,
                async {
                    NetworkSigner::aws_kms(&key).await.map_err(|error| {
                        SuccinctZkProverBuildError::operation(
                            "failed to create KMS network signer",
                            error,
                        )
                    })
                },
                "kms_network_signer",
            )
            .await
        } else {
            NetworkSigner::local(&key).map(Some).map_err(|error| {
                SuccinctZkProverBuildError::operation(
                    "failed to create local network signer",
                    error,
                )
            })
        }
    }

    /// Parse a network proof ID from its hex string representation.
    pub fn parse_proof_id(proof_id: &str) -> Result<B256, ZkProverError> {
        proof_id.parse::<B256>().map_err(|e| backend_error!("invalid network proof ID: {e}"))
    }

    /// Map an SP1 Network proof status response to the service session state.
    pub fn session_state(status: &GetProofRequestStatusResponse) -> ZkSessionState {
        match FulfillmentStatus::try_from(status.fulfillment_status()) {
            Ok(FulfillmentStatus::Fulfilled) => ZkSessionState::Completed,
            Ok(FulfillmentStatus::Unfulfillable) => ZkSessionState::Failed(format!(
                "proof unfulfillable, execution_status={}",
                status.execution_status()
            )),
            Ok(_) => ZkSessionState::Running,
            Err(_) => ZkSessionState::Failed(format!(
                "unknown network proof fulfillment status: {}",
                status.fulfillment_status()
            )),
        }
    }

    /// Fetch the network session state and any proof returned by the SP1 Network.
    pub async fn get_network_proof_status(
        &self,
        backend_session_id: &str,
    ) -> Result<(ZkSessionState, Option<SP1ProofWithPublicValues>), ZkProverError> {
        let proof_id = Self::parse_proof_id(backend_session_id)?;
        let (status, proof) = self
            .config
            .network_prover
            .get_proof_status(proof_id)
            .await
            .map_err(|e| backend_error!("failed to get network proof status: {e}"))?;

        Ok((Self::session_state(&status), proof))
    }

    /// Submit a compressed range proof to the SP1 prover network.
    pub async fn submit_range_proof(
        &self,
        request: &ZkProofRequest,
        request_session_id: &str,
    ) -> Result<String, ZkProverError> {
        let start_block = request.start_block_number;
        let end_block = start_block
            .checked_add(request.number_of_blocks_to_prove)
            .ok_or_else(|| backend_error!("proof range end block overflowed u64"))?;
        let sequence_window =
            request.sequence_window.unwrap_or(self.config.default_sequence_window);
        let intermediate_root_interval =
            request.intermediate_root_interval.unwrap_or(DEFAULT_INTERMEDIATE_ROOT_INTERVAL);

        info!(
            request_session_id = %request_session_id,
            start_block = start_block,
            end_block = end_block,
            number_of_blocks = request.number_of_blocks_to_prove,
            sequence_window = sequence_window,
            intermediate_root_interval = intermediate_root_interval,
            l1_head = ?request.l1_head,
            "starting SP1 Network range proof generation"
        );

        let witness_start = std::time::Instant::now();
        let stdin = self
            .provider
            .generate_witness(WitnessParams {
                start_block,
                end_block,
                l1_head: request.l1_head.map_or(
                    L1HeadSource::SequenceWindow {
                        sequence_window,
                        l1_node_url: &self.config.l1_node_url,
                        base_consensus_url: &self.config.base_consensus_url,
                    },
                    L1HeadSource::Pinned,
                ),
                intermediate_root_interval,
            })
            .await
            .map_err(|e| {
                error!(
                    start_block = start_block,
                    end_block = end_block,
                    error = %e,
                    "witness generation failed"
                );
                backend_error!("witness generation failed: {e}")
            })?;
        let witness_gen_duration_ms = witness_start.elapsed().as_secs_f64() * 1000.0;

        info!(
            request_session_id = %request_session_id,
            witness_gen_duration_ms = witness_gen_duration_ms,
            range_cycle_limit = self.config.range_cycle_limit,
            range_gas_limit = self.config.range_gas_limit,
            "witness generated, submitting range proof to SP1 Network"
        );

        let proof_id = self
            .config
            .network_prover
            .prove(self.config.range_pk.as_ref(), stdin)
            .compressed()
            .skip_simulation(true)
            .strategy(FulfillmentStrategy::Auction)
            .timeout(self.config.timeout)
            .cycle_limit(self.config.range_cycle_limit)
            .gas_limit(self.config.range_gas_limit)
            .request()
            .await
            .map_err(|e| {
                error!(error = %e, "failed to submit proof to SP1 Network");
                backend_error!("failed to submit to SP1 Network: {e}")
            })?;

        info!(
            request_session_id = %request_session_id,
            proof_id = %proof_id,
            "proof request submitted to SP1 Network"
        );

        Ok(proof_id.to_string())
    }
}

#[async_trait]
impl ZkProver for NetworkZkProver {
    async fn submit(
        &self,
        request: &ZkProofRequest,
        request_session_id: &str,
    ) -> Result<String, ZkProverError> {
        self.submit_range_proof(request, request_session_id).await
    }

    async fn poll(&self, backend_session_id: &str) -> Result<ZkSessionState, ZkProverError> {
        let (state, _proof) = self.get_network_proof_status(backend_session_id).await?;

        Ok(state)
    }

    async fn download(
        &self,
        _session_type: SessionType,
        backend_session_id: &str,
    ) -> Result<ProofResult, ZkProverError> {
        let (state, proof) = self.get_network_proof_status(backend_session_id).await?;
        let proof = match state {
            ZkSessionState::Completed => proof.ok_or_else(|| {
                backend_error!(
                    "network proof {backend_session_id} is fulfilled but no proof was returned"
                )
            })?,
            ZkSessionState::Running => {
                return Err(backend_error!(
                    "network proof {backend_session_id} is not fulfilled yet"
                ));
            }
            ZkSessionState::Failed(reason) => return Err(backend_error!("{reason}")),
            ZkSessionState::NotFound => {
                return Err(backend_error!("network proof {backend_session_id} was not found"));
            }
        };
        let proof = bincode::serde::encode_to_vec(&proof, bincode::config::standard())
            .map_err(|e| backend_error!("failed to serialize proof: {e}"))?;

        Ok(ProofResult::Compressed(ZkProofResult {
            zk_vm: ZkVm::Sp1,
            proof: proof.into(),
            execution_stats: None,
        }))
    }
}
