//! TEE-specific worker submission request builder.

use alloy_primitives::{Address, B256};
use base_proof_primitives::ProofResult as NitroProofResult;
use base_prover_service_protocol::{
    ProofResult as ServiceProofResult, TeeKind, TeeProofResult, WorkerSubmitProofRequest,
};
use thiserror::Error;

use crate::{NitroHostError, TeeSignerRecovery};

/// Errors raised while building a Nitro TEE worker submission request.
#[derive(Debug, Error)]
pub enum ProofSubmitterRequestError {
    /// Nitro proof submitter only submits TEE proof results.
    #[error("nitro proof submitter only accepts TEE proof results")]
    UnsupportedProofResult,
    /// A TEE proof result carried no proposals to recover the signer from.
    #[error("tee proof result contained no proposals")]
    MissingProposals,
    /// The enclave signer could not be recovered from the proof signature.
    #[error(transparent)]
    SignerRecovery(#[from] NitroHostError),
}

/// Helper for building prover-service worker proof submission requests.
#[derive(Debug)]
pub struct ProofSubmitterRequest;

impl ProofSubmitterRequest {
    /// Builds a worker proof submission request from a generated Nitro TEE proof.
    ///
    /// The enclave signer is recovered host-side from the first per-block
    /// proposal's signature, using `proposer` and `tee_image_hash` from the
    /// originating proof request to reconstruct the signed journal. This is the
    /// exact key that signed the proof, so it cannot drift from a concurrent
    /// enclave restart the way a separate signer query could.
    ///
    /// Relies on the invariant that a TEE proof always carries at least one
    /// per-block proposal (the enclave rejects empty proposal sets); an empty
    /// set yields [`ProofSubmitterRequestError::MissingProposals`] rather than
    /// falling back to the aggregate.
    pub fn from_tee_proof(
        session_id: String,
        lock_id: String,
        worker_id: String,
        proof: NitroProofResult,
        proposer: Address,
        tee_image_hash: B256,
    ) -> Result<WorkerSubmitProofRequest, ProofSubmitterRequestError> {
        let NitroProofResult::Tee { aggregate_proposal, proposals } = proof else {
            return Err(ProofSubmitterRequestError::UnsupportedProofResult);
        };

        let signer_proposal =
            proposals.first().ok_or(ProofSubmitterRequestError::MissingProposals)?;
        let tee_signer =
            TeeSignerRecovery::recover_from_proposal(signer_proposal, proposer, tee_image_hash)?;

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
    use alloy_primitives::{Address, B256, Bytes, keccak256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_proof_primitives::{ProofJournal, ProofResult as NitroProofResult, Proposal};
    use base_prover_service_protocol::{ProofResult as ServiceProofResult, TeeKind};

    use super::*;

    const PROPOSER: Address = Address::repeat_byte(0x22);
    const TEE_IMAGE_HASH: B256 = B256::repeat_byte(0x33);

    fn proposal(signer: &PrivateKeySigner, block: u64) -> Proposal {
        let mut proposal = Proposal {
            output_root: B256::repeat_byte(1),
            signature: Bytes::new(),
            l1_origin_hash: B256::repeat_byte(2),
            l1_origin_number: block.saturating_sub(1),
            l2_block_number: block,
            prev_output_root: B256::repeat_byte(3),
            config_hash: B256::repeat_byte(4),
            schedule_id: B256::repeat_byte(5),
        };

        let journal = ProofJournal {
            proposer: PROPOSER,
            l1_origin_hash: proposal.l1_origin_hash,
            prev_output_root: proposal.prev_output_root,
            starting_l2_block: block - 1,
            output_root: proposal.output_root,
            ending_l2_block: block,
            intermediate_roots: Vec::new(),
            config_hash: proposal.config_hash,
            tee_image_hash: TEE_IMAGE_HASH,
            schedule_id: proposal.schedule_id,
        };
        let signature = signer.sign_hash_sync(&keccak256(journal.encode())).unwrap();
        proposal.signature = Bytes::from(signature.as_rsy().to_vec());
        proposal
    }

    fn nitro_tee_proof(signer: &PrivateKeySigner) -> NitroProofResult {
        NitroProofResult::Tee {
            aggregate_proposal: proposal(signer, 10),
            proposals: vec![proposal(signer, 8), proposal(signer, 9), proposal(signer, 10)],
        }
    }

    #[test]
    fn tee_proof_request_recovers_signer_from_proposal() {
        let signer = PrivateKeySigner::random();
        let request = ProofSubmitterRequest::from_tee_proof(
            "session-1".to_string(),
            "lock-1".to_string(),
            "worker-1".to_string(),
            nitro_tee_proof(&signer),
            PROPOSER,
            TEE_IMAGE_HASH,
        )
        .expect("tee proof should build a submission request");

        assert_eq!(request.session_id, "session-1");
        assert_eq!(request.lock_id, "lock-1");
        assert_eq!(request.worker_id, "worker-1");
        let ServiceProofResult::Tee(result) = request.result else {
            panic!("expected tee proof result");
        };
        assert_eq!(result.tee_signer, signer.address());
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
            PROPOSER,
            TEE_IMAGE_HASH,
        );

        assert!(matches!(result, Err(ProofSubmitterRequestError::UnsupportedProofResult)));
    }
}
