use alloc::vec::Vec;
use core::num::NonZeroU64;

use alloy_primitives::{Address, B256};
use base_proof_preimage::PreimageKey;

use crate::Proposal;

/// The result of a proof computation, parameterized by backend.
///
/// Each variant carries exactly the data its backend needs:
///
/// - **`Tee`**: The TEE server signs one [`Proposal`] covering the whole range,
///   committing to the intermediate output roots it sampled. On-chain
///   verification checks that ECDSA signature against the journal it rebuilds
///   from the game; the attestation document is handled separately at signer
///   registration time.
///
/// - **`Zk`**: The ZK prover produces an opaque proof blob that the on-chain
///   verifier checks against a committed image ID.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ProofResult {
    /// Result from a TEE backend.
    Tee {
        /// The signed proposal covering the entire proven block range.
        aggregate_proposal: Proposal,
        /// The intermediate output roots committed by the aggregate signature.
        intermediate_roots: Vec<B256>,
        /// Stamped by the enclave at signing time, so the signer and the proof
        /// are returned atomically.
        tee_signer: Address,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Block number of the L1 head.
    ///
    /// Stored alongside `l1_head` (the hash) so the enclave can reference the
    /// L1 head block number without an extra lookup.
    #[cfg_attr(feature = "serde", serde(default))]
    pub l1_head_number: u64,
    /// L2 block used to pin the upgrade schedule; defaults to the claimed block.
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
    pub schedule_l2_block_number: Option<u64>,
    /// Number of L2 blocks between two intermediate output roots of the game being proven.
    pub intermediate_block_interval: NonZeroU64,
}

#[cfg(any(test, feature = "test-utils"))]
impl ProofRequest {
    /// Builds a zeroed request for a game whose range ends at `claimed_l2_block_number`, sampled
    /// every `intermediate_block_interval` blocks.
    pub const fn for_tests(claimed_l2_block_number: u64, intermediate_block_interval: u64) -> Self {
        Self {
            l1_head: B256::ZERO,
            agreed_l2_head_hash: B256::ZERO,
            agreed_l2_output_root: B256::ZERO,
            claimed_l2_output_root: B256::ZERO,
            claimed_l2_block_number,
            proposer: Address::ZERO,
            l1_head_number: 0,
            schedule_l2_block_number: None,
            intermediate_block_interval: NonZeroU64::new(intermediate_block_interval)
                .expect("intermediate block interval must be non-zero"),
        }
    }
}

/// A proof request bundled with the witness data needed to fulfill it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProofBundle {
    /// What to prove.
    pub request: ProofRequest,
    /// The preimage key-value pairs.
    pub preimages: Vec<(PreimageKey, Vec<u8>)>,
}
