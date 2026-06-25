//! Dual TEE proof types for proposer submissions.

use alloy_primitives::{B256, Bytes};
use base_proof_primitives::{ProofEncoder, Proposal};

use crate::ProposerError;

/// Expected image hashes for the two TEE platforms required by the verifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeeImageHashes {
    /// AWS Nitro image hash.
    pub nitro: B256,
    /// Intel TDX image hash.
    pub tdx: B256,
}

impl TeeImageHashes {
    /// Returns the image hash for a prover-service TEE kind.
    pub const fn for_kind(&self, tee_kind: base_prover_service_protocol::TeeKind) -> B256 {
        match tee_kind {
            base_prover_service_protocol::TeeKind::AwsNitro => self.nitro,
            base_prover_service_protocol::TeeKind::IntelTdx => self.tdx,
        }
    }
}

/// A single-platform TEE proof returned by prover-service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeeProof {
    /// Image hash used in the signed journal.
    pub image_hash: B256,
    /// Aggregate proposal for the whole range.
    pub aggregate_proposal: Proposal,
    /// Per-block proposals used for intermediate roots.
    pub proposals: Vec<Proposal>,
}

/// The paired Nitro and TDX proofs required by `AggregateVerifier`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeeProofPair {
    /// AWS Nitro proof.
    pub nitro: TeeProof,
    /// Intel TDX proof.
    pub tdx: TeeProof,
}

impl TeeProofPair {
    /// Builds a pair after checking both platforms signed the same public inputs.
    pub fn new(nitro: TeeProof, tdx: TeeProof) -> Result<Self, ProposerError> {
        Self::validate_matching_public_inputs(&nitro.aggregate_proposal, &tdx.aggregate_proposal)?;
        if nitro.proposals.len() != tdx.proposals.len() {
            return Err(ProposerError::Prover(format!(
                "TEE proof proposal count mismatch: nitro={}, tdx={}",
                nitro.proposals.len(),
                tdx.proposals.len()
            )));
        }
        for (index, (nitro, tdx)) in nitro.proposals.iter().zip(&tdx.proposals).enumerate() {
            Self::validate_matching_public_inputs(nitro, tdx).map_err(|e| {
                ProposerError::Prover(format!("TEE proof proposal {index} mismatch: {e}"))
            })?;
        }
        Ok(Self { nitro, tdx })
    }

    /// Returns the aggregate proposal public inputs.
    pub const fn aggregate_proposal(&self) -> &Proposal {
        &self.nitro.aggregate_proposal
    }

    /// Returns the per-block proposals used for intermediate root extraction.
    pub fn proposals(&self) -> &[Proposal] {
        &self.nitro.proposals
    }

    /// Builds init proof data for `AggregateVerifier.initializeWithInitData()`.
    pub fn build_proof_data(&self) -> Result<Bytes, ProposerError> {
        let proposal = self.aggregate_proposal();
        ProofEncoder::encode_dual_tee_proof_bytes(
            self.nitro.image_hash,
            &self.nitro.aggregate_proposal.signature,
            self.tdx.image_hash,
            &self.tdx.aggregate_proposal.signature,
            proposal.l1_origin_hash,
            proposal.l1_origin_number,
        )
        .map_err(|e| ProposerError::Internal(e.to_string()))
    }

    /// Builds compact proof bytes for `AggregateVerifier.verifyProposalProof()`.
    pub fn build_dispute_proof_bytes(&self) -> Result<Bytes, ProposerError> {
        ProofEncoder::encode_dual_tee_dispute_proof_bytes(
            self.nitro.image_hash,
            &self.nitro.aggregate_proposal.signature,
            self.tdx.image_hash,
            &self.tdx.aggregate_proposal.signature,
        )
        .map_err(|e| ProposerError::Internal(e.to_string()))
    }

    fn validate_matching_public_inputs(
        left: &Proposal,
        right: &Proposal,
    ) -> Result<(), ProposerError> {
        if left.output_root != right.output_root
            || left.l1_origin_hash != right.l1_origin_hash
            || left.l1_origin_number != right.l1_origin_number
            || left.l2_block_number != right.l2_block_number
            || left.prev_output_root != right.prev_output_root
            || left.config_hash != right.config_hash
        {
            return Err(ProposerError::Prover("TEE proofs signed different public inputs".into()));
        }
        Ok(())
    }
}
