//! ZK-specific worker submission request builder.

use base_proof_worker::ProofSubmitterError;
use base_prover_service_protocol::{ProofResult, WorkerSubmitProofRequest};

/// Helper for building prover-service worker proof submission requests.
#[derive(Debug)]
pub struct ProofSubmitterRequest;

impl ProofSubmitterRequest {
    /// Builds a worker proof submission request from a generated ZK proof result.
    pub fn from_zk_result(
        session_id: String,
        lock_id: String,
        worker_id: String,
        result: ProofResult,
    ) -> Result<WorkerSubmitProofRequest, ProofSubmitterError> {
        match result {
            ProofResult::Compressed(_) | ProofResult::SnarkGroth16(_) => {
                Ok(WorkerSubmitProofRequest { session_id, lock_id, worker_id, result })
            }
            ProofResult::Tee(_) => Err(ProofSubmitterError::UnsupportedProofResult),
        }
    }
}

#[cfg(test)]
mod tests {
    use base_prover_service_protocol::{ProofResult, TeeKind, TeeProofResult, ZkProofResult, ZkVm};

    use super::*;

    fn zk_result() -> ProofResult {
        ProofResult::Compressed(ZkProofResult { zk_vm: ZkVm::Sp1, proof: vec![1, 2, 3].into() })
    }

    #[test]
    fn zk_result_builds_submission_request() {
        let request = ProofSubmitterRequest::from_zk_result(
            "session-1".to_owned(),
            "lock-1".to_owned(),
            "worker-1".to_owned(),
            zk_result(),
        )
        .expect("zk result should build a submission request");

        assert_eq!(request.session_id, "session-1");
        assert_eq!(request.lock_id, "lock-1");
        assert_eq!(request.worker_id, "worker-1");
        assert!(matches!(request.result, ProofResult::Compressed(_)));
    }

    fn proposal() -> base_proof_primitives::Proposal {
        base_proof_primitives::Proposal {
            output_root: alloy_primitives::B256::repeat_byte(1),
            signature: alloy_primitives::Bytes::from(vec![0xab; 65]),
            l1_origin_hash: alloy_primitives::B256::repeat_byte(2),
            l1_origin_number: 10,
            l2_block_number: 11,
            prev_output_root: alloy_primitives::B256::repeat_byte(3),
            config_hash: alloy_primitives::B256::repeat_byte(4),
        }
    }

    #[test]
    fn tee_result_is_rejected() {
        let tee_result = ProofResult::Tee(TeeProofResult {
            aggregate_proposal: proposal(),
            proposals: vec![proposal()],
            tee_kind: TeeKind::AwsNitro,
        });

        let result = ProofSubmitterRequest::from_zk_result(
            "session-1".to_owned(),
            "lock-1".to_owned(),
            "worker-1".to_owned(),
            tee_result,
        );

        assert!(matches!(result, Err(ProofSubmitterError::UnsupportedProofResult)));
    }
}
