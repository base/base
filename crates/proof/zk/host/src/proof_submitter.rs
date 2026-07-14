//! ZK-specific worker submission request builder.

use base_proof_worker::ProofSubmitterError;
use base_prover_service_protocol::{ProofResult, WorkerSubmitProofRequest};

/// Helper for building prover-service worker proof submission requests.
#[derive(Debug)]
pub struct ProofSubmitterRequest {
    /// Proof session identifier.
    pub session_id: String,
    /// Worker claim lock identifier.
    pub lock_id: String,
    /// Worker identifier holding the claim.
    pub worker_id: String,
    /// Generated proof result to submit.
    pub result: ProofResult,
}

impl TryFrom<ProofSubmitterRequest> for WorkerSubmitProofRequest {
    type Error = ProofSubmitterError;

    fn try_from(request: ProofSubmitterRequest) -> Result<Self, Self::Error> {
        match request.result {
            ProofResult::Compressed(_) | ProofResult::SnarkPlonk(_) => Ok(Self {
                session_id: request.session_id,
                lock_id: request.lock_id,
                worker_id: request.worker_id,
                result: request.result,
            }),
            ProofResult::Tee(_) => Err(ProofSubmitterError::UnsupportedProofResult),
        }
    }
}

#[cfg(test)]
mod tests {
    use base_prover_service_protocol::{ProofResult, TeeKind, TeeProofResult, ZkProofResult, ZkVm};

    use super::*;

    fn zk_result() -> ProofResult {
        ProofResult::Compressed(ZkProofResult {
            zk_vm: ZkVm::Sp1,
            proof: vec![1, 2, 3].into(),
            execution_stats: None,
        })
    }

    #[test]
    fn zk_result_builds_submission_request() {
        let request = WorkerSubmitProofRequest::try_from(ProofSubmitterRequest {
            session_id: "session-1".to_owned(),
            lock_id: "lock-1".to_owned(),
            worker_id: "worker-1".to_owned(),
            result: zk_result(),
        })
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

        let result = WorkerSubmitProofRequest::try_from(ProofSubmitterRequest {
            session_id: "session-1".to_owned(),
            lock_id: "lock-1".to_owned(),
            worker_id: "worker-1".to_owned(),
            result: tee_result,
        });

        assert!(matches!(result, Err(ProofSubmitterError::UnsupportedProofResult)));
    }
}
