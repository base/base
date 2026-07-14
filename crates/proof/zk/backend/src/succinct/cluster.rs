//! SP1 cluster [`ZkProver`] backend.
//!
//! This backend submits compressed range proofs to a self-hosted SP1 cluster and
//! stores the cluster proof output artifact id inside the backend session id so
//! the stateless worker can later download the completed proof.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use base_proof_succinct_client_utils::client::DEFAULT_INTERMEDIATE_ROOT_INTERVAL;
use base_proof_succinct_proof_utils::{ClusterArtifactStore, ClusterProofConfig};
use base_proof_zk_host::{ZkProver, ZkProverError, ZkSessionState};
use base_prover_service_protocol::{
    ProofResult, SessionType, SnarkPlonkProofRequest, SnarkPlonkProofResult, ZkProofRequest,
    ZkProofResult, ZkVm,
};
use serde::{Deserialize, Serialize};
use sp1_cluster_common::{
    client::ClusterServiceClient,
    proto::{
        ExecutionFailureCause, ExecutionStatus, ProofRequest as ClusterProtoProofRequest,
        ProofRequestCreateRequest, ProofRequestGetRequest, ProofRequestStatus,
    },
};
use sp1_prover_types::{Artifact, ArtifactClient as _, ArtifactType};
use sp1_sdk::{
    HashableKey, ProofFromNetwork, SP1ProofWithPublicValues, SP1Stdin, SP1VerifyingKey,
    network::proto::types::ProofMode,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::succinct::{
    L1HeadSource, OpSuccinctWitnessProvider, SuccinctRpcConfig, SuccinctZkProverBuildError,
    SuccinctZkProverBuilder, WitnessParams,
};

macro_rules! backend_error {
    ($($arg:tt)*) => {
        ZkProverError::Backend(std::io::Error::other(format!($($arg)*)).into())
    };
}

/// SP1 cluster backend settings.
#[derive(Clone, Debug)]
pub struct SuccinctClusterBackendConfig {
    /// Shared RPC settings.
    pub rpc: SuccinctRpcConfig,
    /// SP1 cluster gRPC endpoint.
    pub cluster_rpc: String,
    /// S3 artifact store bucket.
    pub s3_bucket: String,
    /// S3 artifact store region.
    pub s3_region: String,
    /// Proof timeout.
    pub timeout: Duration,
    /// Cycle limit for range proof requests.
    pub range_cycle_limit: u64,
    /// Gas limit for range proof requests.
    pub range_gas_limit: u64,
    /// Cycle limit for aggregation proof requests.
    pub aggregation_cycle_limit: u64,
    /// Gas limit for aggregation proof requests.
    pub aggregation_gas_limit: u64,
}

/// Configuration for [`ClusterZkProver`].
#[derive(Clone)]
pub struct ClusterZkProverConfig {
    /// Base consensus node RPC URL.
    pub base_consensus_url: String,
    /// L1 execution node RPC URL.
    pub l1_node_url: String,
    /// Default sequence window for L1 head calculations.
    pub default_sequence_window: u64,
    /// Cluster proof submission and artifact configuration.
    pub cluster: Arc<ClusterProofConfig>,
    /// Range program verification key used by the aggregation program.
    pub range_vk: Arc<SP1VerifyingKey>,
    /// Proof timeout.
    pub timeout: Duration,
    /// Cycle limit for range proof requests.
    pub range_cycle_limit: u64,
    /// Gas limit for range proof requests.
    pub range_gas_limit: u64,
    /// Cycle limit for aggregation proof requests.
    pub aggregation_cycle_limit: u64,
    /// Gas limit for aggregation proof requests.
    pub aggregation_gas_limit: u64,
}

impl std::fmt::Debug for ClusterZkProverConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterZkProverConfig")
            .field("default_sequence_window", &self.default_sequence_window)
            .field("range_vk", &self.range_vk.bytes32())
            .field("timeout", &self.timeout)
            .field("range_cycle_limit", &self.range_cycle_limit)
            .field("range_gas_limit", &self.range_gas_limit)
            .field("aggregation_cycle_limit", &self.aggregation_cycle_limit)
            .field("aggregation_gas_limit", &self.aggregation_gas_limit)
            .finish_non_exhaustive()
    }
}

