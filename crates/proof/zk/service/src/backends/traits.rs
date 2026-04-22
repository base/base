//! Proving backend traits and shared types.

use std::fmt;

use async_trait::async_trait;
use base_zk_client::ProveBlockRequest;
use base_zk_db::{ProofRequest, ProofRequestRepo, ProofSession, ProofStatus, ProofType};
use serde::{Deserialize, Serialize};

/// Wrapper enum for different artifact client implementations.
#[derive(Clone)]
pub enum ArtifactClientWrapper {
    /// Redis-backed artifact storage.
    Redis(sp1_cluster_artifact::redis::RedisArtifactClient),
    /// S3-backed artifact storage.
    S3(sp1_cluster_artifact::s3::S3ArtifactClient),
}

impl fmt::Debug for ArtifactClientWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArtifactClientWrapper").finish_non_exhaustive()
    }
}

impl sp1_prover_types::ArtifactClient for ArtifactClientWrapper {
    async fn upload_raw(
        &self,
        artifact: &impl sp1_prover_types::ArtifactId,
        artifact_type: sp1_prover_types::ArtifactType,
        data: Vec<u8>,
    ) -> anyhow::Result<()> {
        let data_len = data.len();
        tracing::info!(
            artifact_type = %artifact_type,
            artifact_id = %artifact.id(),
            data_bytes = data_len,
            "uploading artifact"
        );
        let result = match self {
            Self::Redis(c) => c.upload_raw(artifact, artifact_type, data).await,
            Self::S3(c) => c.upload_raw(artifact, artifact_type, data).await,
        };
        if let Err(ref e) = result {
            tracing::error!(
                artifact_type = %artifact_type,
                artifact_id = %artifact.id(),
                data_bytes = data_len,
                error = ?e,
                "artifact upload failed"
            );
        }
        result
    }

    async fn download_raw(
        &self,
        artifact: &impl sp1_prover_types::ArtifactId,
        artifact_type: sp1_prover_types::ArtifactType,
    ) -> anyhow::Result<Vec<u8>> {
        match self {
            Self::Redis(c) => c.download_raw(artifact, artifact_type).await,
            Self::S3(c) => c.download_raw(artifact, artifact_type).await,
        }
    }

    async fn exists(
        &self,
        artifact: &impl sp1_prover_types::ArtifactId,
        artifact_type: sp1_prover_types::ArtifactType,
    ) -> anyhow::Result<bool> {
        match self {
            Self::Redis(c) => c.exists(artifact, artifact_type).await,
            Self::S3(c) => c.exists(artifact, artifact_type).await,
        }
    }

    async fn delete(
        &self,
        artifact: &impl sp1_prover_types::ArtifactId,
        artifact_type: sp1_prover_types::ArtifactType,
    ) -> anyhow::Result<()> {
        match self {
            Self::Redis(c) => c.delete(artifact, artifact_type).await,
            Self::S3(c) => c.delete(artifact, artifact_type).await,
        }
    }

    async fn delete_batch(
        &self,
        artifacts: &[impl sp1_prover_types::ArtifactId],
        artifact_type: sp1_prover_types::ArtifactType,
    ) -> anyhow::Result<()> {
        match self {
            Self::Redis(c) => c.delete_batch(artifacts, artifact_type).await,
            Self::S3(c) => c.delete_batch(artifacts, artifact_type).await,
        }
    }
}

/// Artifact storage configuration (cloneable descriptor).
#[derive(Debug, Clone)]
pub enum ArtifactStorageConfig {
    /// Redis cluster nodes.
    Redis {
        /// Redis node URLs.
        nodes: Vec<String>,
    },
    /// S3 bucket.
    S3 {
        /// S3 bucket name.
        bucket: String,
        /// AWS region.
        region: String,
    },
}

impl ArtifactStorageConfig {
    /// Convert to the cluster SDK config type.
    pub fn to_cluster_config(&self) -> sp1_cluster_utils::ArtifactStoreConfig {
        match self {
            Self::Redis { nodes } => {
                sp1_cluster_utils::ArtifactStoreConfig::Redis { nodes: nodes.clone() }
            }
            Self::S3 { bucket, region } => sp1_cluster_utils::ArtifactStoreConfig::S3 {
                bucket: bucket.clone(),
                region: region.clone(),
            },
        }
    }
}

