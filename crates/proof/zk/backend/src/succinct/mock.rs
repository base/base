//! Mock [`ZkProver`] that produces instant placeholder proofs.

use async_trait::async_trait;
use base_proof_zk_host::{ZkProofRequestKind, ZkProver, ZkProverError, ZkSessionState};
use base_prover_service_protocol::{
    ProofResult, SessionType, SnarkGroth16ProofRequest, SnarkGroth16ProofResult, ZkProofResult,
    ZkVm,
};

/// Placeholder proof bytes returned by the mock prover.
pub const MOCK_PROOF_BYTES: &[u8] = b"mock-zk-proof";

/// Prefix marking a mock backend session id for a SNARK proof.
pub const MOCK_SNARK_PREFIX: &str = "mock-snark-";

/// [`ZkProver`] producing instant placeholder proofs without a real backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct MockZkProver;

#[async_trait]
impl ZkProver for MockZkProver {
    async fn submit(
        &self,
        _request: &ZkProofRequestKind,
        request_session_id: &str,
    ) -> Result<String, ZkProverError> {
        Ok(format!("mock-stark-{request_session_id}"))
    }

    async fn submit_next(
        &self,
        _request: &SnarkGroth16ProofRequest,
        request_session_id: &str,
        _completed_backend_session_id: &str,
    ) -> Result<String, ZkProverError> {
        Ok(format!("{MOCK_SNARK_PREFIX}{request_session_id}"))
    }

    async fn poll(&self, _backend_session_id: &str) -> Result<ZkSessionState, ZkProverError> {
        Ok(ZkSessionState::Completed)
    }

    async fn download(
        &self,
        session_type: SessionType,
        _backend_session_id: &str,
    ) -> Result<ProofResult, ZkProverError> {
        let zk_proof = ZkProofResult {
            zk_vm: ZkVm::Sp1,
            proof: MOCK_PROOF_BYTES.to_vec().into(),
            execution_stats: None,
        };

        match session_type {
            SessionType::Snark => {
                Ok(ProofResult::SnarkGroth16(SnarkGroth16ProofResult { proof: zk_proof }))
            }
            SessionType::Stark => Ok(ProofResult::Compressed(zk_proof)),
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
        assert!(matches!(
            prover.download(SessionType::Stark, &id).await.unwrap(),
            ProofResult::Compressed(_)
        ));
    }

    #[tokio::test]
    async fn submit_next_advances_groth16_to_snark_session() {
        let prover = MockZkProver;
        let request = snark_groth16();
        let range_id = prover.submit(&request, "session-1").await.unwrap();
        assert_eq!(range_id, "mock-stark-session-1");

        let ZkProofRequestKind::SnarkGroth16(proof_request) = &request else {
            unreachable!("request is Groth16");
        };
        let snark_id = prover.submit_next(proof_request, "session-1", &range_id).await.unwrap();

        assert_eq!(snark_id, "mock-snark-session-1");
        assert!(matches!(
            prover.download(SessionType::Snark, &snark_id).await.unwrap(),
            ProofResult::SnarkGroth16(_)
        ));
    }
}
