//! ZK proving abstraction used by the host worker.

use async_trait::async_trait;
use base_prover_service_protocol::{
    ProofResult, SessionType, SnarkGroth16ProofRequest, ZkProofRequest,
};
use thiserror::Error;

/// Concrete ZK proof request claimed from the prover service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZkProofRequestKind {
    /// Request for a compressed ZK proof.
    Compressed(ZkProofRequest),
    /// Request for a Groth16 SNARK proof.
    SnarkGroth16(SnarkGroth16ProofRequest),
}

impl ZkProofRequestKind {
    /// Returns the first L2 block number covered by this request.
    pub const fn start_block_number(&self) -> u64 {
        match self {
            Self::Compressed(request) => request.start_block_number,
            Self::SnarkGroth16(request) => request.proof.start_block_number,
        }
    }

    /// Returns the number of consecutive L2 blocks to prove.
    pub const fn number_of_blocks_to_prove(&self) -> u64 {
        match self {
            Self::Compressed(request) => request.number_of_blocks_to_prove,
            Self::SnarkGroth16(request) => request.proof.number_of_blocks_to_prove,
        }
    }

    /// Returns whether this request asks for a Groth16 SNARK proof.
    pub const fn is_snark_groth16(&self) -> bool {
        matches!(self, Self::SnarkGroth16(_))
    }

    /// Returns the backend session type tracked for this request.
    pub const fn session_type(&self) -> SessionType {
        match self {
            Self::Compressed(_) => SessionType::Stark,
            Self::SnarkGroth16(_) => SessionType::Snark,
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
    /// Submit the proving job to the backend and return its backend session id.
    async fn submit(
        &self,
        request: &ZkProofRequestKind,
        request_session_id: &str,
    ) -> Result<String, ZkProverError>;

    /// Poll the backend session, returning its current state.
    async fn poll(&self, backend_session_id: &str) -> Result<ZkSessionState, ZkProverError>;

    /// Download the completed proof for a backend session.
    async fn download(&self, backend_session_id: &str) -> Result<ProofResult, ZkProverError>;
}

/// Placeholder [`ZkProver`] that always reports proving as unimplemented.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnimplementedZkProver;

#[async_trait]
impl ZkProver for UnimplementedZkProver {
    async fn submit(
        &self,
        _request: &ZkProofRequestKind,
        _request_session_id: &str,
    ) -> Result<String, ZkProverError> {
        Err(ZkProverError::Unimplemented)
    }

    async fn poll(&self, _backend_session_id: &str) -> Result<ZkSessionState, ZkProverError> {
        Err(ZkProverError::Unimplemented)
    }

    async fn download(&self, _backend_session_id: &str) -> Result<ProofResult, ZkProverError> {
        Err(ZkProverError::Unimplemented)
    }
}

#[cfg(test)]
mod tests {
    use base_prover_service_protocol::{SessionType, ZkVm};

    use super::*;

    fn zk_request() -> ZkProofRequest {
        ZkProofRequest {
            start_block_number: 100,
            number_of_blocks_to_prove: 5,
            sequence_window: None,
            l1_head: None,
            intermediate_root_interval: None,
            zk_vm: ZkVm::Sp1,
        }
    }

    #[test]
    fn request_kind_exposes_block_range() {
        let compressed = ZkProofRequestKind::Compressed(zk_request());
        assert_eq!(compressed.start_block_number(), 100);
        assert_eq!(compressed.number_of_blocks_to_prove(), 5);
        assert!(!compressed.is_snark_groth16());

        let snark = ZkProofRequestKind::SnarkGroth16(SnarkGroth16ProofRequest {
            proof: zk_request(),
            prover_address: alloy_primitives::Address::ZERO,
        });
        assert!(snark.is_snark_groth16());
    }

    #[test]
    fn request_kind_maps_to_session_type() {
        assert_eq!(ZkProofRequestKind::Compressed(zk_request()).session_type(), SessionType::Stark);
        assert_eq!(
            ZkProofRequestKind::SnarkGroth16(SnarkGroth16ProofRequest {
                proof: zk_request(),
                prover_address: alloy_primitives::Address::ZERO,
            })
            .session_type(),
            SessionType::Snark
        );
    }

    #[tokio::test]
    async fn unimplemented_prover_reports_unimplemented() {
        let prover = UnimplementedZkProver;
        let error = prover
            .submit(&ZkProofRequestKind::Compressed(zk_request()), "session-1")
            .await
            .expect_err("stub prover should not produce a proof");

        assert!(matches!(error, ZkProverError::Unimplemented));
    }
}
