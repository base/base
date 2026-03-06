use alloc::vec::Vec;

use alloy_primitives::B256;
use base_proof_preimage::PreimageKey;

/// The claim being proven: an L2 output root at a given block, anchored to an L1 head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProofClaim {
    /// The L2 block number this claim covers.
    pub l2_block_number: u64,
    /// The L2 output root at `l2_block_number`.
    pub output_root: B256,
    /// The L1 head hash used to derive the L2 state.
    pub l1_head: B256,
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
