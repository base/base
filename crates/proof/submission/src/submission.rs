//! Aggregate verifier proof transaction inputs.

use alloy_primitives::{Address, B256, Bytes};
use base_proof_contracts::{
    encode_challenge_calldata, encode_nullify_calldata, encode_verify_proposal_proof_calldata,
};

/// Inputs for `AggregateVerifier.verifyProposalProof(bytes)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyProposalProofSubmission {
    /// Dispute game contract address.
    pub game_address: Address,
    /// Type-prefixed proof bytes accepted by the dispute game.
    pub proof_bytes: Bytes,
}

impl VerifyProposalProofSubmission {
    /// Creates a proposal proof submission.
    pub const fn new(game_address: Address, proof_bytes: Bytes) -> Self {
        Self { game_address, proof_bytes }
    }

    /// Encodes calldata for `AggregateVerifier.verifyProposalProof(bytes)`.
    pub fn calldata(&self) -> Bytes {
        encode_verify_proposal_proof_calldata(self.proof_bytes.clone())
    }
}

/// Inputs for dispute-game `challenge(bytes,uint256,bytes32)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeProofSubmission {
    /// Dispute game contract address.
    pub game_address: Address,
    /// Type-prefixed proof bytes accepted by the dispute game.
    pub proof_bytes: Bytes,
    /// Zero-based intermediate root index being challenged.
    pub intermediate_root_index: u64,
    /// Correct intermediate root to prove for the challenged index.
    pub intermediate_root_to_prove: B256,
}

impl ChallengeProofSubmission {
    /// Creates a challenge proof submission.
    pub const fn new(
        game_address: Address,
        proof_bytes: Bytes,
        intermediate_root_index: u64,
        intermediate_root_to_prove: B256,
    ) -> Self {
        Self { game_address, proof_bytes, intermediate_root_index, intermediate_root_to_prove }
    }

    /// Encodes calldata for `AggregateVerifier.challenge(bytes,uint256,bytes32)`.
    pub fn calldata(&self) -> Bytes {
        encode_challenge_calldata(
            self.proof_bytes.clone(),
            self.intermediate_root_index,
            self.intermediate_root_to_prove,
        )
    }
}

/// Inputs for dispute-game `nullify(bytes,uint256,bytes32)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NullifyProofSubmission {
    /// Dispute game contract address.
    pub game_address: Address,
    /// Type-prefixed proof bytes accepted by the dispute game.
    pub proof_bytes: Bytes,
    /// Zero-based intermediate root index being nullified.
    pub intermediate_root_index: u64,
    /// Correct intermediate root to prove for the nullified index.
    pub intermediate_root_to_prove: B256,
}

impl NullifyProofSubmission {
    /// Creates a nullify proof submission.
    pub const fn new(
        game_address: Address,
        proof_bytes: Bytes,
        intermediate_root_index: u64,
        intermediate_root_to_prove: B256,
    ) -> Self {
        Self { game_address, proof_bytes, intermediate_root_index, intermediate_root_to_prove }
    }

    /// Encodes calldata for `AggregateVerifier.nullify(bytes,uint256,bytes32)`.
    pub fn calldata(&self) -> Bytes {
        encode_nullify_calldata(
            self.proof_bytes.clone(),
            self.intermediate_root_index,
            self.intermediate_root_to_prove,
        )
    }
}
