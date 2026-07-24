//! Pluggable post-execution inclusion policy for the flashblocks build loop.
//!
//! After the builder executes a candidate transaction — but before it commits the result to the
//! flashblock — it consults an [`InclusionPolicy`] to decide whether the transaction is included or
//! dropped. The default [`DefaultInclusionPolicy`] includes every executed transaction, reproducing
//! the builder's historical behavior exactly (where even reverting transactions are committed).
//!
//! This is a generic, transaction-agnostic seam. The policy sees only the neutral
//! [`CandidateOutcome`] — hash, sender, execution result, and block position — and returns an
//! [`InclusionDecision`]. It carries no knowledge of any particular transaction class; an external
//! implementation may associate its own metadata with a candidate out of band, keyed by
//! [`CandidateOutcome::hash`].

use alloy_primitives::{Address, TxHash};

/// The outcome of executing a candidate transaction, presented to an [`InclusionPolicy`] before the
/// result is committed to the flashblock.
#[derive(Debug, Clone, Copy)]
pub struct CandidateOutcome {
    /// The transaction hash.
    pub hash: TxHash,
    /// The transaction sender.
    pub sender: Address,
    /// Gas used by the execution.
    pub gas_used: u64,
    /// Whether the execution reverted (as opposed to succeeding).
    pub reverted: bool,
    /// The transaction's effective tip per gas at the current base fee.
    pub effective_tip_per_gas: u128,
    /// Cumulative block gas used before this transaction would be committed.
    pub cumulative_gas_used: u64,
    /// The block gas limit being targeted.
    pub block_gas_limit: u64,
    /// Index of the flashblock currently being built (0-based).
    pub flashblock_index: u64,
    /// Total number of flashblocks targeted for this block.
    pub target_flashblock_count: u64,
}

impl CandidateOutcome {
    /// Whether the flashblock currently being built is the final one of the block.
    #[must_use]
    pub const fn is_final_flashblock(&self) -> bool {
        self.flashblock_index + 1 >= self.target_flashblock_count
    }
}

/// Whether an executed candidate is committed to the flashblock or dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InclusionDecision {
    /// Commit the executed transaction to the flashblock (the default).
    Include,
    /// Drop the executed transaction: its state changes are discarded and it is not included in the
    /// block. The builder marks it invalid so dependent transactions from the same sender are
    /// skipped too, matching how other post-execution rejections behave.
    Skip,
}

/// Decides whether an executed candidate transaction is committed to the flashblock.
///
/// The builder calls [`InclusionPolicy::decide`] after executing each candidate and before
/// committing it. The default [`DefaultInclusionPolicy`] returns [`InclusionDecision::Include`] for
/// every transaction, reproducing the builder's historical behavior (including reverting
/// transactions).
pub trait InclusionPolicy: Send + Sync + std::fmt::Debug {
    /// Decide whether the just-executed candidate should be committed.
    fn decide(&self, outcome: &CandidateOutcome) -> InclusionDecision;
}

/// The default inclusion policy: include every executed transaction, unchanged.
///
/// This reproduces the builder's pre-seam behavior byte-for-byte — reverting transactions are still
/// committed to the block.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultInclusionPolicy;

impl InclusionPolicy for DefaultInclusionPolicy {
    fn decide(&self, _outcome: &CandidateOutcome) -> InclusionDecision {
        InclusionDecision::Include
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, TxHash};

    use super::*;

    fn outcome(reverted: bool) -> CandidateOutcome {
        CandidateOutcome {
            hash: TxHash::ZERO,
            sender: Address::ZERO,
            gas_used: 21_000,
            reverted,
            effective_tip_per_gas: 1,
            cumulative_gas_used: 0,
            block_gas_limit: 30_000_000,
            flashblock_index: 0,
            target_flashblock_count: 4,
        }
    }

    /// A sample policy that drops reverting transactions — the shape an external revert-protection
    /// policy takes — exercised here to prove the seam distinguishes reverts.
    #[derive(Debug)]
    struct DropReverts;
    impl InclusionPolicy for DropReverts {
        fn decide(&self, o: &CandidateOutcome) -> InclusionDecision {
            if o.reverted { InclusionDecision::Skip } else { InclusionDecision::Include }
        }
    }

    #[test]
    fn default_policy_includes_everything() {
        assert_eq!(DefaultInclusionPolicy.decide(&outcome(false)), InclusionDecision::Include);
        // Reverting transactions are still included under the default — historical behavior.
        assert_eq!(DefaultInclusionPolicy.decide(&outcome(true)), InclusionDecision::Include);
    }

    #[test]
    fn alternative_policy_can_drop_reverts() {
        assert_eq!(DropReverts.decide(&outcome(false)), InclusionDecision::Include);
        assert_eq!(DropReverts.decide(&outcome(true)), InclusionDecision::Skip);
    }

    #[test]
    fn final_flashblock_is_detected() {
        let mut o = outcome(false);
        o.flashblock_index = 2;
        o.target_flashblock_count = 4;
        assert!(!o.is_final_flashblock());
        o.flashblock_index = 3;
        assert!(o.is_final_flashblock());
    }
}
