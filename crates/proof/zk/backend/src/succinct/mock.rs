//! Mock [`ZkProver`] that produces instant placeholder proofs.

use async_trait::async_trait;
use base_proof_zk_host::{ZkProofRequestKind, ZkProver, ZkProverError, ZkSessionState};
use base_prover_service_protocol::{ProofResult, SnarkGroth16ProofResult, ZkProofResult, ZkVm};

/// Placeholder proof bytes returned by the mock prover.
pub const MOCK_PROOF_BYTES: &[u8] = b"mock-zk-proof";

/// Prefix marking a mock backend session id for a SNARK proof.
pub const MOCK_SNARK_PREFIX: &str = "mock-snark-";

/// [`ZkProver`] producing instant placeholder proofs without a real backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct MockZkProver;

impl MockZkProver {
    /// Derive the deterministic backend session id for a request.
    pub fn backend_session_id(request: &ZkProofRequestKind, request_session_id: &str) -> String {
        let session_type = if request.is_snark_groth16() { "snark" } else { "stark" };
        format!("mock-{session_type}-{request_session_id}")
    }
}

#[async_trait]
impl ZkProver for MockZkProver {
    async fn submit(
        &self,
        request: &ZkProofRequestKind,
        request_session_id: &str,
    ) -> Result<String, ZkProverError> {
        Ok(Self::backend_session_id(request, request_session_id))
    }

    async fn poll(&self, _backend_session_id: &str) -> Result<ZkSessionState, ZkProverError> {
        Ok(ZkSessionState::Completed)
    }

    async fn download(&self, backend_session_id: &str) -> Result<ProofResult, ZkProverError> {
        let zk_proof = ZkProofResult { zk_vm: ZkVm::Sp1, proof: MOCK_PROOF_BYTES.to_vec().into() };

        if backend_session_id.starts_with(MOCK_SNARK_PREFIX) {
            Ok(ProofResult::SnarkGroth16(SnarkGroth16ProofResult { proof: zk_proof }))
        } else {
            Ok(ProofResult::Compressed(zk_proof))
        }
    }
}

#[cfg(test)]
mod tests {
    use base_prover_service_protocol::{SnarkGroth16ProofRequest, ZkProofRequest, ZkVm};

    use super::*;

    fn zk_request() -> ZkProofRequest {
        ZkProofRequest {
            start_block_number: 1,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            l1_head: None,
            intermediate_root_interval: None,
            zk_vm: ZkVm::Sp1,
        }
    }

    fn compressed() -> ZkProofRequestKind {
        ZkProofRequestKind::Compressed(zk_request())
    }

    fn snark_groth16() -> ZkProofRequestKind {
        ZkProofRequestKind::SnarkGroth16(SnarkGroth16ProofRequest {
            proof: zk_request(),
            prover_address: Default::default(),
        })
    }

    #[tokio::test]
    async fn submit_is_deterministic() {
        let prover = MockZkProver;
        let first = prover.submit(&compressed(), "session-1").await.unwrap();
        let second = prover.submit(&compressed(), "session-1").await.unwrap();

        assert_eq!(first, second);
        assert_eq!(first, "mock-stark-session-1");
    }

    #[tokio::test]
    async fn poll_completes_immediately_and_downloads_compressed() {
        let prover = MockZkProver;
        let id = prover.submit(&compressed(), "session-1").await.unwrap();

        assert_eq!(prover.poll(&id).await.unwrap(), ZkSessionState::Completed);
        assert!(matches!(prover.download(&id).await.unwrap(), ProofResult::Compressed(_)));
    }

    #[tokio::test]
    async fn poll_completes_immediately_and_downloads_snark_groth16() {
        let prover = MockZkProver;
        let id = prover.submit(&snark_groth16(), "session-1").await.unwrap();

        assert_eq!(id, "mock-snark-session-1");
        assert_eq!(prover.poll(&id).await.unwrap(), ZkSessionState::Completed);
        assert!(matches!(prover.download(&id).await.unwrap(), ProofResult::SnarkGroth16(_)));
    }
}