/// Backend session id persisted by the prover-service worker API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterSessionId {
    /// Cluster proof request id.
    pub proof_id: String,
    /// Cluster proof output artifact id.
    pub proof_output_id: String,
}

impl ClusterSessionId {
    /// Serialize this handle into the backend session id string.
    pub fn to_backend_session_id(&self) -> Result<String, ZkProverError> {
        serde_json::to_string(self)
            .map_err(|e| backend_error!("failed to encode cluster session id: {e}"))
    }

    /// Parse this handle from the backend session id string.
    pub fn parse(value: &str) -> Result<Self, ZkProverError> {
        serde_json::from_str(value).map_err(|e| backend_error!("invalid cluster session id: {e}"))
    }
}

/// [`ZkProver`] backed by an SP1 prover cluster.
#[derive(Clone)]
pub struct ClusterZkProver {
    provider: OpSuccinctWitnessProvider,
    config: ClusterZkProverConfig,
}

impl std::fmt::Debug for ClusterZkProver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterZkProver").field("config", &self.config).finish_non_exhaustive()
    }
}

impl ClusterZkProver {
    /// Maximum number of deterministic cluster proof ids to try for one request.
    pub const MAX_SUBMIT_ATTEMPTS: u64 = 8;

    /// Create a cluster prover with a witness provider and cluster config.
    pub const fn new(provider: OpSuccinctWitnessProvider, config: ClusterZkProverConfig) -> Self {
        Self { provider, config }
    }

    /// Builds an SP1 cluster backend.
    pub async fn build_until_cancelled(
        config: SuccinctClusterBackendConfig,
        witness_provider: Option<OpSuccinctWitnessProvider>,
        cancel: &CancellationToken,
    ) -> Result<Option<Arc<dyn ZkProver>>, SuccinctZkProverBuildError> {
        let SuccinctClusterBackendConfig {
            rpc,
            cluster_rpc,
            s3_bucket,
            s3_region,
            timeout,
            range_cycle_limit,
            range_gas_limit,
            aggregation_cycle_limit,
            aggregation_gas_limit,
        } = config;
        let base_consensus_url = rpc.base_consensus_rpc.as_str().to_owned();
        let l1_node_url = rpc.l1_rpc.as_str().to_owned();
        let default_sequence_window = rpc.default_sequence_window;

        info!(backend = "cluster", "using Succinct SP1 cluster backend");
        info!("computing range verification key");
        let Some((range_vk, _aggregation_vk)) = SuccinctZkProverBuilder::complete_unless_cancelled(
            cancel,
            async {
                base_proof_succinct_proof_utils::cluster_setup_vkeys().await.map_err(|error| {
                    SuccinctZkProverBuildError::boxed_operation(
                        "failed to compute proof verification keys",
                        error.into_boxed_dyn_error(),
                    )
                })
            },
            "proof_verification_keys",
        )
        .await?
        else {
            return Ok(None);
        };
        info!("range verification key computed successfully");

        let provider = match witness_provider {
            Some(provider) => provider,
            None => {
                let Some(provider) =
                    SuccinctZkProverBuilder::build_witness_provider(rpc, cancel).await?
                else {
                    return Ok(None);
                };
                provider
            }
        };
        let Some((artifact_store, artifact_store_config)) =
            Self::s3_artifact_store(s3_bucket, s3_region, cancel).await?
        else {
            return Ok(None);
        };
        let Some(service_client) = SuccinctZkProverBuilder::complete_unless_cancelled(
            cancel,
            async {
                // The upstream constructor consumes the endpoint string, and the proof config
                // needs to retain it for request submission.
                ClusterServiceClient::new(cluster_rpc.clone()).await.map_err(|error| {
                    SuccinctZkProverBuildError::boxed_operation(
                        "failed to create SP1 cluster client",
                        error.into(),
                    )
                })
            },
            "sp1_cluster_client",
        )
        .await?
        else {
            return Ok(None);
        };
        let prover_config = ClusterZkProverConfig {
            base_consensus_url,
            l1_node_url,
            default_sequence_window,
            cluster: Arc::new(ClusterProofConfig {
                cluster_rpc,
                artifact_store,
                artifact_store_config,
                service_client,
            }),
            range_vk: range_vk.into(),
            timeout,
            range_cycle_limit,
            range_gas_limit,
            aggregation_cycle_limit,
            aggregation_gas_limit,
        };

        Ok(Some(Arc::new(Self::new(provider, prover_config))))
    }

