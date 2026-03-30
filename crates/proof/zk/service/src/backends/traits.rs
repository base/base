use std::fmt;

use async_trait::async_trait;
use base_zk_client::ProveBlockRequest;
use base_zk_db::{ProofRequest, ProofRequestRepo, ProofSession, ProofStatus, ProofType};
use serde::{Deserialize, Serialize};

/// Artifact storage destinations used by proving backends.
#[derive(Debug, Clone)]
pub enum ArtifactStorageConfig {
    /// Store artifacts in a Redis cluster (`nodes=<redis_urls>`).
    Redis {
        /// Redis node URLs used by the cluster (`nodes=<redis_urls>`).
        nodes: Vec<String>,
    },
    /// Store artifacts in S3 (`bucket=<name>`, `region=<aws_region>`).
    S3 {
        /// S3 bucket name.
        bucket: String,
        /// AWS region where the bucket is hosted.
        region: String,
    },
    /// Store artifacts in Google Cloud Storage (`bucket=<name>`, `concurrency=<workers>`).
    Gcs {
        /// GCS bucket name.
        bucket: String,
        /// Concurrent upload worker count.
        concurrency: usize,
    },
}

/// Supported backend implementations used to execute proving work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendType {
    /// Generic zkVM backend integrated with the cluster prover.
    GenericZkvm,
}

impl fmt::Display for BackendType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GenericZkvm => write!(f, "generic_zkvm"),
        }
    }
}

impl std::str::FromStr for BackendType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "generic_zkvm" => Ok(Self::GenericZkvm),
            _ => Err(format!("Unknown backend type: {s}")),
        }
    }
}

impl From<ProofType> for BackendType {
    fn from(proof_type: ProofType) -> Self {
        match proof_type {
            ProofType::GenericZkvmClusterCompressed | ProofType::GenericZkvmClusterSnarkGroth16 => {
                Self::GenericZkvm
            }
        }
    }
}

/// Configuration for initializing a proving backend.
#[derive(Debug, Clone)]
pub enum BackendConfig {
    /// Generic zkVM backend settings (`base_consensus_url=<url>`, `cluster_rpc=<url>`).
    GenericZkvm {
        /// Base consensus node RPC URL used for rollup-specific chain state.
        base_consensus_url: String,
        /// L1 execution node RPC URL used for L1 state queries.
        l1_node_url: String,
        /// L1 beacon node URL used for beacon chain data.
        l1_beacon_url: String,
        /// L2 execution node RPC URL used for L2 state queries.
        l2_node_url: String,
        /// Default sequence window used for L1 head calculations.
        default_sequence_window: u64,
        /// Cluster RPC endpoint used to submit proving jobs.
        cluster_rpc: String,
        /// Artifact storage target for generated proof outputs.
        artifact_storage: ArtifactStorageConfig,
        /// Proof timeout threshold in hours.
        timeout_hours: u64,
    },
}

/// Result returned when a prove request is accepted by a backend.
#[derive(Debug, Clone)]
pub struct ProveResult {
    /// Backend session identifier (`session_id=<id>`) if asynchronous processing was started.
    pub session_id: Option<String>,
    /// Optional backend-specific metadata (`metadata=<json>`).
    pub metadata: Option<serde_json::Value>,
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
    /// Optional error details (`error=<message>`) when processing fails.
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

    /// Returns a stable backend name for logs and metrics (`backend=<name>`).
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
        assert_eq!(BackendType::GenericZkvm.to_string(), "generic_zkvm");
    }

    #[test]
    fn test_backend_type_from_str() {
        assert_eq!(BackendType::from_str("generic_zkvm").unwrap(), BackendType::GenericZkvm);
        assert!(BackendType::from_str("unknown").is_err());
    }

    #[test]
    fn test_backend_registry() {
        let registry = BackendRegistry::new();
        assert_eq!(registry.all().len(), 0);
    }

    #[test]
    fn test_backend_type_from_proof_type() {
        use base_zk_db::ProofType;

        assert_eq!(
            BackendType::from(ProofType::GenericZkvmClusterCompressed),
            BackendType::GenericZkvm
        );
        assert_eq!(
            BackendType::from(ProofType::GenericZkvmClusterSnarkGroth16),
            BackendType::GenericZkvm
        );
    }
}
