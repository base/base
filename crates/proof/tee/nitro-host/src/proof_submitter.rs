//! TEE-specific worker submission request builder.

use base_proof_primitives::ProofResult as NitroProofResult;
use base_prover_service_protocol::{
    ProofResult as ServiceProofResult, TeeKind, TeeProofResult, WorkerSubmitProofRequest,
};
use thiserror::Error;

/// Errors raised while building a Nitro TEE worker submission request.
#[derive(Debug, Error)]
pub enum ProofSubmitterRequestError {
    /// Nitro proof submitter only submits TEE proof results.
    #[error("nitro proof submitter only accepts TEE proof results")]
    UnsupportedProofResult,
}

/// Helper for building prover-service worker proof submission requests.
#[derive(Debug)]
pub struct ProofSubmitterRequest;

impl ProofSubmitterRequest {
    /// Builds a worker proof submission request from a generated Nitro TEE proof.
    pub fn from_tee_proof(
        session_id: String,
        lock_id: String,
        worker_id: String,
        proof: NitroProofResult,
    ) -> Result<WorkerSubmitProofRequest, ProofSubmitterRequestError> {
        let NitroProofResult::Tee { aggregate_proposal, proposals, tee_signer } = proof else {
            return Err(ProofSubmitterRequestError::UnsupportedProofResult);
        };

        Ok(WorkerSubmitProofRequest {
            session_id,
            lock_id,
            worker_id,
            result: ServiceProofResult::Tee(TeeProofResult {
                aggregate_proposal,
                proposals,
                tee_kind: TeeKind::AwsNitro,
                tee_signer,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes};
    use base_proof_primitives::{ProofResult as NitroProofResult, Proposal};
    use base_prover_service_protocol::{ProofResult as ServiceProofResult, TeeKind};

    use super::*;

    fn proposal(block: u64) -> Proposal {
        Proposal {
            output_root: B256::repeat_byte(1),
            signature: Bytes::from(vec![0xab; 65]),
            l1_origin_hash: B256::repeat_byte(2),
            l1_origin_number: block.saturating_sub(1),
            l2_block_number: block,
            prev_output_root: B256::repeat_byte(3),
            config_hash: B256::repeat_byte(4),
            schedule_id: B256::repeat_byte(5),
        }
    }

    #[test]
    fn tee_proof_request_forwards_enclave_signer() {
        let tee_signer = Address::repeat_byte(0x11);
        let request = ProofSubmitterRequest::from_tee_proof(
            "session-1".to_string(),
            "lock-1".to_string(),
            "worker-1".to_string(),
            NitroProofResult::Tee {
                aggregate_proposal: proposal(10),
                proposals: vec![proposal(8), proposal(9), proposal(10)],
                tee_signer,
            },
        )
        .expect("tee proof should build a submission request");

        assert_eq!(request.session_id, "session-1");
        assert_eq!(request.lock_id, "lock-1");
        assert_eq!(request.worker_id, "worker-1");
        let ServiceProofResult::Tee(result) = request.result else {
            panic!("expected tee proof result");
        };
        assert_eq!(result.tee_signer, tee_signer);
        assert_eq!(result.tee_kind, TeeKind::AwsNitro);
        assert_eq!(result.aggregate_proposal.l2_block_number, 10);
        assert_eq!(result.proposals.len(), 3);
    }

    #[test]
    fn tee_proof_request_rejects_non_tee_result() {
        let result = ProofSubmitterRequest::from_tee_proof(
            "session-1".to_string(),
            "lock-1".to_string(),
            "worker-1".to_string(),
            NitroProofResult::Zk { proof_bytes: vec![1, 2, 3] },
        );

        assert!(matches!(result, Err(ProofSubmitterRequestError::UnsupportedProofResult)));
    }
}