    /// Builds S3 artifact storage for SP1 cluster requests.
    pub async fn s3_artifact_store(
        bucket: String,
        region: String,
        cancel: &CancellationToken,
    ) -> Result<
        Option<(ClusterArtifactStore, sp1_cluster_utils::ArtifactStoreConfig)>,
        SuccinctZkProverBuildError,
    > {
        info!("using S3 artifact storage");
        let Some(download_client) = SuccinctZkProverBuilder::complete_unless_cancelled(
            cancel,
            async {
                Ok(sp1_cluster_artifact::s3::S3ArtifactClient::create_s3_sdk_download_client(
                    region.clone(),
                )
                .await)
            },
            "s3_download_client",
        )
        .await?
        else {
            return Ok(None);
        };
        let download_mode = sp1_cluster_artifact::s3::S3DownloadMode::AwsSDK(download_client);
        let bucket_for_config = bucket.clone();
        let region_for_config = region.clone();
        let Some(client) = SuccinctZkProverBuilder::complete_unless_cancelled(
            cancel,
            async move {
                Ok(sp1_cluster_artifact::s3::S3ArtifactClient::new(
                    region,
                    bucket,
                    32,
                    download_mode,
                )
                .await)
            },
            "s3_artifact_client",
        )
        .await?
        else {
            return Ok(None);
        };

        Ok(Some((
            ClusterArtifactStore::S3(client),
            sp1_cluster_utils::ArtifactStoreConfig::S3 {
                bucket: bucket_for_config,
                region: region_for_config,
            },
        )))
    }

    /// Build the cluster proof id for a prover-service session attempt.
    pub fn proof_id_for_attempt(request_session_id: &str, attempt: u64) -> String {
        let base_proof_id = format!("prover_service_{request_session_id}");
        if attempt == 0 {
            return base_proof_id;
        }

        format!("{base_proof_id}_retry_{attempt}")
    }

    /// Build the deterministic cluster aggregation proof id for a prover-service session attempt.
    pub fn aggregation_proof_id_for_attempt(request_session_id: &str, attempt: u64) -> String {
        let base_proof_id = format!("prover_service_{request_session_id}_aggregation");
        if attempt == 0 {
            return base_proof_id;
        }

        format!("{base_proof_id}_retry_{attempt}")
    }

    /// Build a detailed failure reason from a cluster proof request proto.
    pub fn format_cluster_failure(req: &ClusterProtoProofRequest) -> String {
        let mut parts = vec!["proof generation failed".to_owned()];

        if let Some(exec) = &req.execution_result {
            let exec_status = ExecutionStatus::try_from(exec.status)
                .map(|s| s.as_str_name().to_owned())
                .unwrap_or_else(|_| format!("UNKNOWN({})", exec.status));
            parts.push(format!("execution_status={exec_status}"));

            let cause = ExecutionFailureCause::try_from(exec.failure_cause)
                .map(|c| c.as_str_name().to_owned())
                .unwrap_or_else(|_| format!("UNKNOWN({})", exec.failure_cause));
            if exec.failure_cause != 0 {
                parts.push(format!("failure_cause={cause}"));
            }

            if exec.cycles > 0 {
                parts.push(format!("cycles={}", exec.cycles));
            }
            if exec.gas > 0 {
                parts.push(format!("gas={}", exec.gas));
            }
        }

        if !req.metadata.is_empty() {
            parts.push(format!("metadata={}", req.metadata));
        }
        if let Some(extra) = &req.extra_data
            && !extra.is_empty()
        {
            parts.push(format!("extra_data={extra}"));
        }

        parts.join("; ")
    }

