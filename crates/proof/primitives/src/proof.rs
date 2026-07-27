use alloc::vec::Vec;

use alloy_primitives::{Address, B256};
use base_proof_preimage::PreimageKey;

use crate::Proposal;

/// The result of a proof computation, parameterized by backend.
///
/// Each variant carries exactly the data its backend needs:
///
/// - **`Tee`**: The TEE server generates a [`Proposal`] for each block in the
///   range, then aggregates them into a single [`Proposal`]. On-chain
///   verification uses the per-proposal ECDSA signatures directly; the
///   attestation document is handled separately at signer registration time.
///
/// - **`Zk`**: The ZK prover produces an opaque proof blob that the on-chain
///   verifier checks against a committed image ID.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ProofResult {
    /// Result from a TEE backend.
    Tee {
        /// The aggregated proposal covering the entire proven block range.
        aggregate_proposal: Proposal,
        /// The individual per-block proposals that were aggregated.
        proposals: Vec<Proposal>,
    },
    /// Result from a ZK backend.
    Zk {
        /// The ZK proof bytes.
        proof_bytes: Vec<u8>,
    },
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
    /// Address of the proposer that will submit the proof transaction on-chain.
    ///
    /// Included in the proof journal so on-chain verification can match it against
    /// the actual `msg.sender` (gameCreator).
    #[cfg_attr(feature = "serde", serde(default))]
    pub proposer: Address,
    /// Number of L2 blocks between intermediate output root checkpoints.
    ///
    /// Used by the enclave to sample the correct intermediate roots when
    /// constructing the aggregate proof journal.
    #[cfg_attr(feature = "serde", serde(default))]
    pub intermediate_block_interval: u64,
    /// Block number of the L1 head.
    ///
    /// Stored alongside `l1_head` (the hash) so the enclave can reference the
    /// L1 head block number without an extra lookup.
    #[cfg_attr(feature = "serde", serde(default))]
    pub l1_head_number: u64,
    /// Keccak256 hash of the expected enclave PCR0 measurement.
    ///
    /// Used by multi-enclave provers to select the enclave whose PCR0
    /// matches the on-chain `TEE_IMAGE_HASH`. Single-enclave provers
    /// accept and ignore this field.
    #[cfg_attr(feature = "serde", serde(default))]
    pub image_hash: B256,
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
