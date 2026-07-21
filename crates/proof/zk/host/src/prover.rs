//! ZK proving abstraction used by the host worker.

use async_trait::async_trait;
use base_prover_service_protocol::{
    ProofResult, SessionType, SnarkPlonkProofRequest, ZkBackend, ZkProofRequest,
};
use thiserror::Error;

/// Concrete ZK proof request claimed from the prover service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZkProofRequestKind {
    /// Request for a compressed ZK proof.
    Compressed(ZkProofRequest),
    /// Request for a PLONK SNARK proof.
    SnarkPlonk(SnarkPlonkProofRequest),
}

impl ZkProofRequestKind {
    /// Returns the first L2 block number covered by this request.
    pub const fn start_block_number(&self) -> u64 {
        match self {
            Self::Compressed(request) => request.start_block_number,
            Self::SnarkPlonk(request) => request.proof.start_block_number,
        }
    }

    /// Returns the number of consecutive L2 blocks to prove.
    pub const fn number_of_blocks_to_prove(&self) -> u64 {
        match self {
            Self::Compressed(request) => request.number_of_blocks_to_prove,
            Self::SnarkPlonk(request) => request.proof.number_of_blocks_to_prove,
        }
    }

    /// Returns the proving backend selected for this request.
    pub const fn zk_backend(&self) -> ZkBackend {
        match self {
            Self::Compressed(request) => request.zk_backend,
            Self::SnarkPlonk(request) => request.proof.zk_backend,
        }
    }
}

/// Current state of a backend proving session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZkSessionState {
    /// The backend session is still running.
    Running,
    /// The backend session completed successfully and the proof can be downloaded.
    Completed,
    /// The backend session failed with the given reason.
    Failed(String),
    /// The backend has no record of the session id.
    NotFound,
}

/// Errors raised while generating a ZK proof.
#[derive(Debug, Error)]
pub enum ZkProverError {
    /// ZK proving is not yet implemented for this prover.
    #[error("zk proving is not yet implemented")]
    Unimplemented,
    /// The backend session failed before proof download.
    #[error("backend session {backend_session_id} failed: {reason}")]
    BackendSessionFailed {
        /// Backend proving session identifier.
        backend_session_id: String,
        /// Backend-provided failure reason.
        reason: String,
    },
    /// The backend has no record of the expected session.
    #[error("backend session {backend_session_id} not found")]
    BackendSessionNotFound {
        /// Backend proving session identifier.
        backend_session_id: String,
    },
    /// The proving backend selected by the request is not configured on this host.
    #[error("zk backend {backend} is not configured on this host")]
    UnsupportedBackend {
        /// Backend requested by the proof job.
        backend: ZkBackend,
    },
    /// The proving backend failed to produce a proof.
    #[error("zk proving backend failed")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Recording or reading backend session state via the prover service failed.
    #[error("zk session tracking failed")]
    Session(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Drives a single ZK proving job on a backend.
#[async_trait]
pub trait ZkProver: Send + Sync + std::fmt::Debug {
    /// Submit the range (STARK) proof to the backend and return its backend session id.
    ///
    /// Every job's first stage is a compressed range proof; SNARK aggregation (when required) is
    /// driven separately via [`ZkProver::submit_next`].
    async fn submit(
        &self,
        request: &ZkProofRequest,
        request_session_id: &str,
    ) -> Result<String, ZkProverError>;

    /// Submit the next proof stage for a completed prior stage, returning its backend session id.
    ///
    /// Implementations should be idempotent for a given `(request_session_id,
    /// completed_backend_session_id)` pair where the backend supports it; backends whose provider
    /// assigns the session id (e.g. the SP1 Network) cannot, and document the deviation.
    async fn submit_next(
        &self,
        _request: &SnarkPlonkProofRequest,
        _request_session_id: &str,
        _completed_backend_session_id: &str,
    ) -> Result<String, ZkProverError> {
        Err(ZkProverError::Unimplemented)
    }

    /// Poll the backend session, returning its current state.
    async fn poll(&self, backend_session_id: &str) -> Result<ZkSessionState, ZkProverError>;

    /// Download the completed proof for a backend session.
    async fn download(
        &self,
        session_type: SessionType,
        backend_session_id: &str,
    ) -> Result<ProofResult, ZkProverError>;
}

/// Placeholder [`ZkProver`] that always reports proving as unimplemented.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnimplementedZkProver;

#[async_trait]
impl ZkProver for UnimplementedZkProver {
    async fn submit(
        &self,
        _request: &ZkProofRequest,
        _request_session_id: &str,
    ) -> Result<String, ZkProverError> {
        Err(ZkProverError::Unimplemented)
    }

    async fn poll(&self, _backend_session_id: &str) -> Result<ZkSessionState, ZkProverError> {
        Err(ZkProverError::Unimplemented)
    }

    async fn download(
        &self,
        _session_type: SessionType,
        _backend_session_id: &str,
    ) -> Result<ProofResult, ZkProverError> {
        Err(ZkProverError::Unimplemented)
    }
}

#[cfg(test)]
mod tests {
    use base_prover_service_protocol::{ZkBackend, ZkVm};

    use super::*;

    fn zk_request() -> ZkProofRequest {
        ZkProofRequest {
            start_block_number: 100,
            number_of_blocks_to_prove: 5,
            sequence_window: None,
            l1_head: None,
            intermediate_root_interval: None,
            zk_vm: ZkVm::Sp1,
            zk_backend: ZkBackend::Cluster,
        }
    }

    #[test]
    fn request_kind_exposes_block_range() {
        let compressed = ZkProofRequestKind::Compressed(zk_request());
        assert_eq!(compressed.start_block_number(), 100);
        assert_eq!(compressed.number_of_blocks_to_prove(), 5);

        let snark = ZkProofRequestKind::SnarkPlonk(SnarkPlonkProofRequest {
            proof: zk_request(),
            prover_address: alloy_primitives::Address::ZERO,
        });
        assert_eq!(snark.start_block_number(), 100);
        assert_eq!(snark.number_of_blocks_to_prove(), 5);
    }

    #[tokio::test]
    async fn unimplemented_prover_reports_unimplemented() {
        let prover = UnimplementedZkProver;
        let error = prover
            .submit(&zk_request(), "session-1")
            .await
            .expect_err("stub prover should not produce a proof");

        assert!(matches!(error, ZkProverError::Unimplemented));
    }
}