    /// Fetch a cluster proof request by id.
    pub async fn get_cluster_request(
        &self,
        proof_id: &str,
    ) -> Result<Option<ClusterProtoProofRequest>, ZkProverError> {
        let request = ProofRequestGetRequest { proof_id: proof_id.to_owned() };
        backoff::future::retry(self.config.cluster.service_client.backoff.clone(), || async {
            let mut client = self.config.cluster.service_client.rpc.clone();
            match client.proof_request_get(request.clone()).await {
                Ok(response) => Ok(response.into_inner().proof_request),
                Err(status) => {
                    if Self::is_missing_cluster_request_status(
                        i32::from(status.code()),
                        status.message(),
                    ) {
                        return Ok(None);
                    }

                    Err(sp1_cluster_common::util::status_to_backoff_error(status))
                }
            }
        })
        .await
        .map_err(|e| backend_error!("failed to get cluster proof: {e}"))
    }

    /// Fetch a cluster proof request by id without the cluster client's retry policy.
    pub async fn get_cluster_request_once(
        &self,
        proof_id: &str,
    ) -> Result<Option<ClusterProtoProofRequest>, ZkProverError> {
        let request = ProofRequestGetRequest { proof_id: proof_id.to_owned() };
        let mut client = self.config.cluster.service_client.rpc.clone();
        match client.proof_request_get(request).await {
            Ok(response) => Ok(response.into_inner().proof_request),
            Err(status) => {
                if Self::is_missing_cluster_request_status(
                    i32::from(status.code()),
                    status.message(),
                ) {
                    return Ok(None);
                }

                Err(backend_error!("failed to get cluster proof: {status}"))
            }
        }
    }

    /// Returns true if a cluster gRPC status represents a missing proof request.
    pub fn is_missing_cluster_request_status(code: i32, message: &str) -> bool {
        if code == i32::from(tonic::Code::NotFound) {
            return true;
        }

        // Self-hosted sp1-cluster v2.3.2 reports missing rows as Internal.
        if code == i32::from(tonic::Code::Internal)
            && message.contains("Failed to get proof request")
            && message.contains("no rows returned")
        {
            warn!(
                grpc_code = code,
                error_message = %message,
                "cluster proof request missing row returned as internal error"
            );
            return true;
        }

        false
    }

    /// Decode the cluster proof request status without mapping unknown values to unspecified.
    pub fn cluster_proof_status(
        req: &ClusterProtoProofRequest,
    ) -> Result<ProofRequestStatus, ZkProverError> {
        ProofRequestStatus::try_from(req.proof_status).map_err(|_| {
            backend_error!("cluster proof {} has unknown proof status {}", req.id, req.proof_status)
        })
    }

