//! Submission types: actions to submit against a dispute game.

use std::fmt;

use alloy_primitives::B256;

/// On-chain action this challenger will submit against a dispute game.
///
/// Carries the call parameters for the matching `AggregateVerifier`
/// entrypoint. The proof bytes (TEE signature, ZK SNARK) are produced
/// separately by `prove()` and bundled into `SubmitRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisputeAction {
    /// Calls `challenge(index, our_root)`. Used as failover when
    /// `NullifyTee` is unavailable (no TEE prover, or our local TEE
    /// returned a wrong root).
    Challenge {
        /// Intermediate root index disputed by the challenger.
        index: u64,
        /// The correct root we are asserting.
        our_root: B256,
        /// Predecessor root (anchor or previous intermediate root).
        starting_root: B256,
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
        /// Root attested by our local TEE prover.
        our_root: B256,
        /// Predecessor root (anchor or previous intermediate root).
        starting_root: B256,
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
        /// Root the SNARK is asserting (correct L2 root for `ZkWrong`,
        /// on-chain TEE root for `FraudulentZkChallenge`).
        root_to_prove: B256,
        /// Predecessor root (anchor or previous intermediate root).
        starting_root: B256,
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