/// Supported backend implementations used to execute proving work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendType {
    /// OP-Succinct with SP1 proving.
    OpSuccinct,
}

impl fmt::Display for BackendType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpSuccinct => write!(f, "op_succinct"),
        }
    }
}

impl std::str::FromStr for BackendType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "op_succinct" | "op-succinct" => Ok(Self::OpSuccinct),
            _ => Err(format!("Unknown backend type: {s}")),
        }
    }
}

impl From<ProofType> for BackendType {
    fn from(proof_type: ProofType) -> Self {
        match proof_type {
            ProofType::OpSuccinctSp1ClusterCompressed
            | ProofType::OpSuccinctSp1ClusterSnarkGroth16 => Self::OpSuccinct,
        }
    }
}

/// Configuration for initializing a proving backend.
#[derive(Clone)]
pub enum BackendConfig {
    /// OP-Succinct backend settings.
    OpSuccinct {
        /// Base consensus node RPC URL.
        base_consensus_url: String,
        /// L1 execution node RPC URL.
        l1_node_url: String,
        /// L1 beacon node URL.
        l1_beacon_url: String,
        /// L2 execution node RPC URL.
        l2_node_url: String,
        /// Default sequence window for L1 head calculations.
        default_sequence_window: u64,
        /// Cluster RPC endpoint for submitting proving jobs.
        cluster_rpc: String,
        /// Pre-created SP1 cluster gRPC client (reused for status checks).
        cluster_client: sp1_cluster_common::client::ClusterServiceClient,
        /// Pre-created artifact client (reused for uploads / downloads).
        artifact_client: ArtifactClientWrapper,
        /// Artifact storage descriptor (for `ProofRequestConfig`).
        artifact_storage_config: ArtifactStorageConfig,
        /// Proof timeout threshold in hours.
        timeout_hours: u64,
        /// Range program verifying key (computed once at startup via `CpuProver`).
        range_vk: sp1_sdk::SP1VerifyingKey,
    },
    /// SP1 Network backend settings.
    Network {
        /// Base consensus node RPC URL.
        base_consensus_url: String,
        /// L1 execution node RPC URL.
        l1_node_url: String,
        /// L1 beacon node URL.
        l1_beacon_url: String,
        /// L2 execution node RPC URL.
        l2_node_url: String,
        /// Default sequence window for L1 head calculations.
        default_sequence_window: u64,
        /// Pre-built SP1 network prover (handles submission and status polling).
        network_prover: std::sync::Arc<sp1_sdk::NetworkProver>,
        /// Range program proving key (for proof submission).
        range_pk: sp1_sdk::SP1ProvingKey,
        /// Range program verifying key.
        range_vk: sp1_sdk::SP1VerifyingKey,
        /// Aggregation program proving key.
        agg_pk: sp1_sdk::SP1ProvingKey,
        /// Aggregation program verifying key.
        agg_vk: sp1_sdk::SP1VerifyingKey,
        /// Fulfillment strategy for proof requests.
        fulfillment_strategy: sp1_sdk::network::FulfillmentStrategy,
        /// Proof timeout in hours.
        timeout_hours: u64,
    },
}

impl fmt::Debug for BackendConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackendConfig").finish_non_exhaustive()
    }
}

/// Result returned when a prove request is accepted by a backend.
#[derive(Debug, Clone)]
pub struct ProveResult {
    /// Backend session identifier if asynchronous processing was started.
    pub session_id: Option<String>,
    /// Optional backend-specific metadata.
    pub metadata: Option<serde_json::Value>,
    /// Duration of witness generation in milliseconds (if available).
    pub witness_gen_duration_ms: Option<f64>,
}

/// Runtime status of a backend proof session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    /// Proof session is still running.
    Running,
    /// Proof session finished successfully.
    Completed,
    /// Proof session failed with backend-provided message.
    Failed(String),
    /// Backend does not recognize the requested session.
    NotFound,
}

/// Outcome of processing a persisted proof request.
#[derive(Debug, Clone)]
pub struct ProofProcessingResult {
    /// Proof status to persist after backend processing.
    pub status: ProofStatus,
    /// Optional error details when processing fails.
    pub error_message: Option<String>,
}

