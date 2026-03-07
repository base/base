use alloc::vec::Vec;
use core::fmt;

use alloy_primitives::B256;
use base_proof_preimage::PreimageKey;

use crate::{ECDSA_SIGNATURE_LENGTH, Proposal};

/// Maximum number of individual proposals allowed in a [`ProofClaim`].
///
/// This bounds the size of the `proposals` vector to prevent memory exhaustion
/// when deserializing from untrusted input. Each [`Proposal`] contains several
/// 32-byte hashes, a `U256`, and a variable-length signature, so the per-element
/// cost is non-trivial.
pub const MAX_PROPOSALS: usize = 1024;

/// The claim being proven, containing an aggregated proposal over a block range
/// and the individual per-block proposals that were aggregated.
///
/// The TEE server generates a [`Proposal`] for each block in the range, then
/// aggregates them into a single [`Proposal`]. Verifiers can check both
/// per-block correctness and aggregation integrity.
///
/// # Validation
///
/// A `ProofClaim` deserialized or received from an untrusted source should be
/// validated via [`ProofClaim::validate`] before use. The struct does **not**
/// enforce that `aggregate_proposal` is a correct aggregation of `proposals`;
/// that invariant must be checked by the consumer's verification logic.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProofClaim {
    /// The aggregated proposal covering the entire proven block range.
    pub aggregate_proposal: Proposal,
    /// The individual per-block proposals that were aggregated.
    pub proposals: Vec<Proposal>,
}

/// Errors returned by [`ProofClaim::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProofClaimError {
    /// The `proposals` vector is empty.
    EmptyProposals,
    /// The `proposals` vector exceeds [`MAX_PROPOSALS`].
    TooManyProposals {
        /// The number of proposals present.
        count: usize,
    },
    /// A proposal has an invalid signature length.
    InvalidSignatureLength {
        /// Index of the offending proposal, or `None` for the aggregate.
        index: Option<usize>,
        /// The actual signature length.
        length: usize,
    },
}

impl fmt::Display for ProofClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyProposals => write!(f, "proposals vector is empty"),
            Self::TooManyProposals { count } => {
                write!(f, "too many proposals: {count} exceeds maximum {MAX_PROPOSALS}")
            }
            Self::InvalidSignatureLength { index, length } => match index {
                Some(i) => write!(
                    f,
                    "proposal[{i}] has invalid signature length: expected {ECDSA_SIGNATURE_LENGTH}, got {length}"
                ),
                None => write!(
                    f,
                    "aggregate proposal has invalid signature length: expected {ECDSA_SIGNATURE_LENGTH}, got {length}"
                ),
            },
        }
    }
}

impl core::error::Error for ProofClaimError {}

impl ProofClaim {
    /// Validates basic structural invariants of this claim.
    ///
    /// Checks that:
    /// - The `proposals` vector is non-empty and within [`MAX_PROPOSALS`].
    /// - Every proposal (aggregate and per-block) has a signature of the
    ///   expected [`ECDSA_SIGNATURE_LENGTH`].
    ///
    /// This does **not** verify cryptographic signatures or that the aggregate
    /// is a correct aggregation of the individual proposals. Those checks are
    /// the responsibility of the consumer's verification logic.
    pub fn validate(&self) -> Result<(), ProofClaimError> {
        if self.proposals.is_empty() {
            return Err(ProofClaimError::EmptyProposals);
        }

        if self.proposals.len() > MAX_PROPOSALS {
            return Err(ProofClaimError::TooManyProposals { count: self.proposals.len() });
        }

        if self.aggregate_proposal.signature.len() != ECDSA_SIGNATURE_LENGTH {
            return Err(ProofClaimError::InvalidSignatureLength {
                index: None,
                length: self.aggregate_proposal.signature.len(),
            });
        }

        for (i, proposal) in self.proposals.iter().enumerate() {
            if proposal.signature.len() != ECDSA_SIGNATURE_LENGTH {
                return Err(ProofClaimError::InvalidSignatureLength {
                    index: Some(i),
                    length: proposal.signature.len(),
                });
            }
        }

        Ok(())
    }
}

/// Backend-specific evidence that accompanies a [`ProofClaim`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ProofEvidence {
    /// Evidence produced by a TEE backend.
    Tee {
        /// The attestation document from the enclave.
        attestation_doc: Vec<u8>,
        /// The signature over the claim.
        signature: Vec<u8>,
    },
    /// Evidence produced by a ZK backend.
    Zk {
        /// The ZK proof bytes.
        proof_bytes: Vec<u8>,
        /// The image ID (program hash) that produced this proof.
        image_id: B256,
    },
}

/// A proven claim: a [`ProofClaim`] paired with its [`ProofEvidence`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProofResult {
    /// The claim being proven.
    pub claim: ProofClaim,
    /// The backend-specific evidence for this claim.
    pub evidence: ProofEvidence,
}

