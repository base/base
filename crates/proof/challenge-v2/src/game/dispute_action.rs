//! Dispute action types and L1 calldata encoding.
//!
//! [`DisputeAction`] enumerates the on-chain calls the challenger can
//! make against a dispute game. [`DisputeRequest`] bundles an action
//! with the proof bytes that prove it. Produced by
//! [`crate::Violation::build_dispute_request`] and consumed by
//! [`crate::SubmissionTask`].

use std::fmt;

use alloy_primitives::{Address, B256, Bytes};
use base_proof_contracts::{encode_challenge_calldata, encode_nullify_calldata};

/// On-chain action this challenger will submit against a dispute game.
///
/// Carries the call parameters for the matching `AggregateVerifier`
/// entrypoint. The proof bytes (TEE signature, ZK SNARK) are produced
/// separately by [`crate::Violation::build_dispute_request`] and bundled
/// into [`DisputeRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisputeAction {
    /// Calls `challenge(index, our_root)`. Used as failover when
    /// [`Self::NullifyTee`] cannot be produced; see
    /// [`crate::Violation::build_dispute_request`] for the exact fallback
    /// conditions.
    Challenge {
        /// Intermediate root index disputed by the challenger.
        index: u64,
        /// The root we are asserting (computed from our L2 RPC).
        our_root: B256,
        /// Predecessor root (anchor or previous intermediate root).
        start_root: B256,
        /// L2 block at the start of the challenged range.
        start_block: u64,
        /// L2 block at the end of the challenged range.
        end_block: u64,
    },
    /// Calls `nullify(index, our_root)` with TEE proof bytes
    /// (`proof_type == 0`). Kills `TEE_VERIFIER` globally.
    NullifyTee {
        /// Intermediate root index disputed by the challenger.
        index: u64,
        /// Root attested by our TEE prover.
        our_root: B256,
        /// Predecessor root (anchor or previous intermediate root).
        start_root: B256,
        /// L2 block at the start of the challenged range.
        start_block: u64,
        /// L2 block at the end of the challenged range.
        end_block: u64,
    },
    /// Calls `nullify(index, root_to_prove)` with ZK proof bytes
    /// (`proof_type == 1`). Kills `ZK_VERIFIER` globally.
    NullifyZk {
        /// Intermediate root index disputed by the challenger.
        index: u64,
        /// Root the SNARK is asserting (our computed L2 root for
        /// `ZkWrong`, proposed root for `FraudulentZkChallenge`).
        root_to_prove: B256,
        /// Predecessor root (anchor or previous intermediate root).
        start_root: B256,
        /// L2 block at the start of the challenged range.
        start_block: u64,
        /// L2 block at the end of the challenged range.
        end_block: u64,
    },
}

impl DisputeAction {
    /// Returns the intermediate root index this action targets.
    pub const fn index(&self) -> u64 {
        match self {
            Self::Challenge { index, .. }
            | Self::NullifyTee { index, .. }
            | Self::NullifyZk { index, .. } => *index,
        }
    }

    /// Encodes the L1 calldata for this action, prepending the proof
    /// type discriminator already baked into `proof_bytes`.
    ///
    /// Dispatches to the matching `AggregateVerifier` entrypoint:
    /// - [`Self::Challenge`] uses [`encode_challenge_calldata`].
    /// - [`Self::NullifyTee`] and [`Self::NullifyZk`] use
    ///   [`encode_nullify_calldata`]; the encoder picks the matching
    ///   verifier by reading the first byte of `proof_bytes`
    ///   (`0` for TEE, `1` for ZK).
    pub fn to_calldata(&self, proof_bytes: Bytes) -> Bytes {
        match self {
            Self::Challenge { index, our_root, .. } => {
                encode_challenge_calldata(proof_bytes, *index, *our_root)
            }
            Self::NullifyTee { index, our_root, .. } => {
                encode_nullify_calldata(proof_bytes, *index, *our_root)
            }
            Self::NullifyZk { index, root_to_prove, .. } => {
                encode_nullify_calldata(proof_bytes, *index, *root_to_prove)
            }
        }
    }
}

impl fmt::Display for DisputeAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Challenge { .. } => "Challenge",
            Self::NullifyTee { .. } => "NullifyTee",
            Self::NullifyZk { .. } => "NullifyZk",
        })
    }
}

/// A `DisputeAction` bundled with the proof bytes that prove it.
///
/// Produced by [`crate::Violation::build_dispute_request`] and consumed by
/// [`crate::SubmissionTask`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisputeRequest {
    /// Dispute game proxy this action targets.
    pub game_address: Address,
    /// Action to call on the game contract.
    pub action: DisputeAction,
    /// Proof bytes prefixed with the proof type discriminator
    /// (`PROOF_TYPE_TEE = 0` or `PROOF_TYPE_ZK = 1`).
    pub proof_bytes: Bytes,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn challenge(index: u64, our_root: B256) -> DisputeAction {
        DisputeAction::Challenge {
            index,
            our_root,
            start_root: B256::ZERO,
            start_block: 100,
            end_block: 200,
        }
    }

    fn nullify_tee(index: u64, our_root: B256) -> DisputeAction {
        DisputeAction::NullifyTee {
            index,
            our_root,
            start_root: B256::ZERO,
            start_block: 100,
            end_block: 200,
        }
    }

    fn nullify_zk(index: u64, root_to_prove: B256) -> DisputeAction {
        DisputeAction::NullifyZk {
            index,
            root_to_prove,
            start_root: B256::ZERO,
            start_block: 100,
            end_block: 200,
        }
    }

    #[test]
    fn challenge_dispatches_to_encode_challenge_calldata() {
        let proof = Bytes::from(vec![0x01, 0xAA, 0xBB]);
        let root = B256::repeat_byte(0xAB);

        let calldata = challenge(7, root).to_calldata(proof.clone());

        assert_eq!(calldata, encode_challenge_calldata(proof, 7, root));
    }

    #[test]
    fn nullify_tee_dispatches_to_encode_nullify_with_our_root() {
        let proof = Bytes::from(vec![0x00, 0xCC, 0xDD]);
        let root = B256::repeat_byte(0xCD);

        let calldata = nullify_tee(3, root).to_calldata(proof.clone());

        assert_eq!(calldata, encode_nullify_calldata(proof, 3, root));
    }

    #[test]
    fn nullify_zk_dispatches_to_encode_nullify_with_root_to_prove() {
        let proof = Bytes::from(vec![0x01, 0xEE, 0xFF]);
        let root = B256::repeat_byte(0xEF);

        let calldata = nullify_zk(5, root).to_calldata(proof.clone());

        assert_eq!(calldata, encode_nullify_calldata(proof, 5, root));
    }

    #[test]
    fn ignores_start_root_and_block_range_fields() {
        // start_root, start_block and end_block are local context for
        // submission ergonomics; they must not appear in the L1 calldata.
        let proof = Bytes::from(vec![0x01]);
        let root = B256::repeat_byte(0x42);
        let same_index_same_root = DisputeAction::Challenge {
            index: 1,
            our_root: root,
            start_root: B256::repeat_byte(0xAA),
            start_block: 1,
            end_block: 2,
        };
        let other_context = DisputeAction::Challenge {
            index: 1,
            our_root: root,
            start_root: B256::repeat_byte(0xBB),
            start_block: 999,
            end_block: 1000,
        };

        assert_eq!(
            same_index_same_root.to_calldata(proof.clone()),
            other_context.to_calldata(proof),
        );
    }
}
