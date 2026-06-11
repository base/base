//! Dry-run [`ZkProver`] that completes instantly with an empty proof.

use async_trait::async_trait;
use base_proof_zk_host::{ZkProofRequestKind, ZkProver, ZkProverError, ZkSessionState};
use base_prover_service_protocol::{ProofResult, SnarkGroth16ProofResult, ZkProofResult, ZkVm};

/// Prefix marking a dry-run backend session id for a SNARK proof.
pub const DRY_RUN_SNARK_PREFIX: &str = "dry-run-snark-";

/// [`ZkProver`] that completes instantly with an empty proof payload.
#[derive(Debug, Clone, Copy, Default)]
pub struct DryRunZkProver;

impl DryRunZkProver {
    /// Derive the deterministic backend session id for a request.
    pub fn backend_session_id(request: &ZkProofRequestKind, request_session_id: &str) -> String {
        let session_type = if request.is_snark_groth16() { "snark" } else { "stark" };
        format!("dry-run-{session_type}-{request_session_id}")
    }
}

#[async_trait]
impl ZkProver for DryRunZkProver {
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
        let zk_proof = ZkProofResult { zk_vm: ZkVm::Sp1, proof: Vec::new().into() };

        if backend_session_id.starts_with(DRY_RUN_SNARK_PREFIX) {
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
    async fn dry_run_completes_with_empty_proof() {
        let prover = DryRunZkProver;
        let id = prover.submit(&compressed(), "session-1").await.unwrap();

        assert_eq!(id, "dry-run-stark-session-1");
        assert_eq!(prover.poll(&id).await.unwrap(), ZkSessionState::Completed);

        match prover.download(&id).await.unwrap() {
            ProofResult::Compressed(proof) => assert!(proof.proof.is_empty()),
            other => panic!("expected compressed proof, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dry_run_completes_with_empty_snark_groth16_proof() {
        let prover = DryRunZkProver;
        let id = prover.submit(&snark_groth16(), "session-1").await.unwrap();

        assert_eq!(id, "dry-run-snark-session-1");
        assert_eq!(prover.poll(&id).await.unwrap(), ZkSessionState::Completed);

        match prover.download(&id).await.unwrap() {
            ProofResult::SnarkGroth16(proof) => assert!(proof.proof.proof.is_empty()),
            other => panic!("expected SNARK Groth16 proof, got {other:?}"),
        }
    }
}