/// Per-proof parameters — which block to prove.
///
/// Maps 1:1 to `BootInfo` local preimage keys on the guest side.
/// Changes every proof; the caller passes one per `prove_block()` call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProofRequest {
    /// Hash of the L1 head block.
    pub l1_head: B256,
    /// Hash of the agreed-upon safe L2 block.
    pub agreed_l2_head_hash: B256,
    /// Agreed safe L2 output root to start derivation from.
    pub agreed_l2_output_root: B256,
    /// Claimed L2 output root to validate.
    pub claimed_l2_output_root: B256,
    /// L2 block number that the claimed output root commits to.
    pub claimed_l2_block_number: u64,
}

/// A proof request bundled with the witness data needed to fulfill it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProofBundle {
    /// What to prove.
    pub request: ProofRequest,
    /// The preimage key-value pairs.
    pub preimages: Vec<(PreimageKey, Vec<u8>)>,
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::vec;

    use alloy_primitives::{B256, Bytes, U256};

    use super::*;

    fn valid_proposal() -> Proposal {
        Proposal {
            output_root: B256::ZERO,
            signature: Bytes::from(vec![0xab; ECDSA_SIGNATURE_LENGTH]),
            l1_origin_hash: B256::ZERO,
            l1_origin_number: U256::from(100),
            l2_block_number: U256::from(42),
            prev_output_root: B256::ZERO,
            config_hash: B256::ZERO,
        }
    }

    fn valid_claim() -> ProofClaim {
        ProofClaim { aggregate_proposal: valid_proposal(), proposals: vec![valid_proposal()] }
    }

    #[test]
    fn validate_accepts_valid_claim() {
        assert!(valid_claim().validate().is_ok());
    }

    #[test]
    fn validate_accepts_multiple_proposals() {
        let claim = ProofClaim {
            aggregate_proposal: valid_proposal(),
            proposals: vec![valid_proposal(), valid_proposal(), valid_proposal()],
        };
        assert!(claim.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_proposals() {
        let claim = ProofClaim { aggregate_proposal: valid_proposal(), proposals: vec![] };
        assert_eq!(claim.validate(), Err(ProofClaimError::EmptyProposals));
    }

    #[test]
    fn validate_accepts_max_proposals() {
        let claim = ProofClaim {
            aggregate_proposal: valid_proposal(),
            proposals: vec![valid_proposal(); MAX_PROPOSALS],
        };
        assert!(claim.validate().is_ok());
    }

    #[test]
    fn validate_rejects_too_many_proposals() {
        let claim = ProofClaim {
            aggregate_proposal: valid_proposal(),
            proposals: vec![valid_proposal(); MAX_PROPOSALS + 1],
        };
        assert_eq!(
            claim.validate(),
            Err(ProofClaimError::TooManyProposals { count: MAX_PROPOSALS + 1 })
        );
    }

    #[test]
    fn validate_rejects_short_aggregate_signature() {
        let mut claim = valid_claim();
        claim.aggregate_proposal.signature = Bytes::from(vec![0xab; 64]);
        assert_eq!(
            claim.validate(),
            Err(ProofClaimError::InvalidSignatureLength { index: None, length: 64 })
        );
    }

    #[test]
    fn validate_rejects_long_aggregate_signature() {
        let mut claim = valid_claim();
        claim.aggregate_proposal.signature = Bytes::from(vec![0xab; 66]);
        assert_eq!(
            claim.validate(),
            Err(ProofClaimError::InvalidSignatureLength { index: None, length: 66 })
        );
    }

    #[test]
    fn validate_rejects_bad_proposal_signature() {
        let mut bad = valid_proposal();
        bad.signature = Bytes::new();
        let claim = ProofClaim {
            aggregate_proposal: valid_proposal(),
            proposals: vec![valid_proposal(), bad],
        };
        assert_eq!(
            claim.validate(),
            Err(ProofClaimError::InvalidSignatureLength { index: Some(1), length: 0 })
        );
    }

    #[test]
    fn error_display_messages() {
        assert_eq!(ProofClaimError::EmptyProposals.to_string(), "proposals vector is empty");

        assert_eq!(
            ProofClaimError::TooManyProposals { count: 2000 }.to_string(),
            "too many proposals: 2000 exceeds maximum 1024"
        );

        assert_eq!(
            ProofClaimError::InvalidSignatureLength { index: None, length: 64 }.to_string(),
            "aggregate proposal has invalid signature length: expected 65, got 64"
        );

        assert_eq!(
            ProofClaimError::InvalidSignatureLength { index: Some(3), length: 0 }.to_string(),
            "proposal[3] has invalid signature length: expected 65, got 0"
        );
    }
}
