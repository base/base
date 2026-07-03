//! TEE proof types for proposer submissions.

use alloy_primitives::{B256, Bytes};
use base_proof_primitives::{ProofEncoder, Proposal};
use base_prover_service_protocol::TeeKind;
use clap::ValueEnum;

use crate::ProposerError;

/// TEE platforms required before submitting a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TeeProofMode {
    /// Require only an AWS Nitro proof.
    Nitro,
    /// Require only an Intel TDX proof.
    Tdx,
    /// Require both AWS Nitro and Intel TDX proofs.
    Both,
}

impl TeeProofMode {
    /// Returns true when the mode requires a Nitro proof.
    pub const fn requires_nitro(self) -> bool {
        matches!(self, Self::Nitro | Self::Both)
    }

    /// Returns true when the mode requires a TDX proof.
    pub const fn requires_tdx(self) -> bool {
        matches!(self, Self::Tdx | Self::Both)
    }

    /// Returns the TEE kinds required by this mode.
    pub const fn tee_kinds(self) -> &'static [TeeKind] {
        match self {
            Self::Nitro => &[TeeKind::AwsNitro],
            Self::Tdx => &[TeeKind::IntelTdx],
            Self::Both => &[TeeKind::AwsNitro, TeeKind::IntelTdx],
        }
    }
}

impl Default for TeeProofMode {
    fn default() -> Self {
        Self::Nitro
    }
}

impl std::fmt::Display for TeeProofMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nitro => write!(f, "nitro"),
            Self::Tdx => write!(f, "tdx"),
            Self::Both => write!(f, "both"),
        }
    }
}

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
    pub const fn for_kind(&self, tee_kind: TeeKind) -> B256 {
        match tee_kind {
            TeeKind::AwsNitro => self.nitro,
            TeeKind::IntelTdx => self.tdx,
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

/// The TEE proofs required by the configured proposer mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeeProofPair {
    /// AWS Nitro proof.
    Nitro(TeeProof),
    /// Intel TDX proof.
    Tdx(TeeProof),
    /// AWS Nitro and Intel TDX proofs over the same public inputs.
    Both {
        /// AWS Nitro proof.
        nitro: TeeProof,
        /// Intel TDX proof.
        tdx: TeeProof,
    },
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
        Ok(Self::Both { nitro, tdx })
    }

    /// Builds the proof set for the configured mode.
    pub fn from_mode(
        mode: TeeProofMode,
        nitro: Option<TeeProof>,
        tdx: Option<TeeProof>,
    ) -> Result<Self, ProposerError> {
        match mode {
            TeeProofMode::Nitro => Ok(Self::Nitro(nitro.ok_or_else(|| {
                ProposerError::Prover("missing required Nitro TEE proof".into())
            })?)),
            TeeProofMode::Tdx => {
                Ok(Self::Tdx(tdx.ok_or_else(|| {
                    ProposerError::Prover("missing required TDX TEE proof".into())
                })?))
            }
            TeeProofMode::Both => Self::new(
                nitro.ok_or_else(|| {
                    ProposerError::Prover("missing required Nitro TEE proof".into())
                })?,
                tdx.ok_or_else(|| ProposerError::Prover("missing required TDX TEE proof".into()))?,
            ),
        }
    }

    /// Returns the aggregate proposal public inputs.
    pub const fn aggregate_proposal(&self) -> &Proposal {
        match self {
            Self::Nitro(proof) | Self::Tdx(proof) => &proof.aggregate_proposal,
            Self::Both { nitro, .. } => &nitro.aggregate_proposal,
        }
    }

    /// Returns the per-block proposals used for intermediate root extraction.
    pub fn proposals(&self) -> &[Proposal] {
        match self {
            Self::Nitro(proof) | Self::Tdx(proof) => &proof.proposals,
            Self::Both { nitro, .. } => &nitro.proposals,
        }
    }

    /// Builds init proof data for `AggregateVerifier.initializeWithInitData()`.
    pub fn build_proof_data(&self) -> Result<Bytes, ProposerError> {
        let proposal = self.aggregate_proposal();
        match self {
            Self::Nitro(proof) | Self::Tdx(proof) => ProofEncoder::encode_proof_bytes(
                &proof.aggregate_proposal.signature,
                proposal.l1_origin_hash,
                proposal.l1_origin_number,
            ),
            Self::Both { nitro, tdx } => ProofEncoder::encode_dual_tee_proof_bytes(
                &nitro.aggregate_proposal.signature,
                &tdx.aggregate_proposal.signature,
                proposal.l1_origin_hash,
                proposal.l1_origin_number,
            ),
        }
        .map_err(|e| ProposerError::Internal(e.to_string()))
    }

    /// Builds compact proof bytes for `AggregateVerifier.verifyProposalProof()`.
    pub fn build_dispute_proof_bytes(&self) -> Result<Bytes, ProposerError> {
        match self {
            Self::Nitro(proof) | Self::Tdx(proof) => {
                ProofEncoder::encode_dispute_proof_bytes(&proof.aggregate_proposal.signature)
            }
            Self::Both { nitro, tdx } => ProofEncoder::encode_dual_tee_dispute_proof_bytes(
                &nitro.aggregate_proposal.signature,
                &tdx.aggregate_proposal.signature,
            ),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNATURE: [u8; 65] = {
        let mut signature = [0xab; 65];
        signature[64] = 1;
        signature
    };

    fn proof(block: u64, image_hash: B256) -> TeeProof {
        TeeProof {
            image_hash,
            aggregate_proposal: Proposal {
                output_root: B256::repeat_byte(block as u8),
                signature: Bytes::from_static(&SIGNATURE),
                l1_origin_hash: B256::repeat_byte(0x01),
                l1_origin_number: block + 10,
                l2_block_number: block,
                prev_output_root: B256::repeat_byte(0x02),
                config_hash: B256::repeat_byte(0x03),
            },
            proposals: Vec::new(),
        }
    }

    #[test]
    fn single_proof_uses_single_signature_encoding() {
        let proof = TeeProofPair::from_mode(
            TeeProofMode::Nitro,
            Some(proof(100, B256::repeat_byte(0x05))),
            None,
        )
        .unwrap();

        assert_eq!(proof.build_proof_data().unwrap().len(), 130);
        assert_eq!(proof.build_dispute_proof_bytes().unwrap().len(), 66);
    }

    #[test]
    fn both_proof_uses_dual_signature_encoding() {
        let proof = TeeProofPair::from_mode(
            TeeProofMode::Both,
            Some(proof(100, B256::repeat_byte(0x05))),
            Some(proof(100, B256::repeat_byte(0x06))),
        )
        .unwrap();

        assert_eq!(proof.build_proof_data().unwrap().len(), 195);
        assert_eq!(proof.build_dispute_proof_bytes().unwrap().len(), 131);
    }
}