    /// Validate that an existing cluster request is safe to resume after create raced.
    pub fn validate_existing_cluster_request(
        req: &ClusterProtoProofRequest,
    ) -> Result<ProofRequestStatus, ZkProverError> {
        let proof_status = Self::cluster_proof_status(req)?;
        match proof_status {
            ProofRequestStatus::Failed | ProofRequestStatus::Cancelled => {
                error!(
                    proof_id = %req.id,
                    proof_status = %proof_status.as_str_name(),
                    "cluster proof created concurrently but already terminal"
                );
                return Err(backend_error!(
                    "cluster proof {} created concurrently but already terminal: {}",
                    req.id,
                    proof_status.as_str_name()
                ));
            }
            ProofRequestStatus::Unspecified => {
                error!(
                    proof_id = %req.id,
                    "cluster proof created concurrently but has unspecified status"
                );
                return Err(backend_error!(
                    "cluster proof {} created concurrently but has unspecified status",
                    req.id
                ));
            }
            ProofRequestStatus::Pending => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|e| backend_error!("invalid unix timestamp: {e}"))?
                    .as_secs();
                if req.deadline <= now {
                    error!(
                        proof_id = %req.id,
                        deadline = req.deadline,
                        now = now,
                        "cluster proof created concurrently but deadline already elapsed"
                    );
                    return Err(backend_error!(
                        "cluster proof {} created concurrently but deadline already elapsed",
                        req.id
                    ));
                }
            }
            ProofRequestStatus::Completed => {}
        }

        Ok(proof_status)
    }

    /// Convert a cluster proof request into a persisted session id.
    pub fn session_id_from_request(
        req: &ClusterProtoProofRequest,
    ) -> Result<ClusterSessionId, ZkProverError> {
        let proof_output_id = req
            .proof_artifact_id
            .clone()
            .ok_or_else(|| backend_error!("cluster proof {} has no proof artifact id", req.id))?;
        Ok(ClusterSessionId { proof_id: req.id.clone(), proof_output_id })
    }

    /// Find a usable cluster proof id, scanning candidates from `id_for_attempt` and skipping any
    /// terminal or deadline-elapsed request. Returns `(id, None)` for a free id, or `(id,
    /// Some(backend_session_id))` when a still-usable request can be reused directly.
    pub async fn find_available_proof_id(
        &self,
        request_session_id: &str,
        stage: &str,
        id_for_attempt: impl Fn(&str, u64) -> String,
    ) -> Result<(String, Option<String>), ZkProverError> {
        for attempt in 0..Self::MAX_SUBMIT_ATTEMPTS {
            let candidate = id_for_attempt(request_session_id, attempt);
            let Some(existing) = self.get_cluster_request_once(&candidate).await? else {
                return Ok((candidate, None));
            };

            let proof_status = Self::cluster_proof_status(&existing)?;
            match proof_status {
                ProofRequestStatus::Failed | ProofRequestStatus::Cancelled => {
                    info!(
                        proof_id = %candidate,
                        stage = stage,
                        proof_status = %proof_status.as_str_name(),
                        "existing cluster request is terminal, trying next proof id"
                    );
                }
                ProofRequestStatus::Pending => {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_err(|e| backend_error!("invalid unix timestamp: {e}"))?
                        .as_secs();
                    if existing.deadline <= now {
                        info!(
                            proof_id = %candidate,
                            stage = stage,
                            deadline = existing.deadline,
                            now = now,
                            "existing cluster request deadline elapsed, trying next proof id"
                        );
                        continue;
                    }

                    info!(proof_id = %candidate, stage = stage, "cluster request already exists");
                    let backend_session_id =
                        Self::session_id_from_request(&existing)?.to_backend_session_id()?;
                    return Ok((candidate, Some(backend_session_id)));
                }
                ProofRequestStatus::Unspecified => {
                    return Err(backend_error!(
                        "cluster {stage} proof {candidate} has unspecified status"
                    ));
                }
                ProofRequestStatus::Completed => {
                    info!(proof_id = %candidate, stage = stage, "cluster request already exists");
                    let backend_session_id =
                        Self::session_id_from_request(&existing)?.to_backend_session_id()?;
                    return Ok((candidate, Some(backend_session_id)));
                }
            }
        }

        Err(backend_error!(
            "exhausted {} cluster {stage} submit attempts for request session id {request_session_id}",
            Self::MAX_SUBMIT_ATTEMPTS
        ))
    }

    /// Submit a compressed range proof to the cluster.
    pub async fn submit_range_proof(
        &self,
        request: &ZkProofRequest,
        request_session_id: &str,
    ) -> Result<String, ZkProverError> {
        let (proof_id, existing_backend_session_id) = self
            .find_available_proof_id(request_session_id, "range", Self::proof_id_for_attempt)
            .await?;
        if let Some(backend_session_id) = existing_backend_session_id {
            return Ok(backend_session_id);
        }

        let start_block = request.start_block_number;
        let end_block = start_block
            .checked_add(request.number_of_blocks_to_prove)
            .ok_or_else(|| backend_error!("proof range end block overflowed u64"))?;
        let sequence_window =
            request.sequence_window.unwrap_or(self.config.default_sequence_window);
        let intermediate_root_interval =
            request.intermediate_root_interval.unwrap_or(DEFAULT_INTERMEDIATE_ROOT_INTERVAL);

        info!(
            proof_id = %proof_id,
            start_block = start_block,
            end_block = end_block,
            number_of_blocks = request.number_of_blocks_to_prove,
            sequence_window = sequence_window,
            intermediate_root_interval = intermediate_root_interval,
            l1_head = ?request.l1_head,
            "starting SP1 cluster range proof generation"
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
            proof_id = %proof_id,
            witness_gen_duration_ms = witness_gen_duration_ms,
            timeout_secs = self.config.timeout.as_secs(),
            range_cycle_limit = self.config.range_cycle_limit,
            range_gas_limit = self.config.range_gas_limit,
            "witness generated, submitting range proof to SP1 cluster"
        );

        let session = self.create_cluster_request(proof_id, stdin, ProofMode::Compressed).await?;
        let backend_session_id = session.to_backend_session_id()?;
        info!(
            proof_id = %session.proof_id,
            proof_output_id = %session.proof_output_id,
            "proof request submitted to SP1 cluster"
        );

        Ok(backend_session_id)
    }

    /// Upload the stage artifacts and create the cluster proof request, dispatching on the
    /// configured artifact store.
    pub async fn create_cluster_request(
        &self,
        proof_id: String,
        stdin: SP1Stdin,
        proof_mode: ProofMode,
    ) -> Result<ClusterSessionId, ZkProverError> {
        match &self.config.cluster.artifact_store {
            ClusterArtifactStore::Redis(client) => {
                self.create_cluster_request_with_client(client.clone(), proof_id, stdin, proof_mode)
                    .await
            }
            ClusterArtifactStore::S3(client) => {
                self.create_cluster_request_with_client(client.clone(), proof_id, stdin, proof_mode)
                    .await
            }
        }
    }

    /// Upload the stage artifacts with the provided client and create the cluster request. The
    /// `proof_mode` selects the stage: `Compressed` for the range proof, `Plonk` for aggregation.
    pub async fn create_cluster_request_with_client<A>(
        &self,
        artifact_client: A,
        proof_id: String,
        stdin: SP1Stdin,
        proof_mode: ProofMode,
    ) -> Result<ClusterSessionId, ZkProverError>
    where
        A: sp1_prover_types::ArtifactClient,
    {
        let (elf, cycle_limit, gas_limit, stage) = match proof_mode {
            ProofMode::Compressed => (
                base_proof_succinct_elfs::RANGE_ELF_EMBEDDED,
                self.config.range_cycle_limit,
                self.config.range_gas_limit,
                "range",
            ),
            ProofMode::Plonk => (
                base_proof_succinct_elfs::AGGREGATION_ELF,
                self.config.aggregation_cycle_limit,
                self.config.aggregation_gas_limit,
                "aggregation",
            ),
            other => {
                return Err(backend_error!(
                    "unsupported cluster proof mode: {}",
                    other.as_str_name()
                ));
            }
        };

        let elf_id = artifact_client
            .create_artifact()
            .map_err(|e| backend_error!("failed to create {stage} ELF artifact: {e}"))?;
        artifact_client
            .upload_with_type(&elf_id, ArtifactType::Program, elf.to_vec())
            .await
            .map_err(|e| backend_error!("failed to upload {stage} ELF: {e}"))?;

        let stdin_id = artifact_client
            .create_artifact()
            .map_err(|e| backend_error!("failed to create {stage} stdin artifact: {e}"))?;
        artifact_client
            .upload_with_type(&stdin_id, ArtifactType::Stdin, stdin)
            .await
            .map_err(|e| backend_error!("failed to upload {stage} stdin: {e}"))?;

        let proof_output_id = artifact_client
            .create_artifact()
            .map_err(|e| backend_error!("failed to create {stage} proof artifact: {e}"))?;
        let proof_output_id = proof_output_id.to_id();
        let deadline = SystemTime::now() + self.config.timeout;
        let deadline = deadline
            .duration_since(UNIX_EPOCH)
            .map_err(|e| backend_error!("invalid cluster {stage} deadline: {e}"))?
            .as_secs();

        let request = ProofRequestCreateRequest {
            proof_id: proof_id.clone(),
            program_artifact_id: elf_id.to_id(),
            stdin_artifact_id: stdin_id.to_id(),
            options_artifact_id: Some((proof_mode as i32).to_string()),
            proof_artifact_id: Some(proof_output_id.clone()),
            requester: vec![],
            deadline,
            cycle_limit,
            gas_limit,
            scheduled_by: None,
            stdin_private: false,
        };

        match self.config.cluster.service_client.create_proof_request(request).await {
            Ok(()) => Ok(ClusterSessionId { proof_id, proof_output_id }),
            Err(e) => {
                if let Some(existing) = self.get_cluster_request(&proof_id).await? {
                    let proof_status = Self::validate_existing_cluster_request(&existing)
                        .inspect_err(|_| {
                            error!(
                                proof_id = %proof_id,
                                stage = stage,
                                error = %e,
                                "cluster proof create raced into an unusable existing request"
                            );
                        })?;

                    info!(
                        proof_id = %proof_id,
                        stage = stage,
                        proof_status = %proof_status.as_str_name(),
                        error = %e,
                        "cluster proof create raced, using existing request"
                    );
                    return Self::session_id_from_request(&existing);
                }

                error!(
                    proof_id = %proof_id,
                    stage = stage,
                    error = %e,
                    "cluster proof create failed with no existing request"
                );
                Err(backend_error!("failed to create cluster {stage} proof: {e}"))
            }
        }
    }

    /// Submit the PLONK aggregation proof after the compressed range proof completes.
    pub async fn submit_aggregation_proof(
        &self,
        request: &SnarkPlonkProofRequest,
        request_session_id: &str,
        range_backend_session_id: &str,
    ) -> Result<String, ZkProverError> {
        let (proof_id, existing_backend_session_id) = self
            .find_available_proof_id(
                request_session_id,
                "aggregation",
                Self::aggregation_proof_id_for_attempt,
            )
            .await?;
        if let Some(backend_session_id) = existing_backend_session_id {
            return Ok(backend_session_id);
        }

        let range_session = ClusterSessionId::parse(range_backend_session_id)?;
        let witness_start = std::time::Instant::now();
        let range_proof = self.download_cluster_proof(&range_session).await?;
        let stdin = self
            .provider
            .generate_aggregation_witness(
                range_proof,
                self.config.range_vk.as_ref(),
                request.prover_address,
            )
            .await
            .map_err(|e| backend_error!("aggregation witness generation failed: {e}"))?;
        let witness_gen_duration_ms = witness_start.elapsed().as_secs_f64() * 1000.0;

        let session = self.create_cluster_request(proof_id, stdin, ProofMode::Plonk).await?;
        let backend_session_id = session.to_backend_session_id()?;
        info!(
            proof_id = %session.proof_id,
            proof_output_id = %session.proof_output_id,
            witness_gen_duration_ms = witness_gen_duration_ms,
            cycle_limit = self.config.aggregation_cycle_limit,
            gas_limit = self.config.aggregation_gas_limit,
            "aggregation proof request submitted to SP1 cluster"
        );

        Ok(backend_session_id)
    }

    /// Download the proof output artifact.
    pub async fn download_cluster_proof(
        &self,
        session: &ClusterSessionId,
    ) -> Result<SP1ProofWithPublicValues, ZkProverError> {
        let proof_output_id = Artifact::from(session.proof_output_id.clone());
        let proof: ProofFromNetwork = match &self.config.cluster.artifact_store {
            ClusterArtifactStore::Redis(client) => {
                client.download_with_type(&proof_output_id, ArtifactType::Proof).await
            }
            ClusterArtifactStore::S3(client) => {
                client.download_with_type(&proof_output_id, ArtifactType::Proof).await
            }
        }
        .map_err(|e| backend_error!("failed to download cluster proof artifact: {e}"))?;

        Ok(proof.into())
    }
}

