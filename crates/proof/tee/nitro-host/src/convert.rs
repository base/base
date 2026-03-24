//! Conversions between enclave-native and host-side proof types.

use base_proof_primitives::{ProofResult, Proposal};
use base_proof_tee_nitro_enclave::{Proposal as EnclaveProposal, TeeProofResult};

pub(super) fn proposal_from_enclave(p: EnclaveProposal) -> Proposal {
    Proposal {
        output_root: p.output_root,
        signature: p.signature,
        l1_origin_hash: p.l1_origin_hash,
        l1_origin_number: p.l1_origin_number,
        l2_block_number: p.l2_block_number,
        prev_output_root: p.prev_output_root,
        config_hash: p.config_hash,
    }
}

pub(super) fn proof_result_from_enclave(t: TeeProofResult) -> ProofResult {
    ProofResult::Tee {
        aggregate_proposal: proposal_from_enclave(t.aggregate_proposal),
        proposals: t.proposals.into_iter().map(proposal_from_enclave).collect(),
    }
}