/// Backend interface implemented by all proving engines.
#[async_trait]
pub trait ProvingBackend: Send + Sync {
    /// Returns the backend type discriminator used for backend routing.
    fn backend_type(&self) -> BackendType;

    /// Starts proving for a block request.
    async fn prove(&self, request: &ProveBlockRequest) -> anyhow::Result<ProveResult>;

    /// Processes a queued proof request and returns the next status transition.
    async fn process_proof_request(
        &self,
        proof_request: &ProofRequest,
        repo: &ProofRequestRepo,
    ) -> anyhow::Result<ProofProcessingResult>;

    /// Fetches latest backend session status for a stored session reference.
    async fn get_session_status(&self, session: &ProofSession) -> anyhow::Result<SessionStatus>;

    /// Returns a stable backend name for logs and metrics.
    fn name(&self) -> &'static str;
}

/// Registry that stores all enabled proving backend instances.
pub struct BackendRegistry {
    backends: Vec<std::sync::Arc<dyn ProvingBackend>>,
}

impl fmt::Debug for BackendRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackendRegistry").field("backends_count", &self.backends.len()).finish()
    }
}

impl BackendRegistry {
    /// Creates an empty backend registry.
    pub fn new() -> Self {
        Self { backends: Vec::new() }
    }

    /// Registers a backend instance so it can receive prove requests.
    pub fn register(&mut self, backend: std::sync::Arc<dyn ProvingBackend>) {
        self.backends.push(backend);
    }

    /// Returns the first backend matching `backend_type`.
    pub fn get(&self, backend_type: BackendType) -> Option<std::sync::Arc<dyn ProvingBackend>> {
        self.backends.iter().find(|b| b.backend_type() == backend_type).cloned()
    }

    /// Returns a cloned list of all registered backends.
    pub fn all(&self) -> Vec<std::sync::Arc<dyn ProvingBackend>> {
        self.backends.clone()
    }
}

impl Default for BackendRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn test_backend_type_display() {
        assert_eq!(BackendType::OpSuccinct.to_string(), "op_succinct");
    }

    #[test]
    fn test_backend_type_from_str() {
        assert_eq!(BackendType::from_str("op_succinct").unwrap(), BackendType::OpSuccinct);
        assert_eq!(BackendType::from_str("op-succinct").unwrap(), BackendType::OpSuccinct);
        assert!(BackendType::from_str("unknown").is_err());
    }

    #[test]
    fn test_backend_registry_empty() {
        let registry = BackendRegistry::new();
        assert_eq!(registry.all().len(), 0);
        assert!(registry.get(BackendType::OpSuccinct).is_none());
    }

    struct StubBackend;

    #[async_trait]
    impl ProvingBackend for StubBackend {
        fn backend_type(&self) -> BackendType {
            BackendType::OpSuccinct
        }
        async fn prove(&self, _request: &ProveBlockRequest) -> anyhow::Result<ProveResult> {
            unimplemented!()
        }
        async fn process_proof_request(
            &self,
            _proof_request: &ProofRequest,
            _repo: &ProofRequestRepo,
        ) -> anyhow::Result<ProofProcessingResult> {
            unimplemented!()
        }
        async fn get_session_status(
            &self,
            _session: &ProofSession,
        ) -> anyhow::Result<SessionStatus> {
            unimplemented!()
        }
        fn name(&self) -> &'static str {
            "stub"
        }
    }

    #[test]
    fn test_backend_registry_register_and_get() {
        let mut registry = BackendRegistry::new();
        registry.register(std::sync::Arc::new(StubBackend));

        assert_eq!(registry.all().len(), 1);

        let backend = registry.get(BackendType::OpSuccinct);
        assert!(backend.is_some());
        assert_eq!(backend.unwrap().name(), "stub");
    }

    #[test]
    fn test_backend_type_from_proof_type() {
        assert_eq!(
            BackendType::from(ProofType::OpSuccinctSp1ClusterCompressed),
            BackendType::OpSuccinct
        );
        assert_eq!(
            BackendType::from(ProofType::OpSuccinctSp1ClusterSnarkGroth16),
            BackendType::OpSuccinct
        );
    }
}