#[async_trait]
impl ZkProver for ClusterZkProver {
    /// Submit the initial range (STARK) proof for a request. The range proof-id namespace is keyed
    /// on `request_session_id`, which the orchestrator maps to exactly one request kind; aggregation
    /// runs later under a distinct `_aggregation` proof id (see `aggregation_proof_id_for_attempt`).
    async fn submit(
        &self,
        request: &ZkProofRequest,
        request_session_id: &str,
    ) -> Result<String, ZkProverError> {
        self.submit_range_proof(request, request_session_id).await
    }

    async fn poll(&self, backend_session_id: &str) -> Result<ZkSessionState, ZkProverError> {
        let session = ClusterSessionId::parse(backend_session_id)?;
        let Some(req) = self.get_cluster_request(&session.proof_id).await? else {
            return Ok(ZkSessionState::NotFound);
        };

        let proof_status = Self::cluster_proof_status(&req)?;
        match proof_status {
            ProofRequestStatus::Completed => Ok(ZkSessionState::Completed),
            ProofRequestStatus::Failed => {
                Ok(ZkSessionState::Failed(Self::format_cluster_failure(&req)))
            }
            ProofRequestStatus::Cancelled => {
                Ok(ZkSessionState::Failed("proof generation cancelled".to_owned()))
            }
            ProofRequestStatus::Pending => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|e| backend_error!("invalid unix timestamp: {e}"))?
                    .as_secs();
                if req.deadline <= now {
                    warn!(
                        proof_id = %session.proof_id,
                        deadline = req.deadline,
                        now = now,
                        "cluster proof request deadline elapsed"
                    );
                    return Ok(ZkSessionState::Failed(format!(
                        "cluster proof {} deadline elapsed before being picked up",
                        session.proof_id
                    )));
                }

                Ok(ZkSessionState::Running)
            }
            ProofRequestStatus::Unspecified => {
                error!(
                    proof_id = %session.proof_id,
                    proof_status = %proof_status.as_str_name(),
                    "unexpected cluster proof status"
                );
                Err(backend_error!(
                    "unexpected cluster proof status for {}: {}",
                    session.proof_id,
                    proof_status.as_str_name()
                ))
            }
        }
    }

    async fn submit_next(
        &self,
        request: &SnarkPlonkProofRequest,
        request_session_id: &str,
        completed_backend_session_id: &str,
    ) -> Result<String, ZkProverError> {
        self.submit_aggregation_proof(request, request_session_id, completed_backend_session_id)
            .await
    }

    async fn download(
        &self,
        session_type: SessionType,
        backend_session_id: &str,
    ) -> Result<ProofResult, ZkProverError> {
        let session = ClusterSessionId::parse(backend_session_id)?;
        let req = self
            .get_cluster_request(&session.proof_id)
            .await?
            .ok_or_else(|| backend_error!("cluster proof {} not found", session.proof_id))?;
        let proof_status = Self::cluster_proof_status(&req)?;
        if proof_status != ProofRequestStatus::Completed {
            return Err(backend_error!(
                "cluster proof {} is not completed (status: {})",
                session.proof_id,
                proof_status.as_str_name()
            ));
        }

        let proof = self.download_cluster_proof(&session).await?;
        let proof = bincode::serde::encode_to_vec(&proof, bincode::config::standard())
            .map_err(|e| backend_error!("failed to serialize proof: {e}"))?;

        let proof = ZkProofResult { zk_vm: ZkVm::Sp1, proof: proof.into(), execution_stats: None };
        match session_type {
            SessionType::Snark => Ok(ProofResult::SnarkPlonk(SnarkPlonkProofResult { proof })),
            SessionType::Stark => Ok(ProofResult::Compressed(proof)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ClusterSessionId, ClusterZkProver};

    #[test]
    fn cluster_session_id_round_trips_json() {
        let session = ClusterSessionId {
            proof_id: "proof-1".to_owned(),
            proof_output_id: "artifact-1".to_owned(),
        };

        let encoded = session.to_backend_session_id().unwrap();
        let decoded = ClusterSessionId::parse(&encoded).unwrap();

        assert_eq!(decoded, session);
    }

    #[test]
    fn proof_id_for_attempt_uses_retry_suffix_after_first_attempt() {
        let first = ClusterZkProver::proof_id_for_attempt("session-1", 0);
        let retry = ClusterZkProver::proof_id_for_attempt("session-1", 2);

        assert_eq!(first, "prover_service_session-1");
        assert_eq!(retry, "prover_service_session-1_retry_2");
    }

    #[test]
    fn aggregation_proof_id_for_attempt_uses_stage_suffix() {
        let first = ClusterZkProver::aggregation_proof_id_for_attempt("session-1", 0);
        let retry = ClusterZkProver::aggregation_proof_id_for_attempt("session-1", 2);

        assert_eq!(first, "prover_service_session-1_aggregation");
        assert_eq!(retry, "prover_service_session-1_aggregation_retry_2");
    }
}
