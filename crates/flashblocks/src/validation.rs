//! Flashblock sequence validation and reorganization detection.
//!
//! This module provides pure, stateless validation logic for determining
//! whether an incoming flashblock is valid in the context of the current
//! pending state. The validator is designed to be easily unit-testable
//! without any external dependencies.
//!
//! It also provides utilities for detecting chain reorganizations by comparing
//! tracked transaction sets against canonical chain data.

use std::collections::HashSet;

use alloy_primitives::B256;

/// Result of validating a flashblock's position in the sequence.
///
/// This enum represents all possible outcomes when validating whether
/// an incoming flashblock follows the expected sequence relative to
/// the current latest flashblock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SequenceValidationResult {
    /// The flashblock is the next consecutive flashblock within the current block.
    ///
    /// This occurs when:
    /// - `incoming_block_number == latest_block_number`
    /// - `incoming_index == latest_flashblock_index + 1`
    NextInSequence,

    /// The flashblock is the first flashblock (index 0) of the next block.
    ///
    /// This occurs when:
    /// - `incoming_block_number == latest_block_number + 1`
    /// - `incoming_index == 0`
    FirstOfNextBlock,

    /// The flashblock has the same index as the current latest flashblock.
    ///
    /// This is a duplicate that should be ignored.
    Duplicate,

    /// The flashblock has a non-sequential index within the same block.
    ///
    /// This indicates a gap in the flashblock sequence, which means
    /// some flashblocks were missed.
    NonSequentialGap {
        /// The expected flashblock index.
        expected: u64,
        /// The actual incoming flashblock index.
        actual: u64,
    },

    /// A new block was received with a non-zero flashblock index.
    ///
    /// The first flashblock of any new block must have index 0.
    /// Receiving a non-zero index for a new block means we missed
    /// the base flashblock.
    InvalidNewBlockIndex {
        /// The block number of the incoming flashblock.
        block_number: u64,
        /// The invalid (non-zero) index received.
        index: u64,
    },
}

/// Pure validator for flashblock sequence ordering.
///
/// This validator determines whether an incoming flashblock is valid
/// in the context of the current pending state. It is designed to be
/// stateless and easily testable.
///
/// # Example
///
/// ```
/// use base_reth_flashblocks::validation::{FlashblockSequenceValidator, SequenceValidationResult};
///
/// // Validate that flashblock index 3 follows index 2 in block 100
/// let result = FlashblockSequenceValidator::validate(100, 2, 100, 3);
/// assert_eq!(result, SequenceValidationResult::NextInSequence);
///
/// // Validate that flashblock index 0 of block 101 follows any flashblock in block 100
/// let result = FlashblockSequenceValidator::validate(100, 5, 101, 0);
/// assert_eq!(result, SequenceValidationResult::FirstOfNextBlock);
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct FlashblockSequenceValidator;

impl FlashblockSequenceValidator {
    /// Validates whether an incoming flashblock follows the expected sequence.
    ///
    /// This method implements the core validation logic for flashblock ordering:
    ///
    /// 1. **Next in sequence**: The incoming flashblock is the next consecutive
    ///    flashblock within the current block (same block number, index + 1).
    ///
    /// 2. **First of next block**: The incoming flashblock is the first flashblock
    ///    (index 0) of the next block (block number + 1).
    ///
    /// 3. **Duplicate**: The incoming flashblock has the same index as the current
    ///    latest flashblock within the same block.
    ///
    /// 4. **Non-sequential gap**: The incoming flashblock has a different block number
    ///    or a non-consecutive index within the same block.
    ///
    /// 5. **Invalid new block index**: A new block is received with a non-zero index.
    ///
    /// # Arguments
    ///
    /// * `latest_block_number` - The block number of the current latest flashblock.
    /// * `latest_flashblock_index` - The index of the current latest flashblock.
    /// * `incoming_block_number` - The block number of the incoming flashblock.
    /// * `incoming_index` - The index of the incoming flashblock.
    ///
    /// # Returns
    ///
    /// A [`SequenceValidationResult`] indicating the validation outcome.
    pub const fn validate(
        latest_block_number: u64,
        latest_flashblock_index: u64,
        incoming_block_number: u64,
        incoming_index: u64,
    ) -> SequenceValidationResult {
        // Check if this is the next flashblock within the current block
        let is_next_of_block = incoming_block_number == latest_block_number
            && incoming_index == latest_flashblock_index + 1;

        // Check if this is the first flashblock of the next block
        let is_first_of_next_block =
            incoming_block_number == latest_block_number + 1 && incoming_index == 0;

        if is_next_of_block || is_first_of_next_block {
            if is_next_of_block {
                SequenceValidationResult::NextInSequence
            } else {
                SequenceValidationResult::FirstOfNextBlock
            }
        } else if incoming_block_number != latest_block_number {
            // New block with non-zero index
            SequenceValidationResult::InvalidNewBlockIndex {
                block_number: incoming_block_number,
                index: incoming_index,
            }
        } else if incoming_index == latest_flashblock_index {
            // Duplicate flashblock
            SequenceValidationResult::Duplicate
        } else {
            // Non-sequential index within the same block
            SequenceValidationResult::NonSequentialGap {
                expected: latest_flashblock_index + 1,
                actual: incoming_index,
            }
        }
    }
}

/// Result of a reorganization detection check.
///
/// This enum represents whether a chain reorganization was detected
/// by comparing tracked transaction hashes against canonical chain data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReorgDetectionResult {
    /// No reorganization detected - transaction sets match exactly.
    NoReorg,
    /// Reorganization detected - transaction sets differ.
    ///
    /// Contains the counts from both sets for diagnostic purposes.
    ReorgDetected {
        /// Number of transactions in the tracked (pending) set.
        tracked_count: usize,
        /// Number of transactions in the canonical chain set.
        canonical_count: usize,
    },
}

impl ReorgDetectionResult {
    /// Returns `true` if a reorganization was detected.
    #[inline]
    pub const fn is_reorg(&self) -> bool {
        matches!(self, Self::ReorgDetected { .. })
    }

    /// Returns `true` if no reorganization was detected.
    #[inline]
    pub const fn is_no_reorg(&self) -> bool {
        matches!(self, Self::NoReorg)
    }
}

/// A pure utility for detecting chain reorganizations.
///
/// `ReorgDetector` compares two sets of transaction hashes to determine
/// if a reorganization has occurred. A reorg is detected when the tracked
/// transaction set differs from the canonical chain's transaction set,
/// either in count or content.
///
/// # Example
///
/// ```
/// use alloy_primitives::B256;
/// use base_reth_flashblocks::validation::{ReorgDetector, ReorgDetectionResult};
///
/// let tracked = vec![B256::ZERO];
/// let canonical = vec![B256::ZERO];
///
/// let result = ReorgDetector::detect(tracked.iter(), canonical.iter());
/// assert!(result.is_no_reorg());
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ReorgDetector;

impl ReorgDetector {
    /// Detects whether a chain reorganization occurred by comparing transaction hash sets.
    ///
    /// This method compares the tracked (pending) transaction hashes against the
    /// canonical chain's transaction hashes. A reorganization is detected if:
    /// - The number of transactions differs, or
    /// - The sets contain different transaction hashes
    ///
    /// # Arguments
    ///
    /// * `tracked_tx_hashes` - Iterator over transaction hashes from the tracked/pending state.
    /// * `canonical_tx_hashes` - Iterator over transaction hashes from the canonical chain.
    ///
    /// # Returns
    ///
    /// Returns [`ReorgDetectionResult::NoReorg`] if the transaction sets match exactly,
    /// or [`ReorgDetectionResult::ReorgDetected`] with the counts if they differ.
    ///
    /// # Example
    ///
    /// ```
    /// use alloy_primitives::B256;
    /// use base_reth_flashblocks::validation::{ReorgDetector, ReorgDetectionResult};
    ///
    /// // Same transactions - no reorg
    /// let hash = B256::repeat_byte(0x42);
    /// let tracked = vec![hash];
    /// let canonical = vec![hash];
    ///
    /// match ReorgDetector::detect(tracked.iter(), canonical.iter()) {
    ///     ReorgDetectionResult::NoReorg => println!("No reorg detected"),
    ///     ReorgDetectionResult::ReorgDetected { tracked_count, canonical_count } => {
    ///         println!("Reorg! tracked: {}, canonical: {}", tracked_count, canonical_count);
    ///     }
    /// }
    /// ```
    pub fn detect<'a, I1, I2>(
        tracked_tx_hashes: I1,
        canonical_tx_hashes: I2,
    ) -> ReorgDetectionResult
    where
        I1: Iterator<Item = &'a B256>,
        I2: Iterator<Item = &'a B256>,
    {
        let tracked_set: HashSet<&B256> = tracked_tx_hashes.collect();
        let canonical_set: HashSet<&B256> = canonical_tx_hashes.collect();

        let tracked_count = tracked_set.len();
        let canonical_count = canonical_set.len();

        // Check both count and content - if counts differ or sets are not equal, it's a reorg
        if tracked_count != canonical_count || tracked_set != canonical_set {
            ReorgDetectionResult::ReorgDetected { tracked_count, canonical_count }
        } else {
            ReorgDetectionResult::NoReorg
        }
    }
}

/// Defines explicit handling approaches for reconciling pending state with canonical state.
///
/// When a canonical block is received, the reconciliation strategy determines
/// how to handle the pending flashblock state based on the relationship between
/// the canonical chain and the pending blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconciliationStrategy {
    /// Canonical chain has caught up to or passed the pending state.
    ///
    /// This occurs when the canonical block number is >= the latest pending block number.
    /// The pending state should be cleared/reset as it's no longer ahead of canonical.
    CatchUp,

    /// A chain reorganization has been detected.
    ///
    /// This occurs when the transactions in the pending state for a given block
    /// don't match the transactions in the canonical block. The pending state
    /// should be rebuilt from canonical without reusing existing state.
    HandleReorg,

    /// The pending blocks have grown too far ahead of the canonical chain.
    ///
    /// This occurs when the depth (canonical_block - earliest_pending_block)
    /// exceeds the configured maximum depth. Contains the current depth and
    /// the configured maximum for diagnostic purposes.
    DepthLimitExceeded {
        /// The current depth of pending blocks.
        depth: u64,
        /// The configured maximum depth.
        max_depth: u64,
    },

    /// No issues detected, continue building on existing pending state.
    ///
    /// This occurs when the canonical block is behind the pending state,
    /// no reorg is detected, and depth limits are not exceeded.
    Continue,

    /// No pending state exists yet.
    ///
    /// This occurs when there is no pending flashblock state to reconcile.
    /// Typically happens at startup or after the pending state has been cleared.
    NoPendingState,
}

/// Reconciler for determining how to handle canonical block updates.
///
/// This struct encapsulates the logic for determining which [`ReconciliationStrategy`]
/// should be used when a new canonical block is received.
///
/// # Example
///
/// ```
/// use base_reth_flashblocks::validation::{CanonicalBlockReconciler, ReconciliationStrategy};
///
/// // Determine strategy when canonical catches up
/// let strategy = CanonicalBlockReconciler::reconcile(
///     Some(100),  // earliest pending block
///     Some(105),  // latest pending block
///     105,        // canonical block number (caught up)
///     10,         // max depth
///     false,      // no reorg detected
/// );
/// assert_eq!(strategy, ReconciliationStrategy::CatchUp);
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct CanonicalBlockReconciler;

impl CanonicalBlockReconciler {
    /// Determines the appropriate reconciliation strategy based on the current state.
    ///
    /// # Arguments
    ///
    /// * `pending_earliest_block` - The earliest block number in the pending state, if any.
    /// * `pending_latest_block` - The latest block number in the pending state, if any.
    /// * `canonical_block_number` - The block number of the new canonical block.
    /// * `max_depth` - The maximum allowed depth between canonical and earliest pending block.
    /// * `reorg_detected` - Whether a reorg was detected (transaction mismatch).
    ///
    /// # Returns
    ///
    /// The [`ReconciliationStrategy`] that should be used to handle this situation.
    ///
    /// # Strategy Selection Logic
    ///
    /// 1. If no pending state exists (`pending_earliest_block` or `pending_latest_block` is `None`),
    ///    returns [`ReconciliationStrategy::NoPendingState`].
    ///
    /// 2. If canonical has caught up or passed pending (`canonical_block_number >= pending_latest_block`),
    ///    returns [`ReconciliationStrategy::CatchUp`].
    ///
    /// 3. If a reorg is detected, returns [`ReconciliationStrategy::HandleReorg`].
    ///
    /// 4. If depth limit is exceeded (`canonical_block_number - pending_earliest_block > max_depth`),
    ///    returns [`ReconciliationStrategy::DepthLimitExceeded`].
    ///
    /// 5. Otherwise, returns [`ReconciliationStrategy::Continue`].
    pub const fn reconcile(
        pending_earliest_block: Option<u64>,
        pending_latest_block: Option<u64>,
        canonical_block_number: u64,
        max_depth: u64,
        reorg_detected: bool,
    ) -> ReconciliationStrategy {
        // Check if pending state exists
        let (earliest, latest) = match (pending_earliest_block, pending_latest_block) {
            (Some(e), Some(l)) => (e, l),
            _ => return ReconciliationStrategy::NoPendingState,
        };

        // Check if canonical has caught up or passed pending
        if latest <= canonical_block_number {
            return ReconciliationStrategy::CatchUp;
        }

        // Check for reorg
        if reorg_detected {
            return ReconciliationStrategy::HandleReorg;
        }

        // Check depth limit
        let depth = canonical_block_number.saturating_sub(earliest);
        if depth > max_depth {
            return ReconciliationStrategy::DepthLimitExceeded { depth, max_depth };
        }

        // No issues, continue building
        ReconciliationStrategy::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    // ==================== FlashblockSequenceValidator Tests ====================

    #[rstest]
    // NextInSequence: consecutive indices within the same block
    #[case(100, 2, 100, 3, SequenceValidationResult::NextInSequence)]
    #[case(100, 0, 100, 1, SequenceValidationResult::NextInSequence)]
    #[case(100, 999, 100, 1000, SequenceValidationResult::NextInSequence)]
    #[case(0, 0, 0, 1, SequenceValidationResult::NextInSequence)]
    #[case(100, u64::MAX - 1, 100, u64::MAX, SequenceValidationResult::NextInSequence)]
    // FirstOfNextBlock: index 0 of the next block
    #[case(0, 0, 1, 0, SequenceValidationResult::FirstOfNextBlock)]
    #[case(100, 5, 101, 0, SequenceValidationResult::FirstOfNextBlock)]
    #[case(100, 0, 101, 0, SequenceValidationResult::FirstOfNextBlock)]
    #[case(999999, 10, 1000000, 0, SequenceValidationResult::FirstOfNextBlock)]
    #[case(0, 5, 1, 0, SequenceValidationResult::FirstOfNextBlock)]
    #[case(u64::MAX - 1, 0, u64::MAX, 0, SequenceValidationResult::FirstOfNextBlock)]
    // Duplicate: same block and index
    #[case(100, 5, 100, 5, SequenceValidationResult::Duplicate)]
    #[case(100, 0, 100, 0, SequenceValidationResult::Duplicate)]
    // NonSequentialGap: non-consecutive indices within the same block
    #[case(100, 2, 100, 4, SequenceValidationResult::NonSequentialGap { expected: 3, actual: 4 })]
    #[case(100, 0, 100, 10, SequenceValidationResult::NonSequentialGap { expected: 1, actual: 10 })]
    #[case(100, 5, 100, 3, SequenceValidationResult::NonSequentialGap { expected: 6, actual: 3 })]
    // InvalidNewBlockIndex: new block with non-zero index or block gap
    #[case(100, 5, 101, 1, SequenceValidationResult::InvalidNewBlockIndex { block_number: 101, index: 1 })]
    #[case(100, 5, 105, 3, SequenceValidationResult::InvalidNewBlockIndex { block_number: 105, index: 3 })]
    #[case(100, 5, 102, 0, SequenceValidationResult::InvalidNewBlockIndex { block_number: 102, index: 0 })]
    #[case(100, 5, 99, 0, SequenceValidationResult::InvalidNewBlockIndex { block_number: 99, index: 0 })]
    #[case(100, 5, 99, 5, SequenceValidationResult::InvalidNewBlockIndex { block_number: 99, index: 5 })]
    fn test_sequence_validator(
        #[case] latest_block: u64,
        #[case] latest_idx: u64,
        #[case] incoming_block: u64,
        #[case] incoming_idx: u64,
        #[case] expected: SequenceValidationResult,
    ) {
        let result =
            FlashblockSequenceValidator::validate(latest_block, latest_idx, incoming_block, incoming_idx);
        assert_eq!(result, expected);
    }

    // ==================== ReorgDetector Tests ====================

    #[rstest]
    // No reorg cases
    #[case(&[], &[], ReorgDetectionResult::NoReorg)]
    #[case(&[0x01], &[0x01], ReorgDetectionResult::NoReorg)]
    #[case(&[0x01, 0x02, 0x03], &[0x01, 0x02, 0x03], ReorgDetectionResult::NoReorg)]
    #[case(&[0x01, 0x02, 0x03], &[0x03, 0x01, 0x02], ReorgDetectionResult::NoReorg)] // order doesn't matter
    #[case(&[0x01, 0x01, 0x02], &[0x01, 0x02], ReorgDetectionResult::NoReorg)] // duplicates deduplicated
    // Reorg cases - different counts
    #[case(&[0x01, 0x02, 0x03], &[0x01, 0x02], ReorgDetectionResult::ReorgDetected { tracked_count: 3, canonical_count: 2 })]
    #[case(&[0x01], &[0x01, 0x02, 0x03], ReorgDetectionResult::ReorgDetected { tracked_count: 1, canonical_count: 3 })]
    #[case(&[], &[0x01], ReorgDetectionResult::ReorgDetected { tracked_count: 0, canonical_count: 1 })]
    #[case(&[0x01], &[], ReorgDetectionResult::ReorgDetected { tracked_count: 1, canonical_count: 0 })]
    // Reorg cases - same count, different hashes
    #[case(&[0x01, 0x02], &[0x03, 0x04], ReorgDetectionResult::ReorgDetected { tracked_count: 2, canonical_count: 2 })]
    #[case(&[0x01, 0x02], &[0x01, 0x03], ReorgDetectionResult::ReorgDetected { tracked_count: 2, canonical_count: 2 })]
    #[case(&[0x42], &[0x43], ReorgDetectionResult::ReorgDetected { tracked_count: 1, canonical_count: 1 })]
    fn test_reorg_detector(
        #[case] tracked_bytes: &[u8],
        #[case] canonical_bytes: &[u8],
        #[case] expected: ReorgDetectionResult,
    ) {
        let tracked: Vec<B256> = tracked_bytes.iter().map(|b| B256::repeat_byte(*b)).collect();
        let canonical: Vec<B256> = canonical_bytes.iter().map(|b| B256::repeat_byte(*b)).collect();
        let result = ReorgDetector::detect(tracked.iter(), canonical.iter());
        assert_eq!(result, expected);
        assert_eq!(result.is_reorg(), matches!(expected, ReorgDetectionResult::ReorgDetected { .. }));
    }

    // ==================== CanonicalBlockReconciler Tests ====================

    #[rstest]
    // NoPendingState
    #[case(None, None, 100, 10, false, ReconciliationStrategy::NoPendingState)]
    #[case(Some(100), None, 100, 10, false, ReconciliationStrategy::NoPendingState)]
    #[case(None, Some(100), 100, 10, false, ReconciliationStrategy::NoPendingState)]
    // CatchUp: canonical >= latest pending
    #[case(Some(100), Some(105), 105, 10, false, ReconciliationStrategy::CatchUp)]
    #[case(Some(100), Some(105), 110, 10, false, ReconciliationStrategy::CatchUp)]
    #[case(Some(100), Some(100), 100, 10, false, ReconciliationStrategy::CatchUp)]
    #[case(Some(100), Some(105), 105, 10, true, ReconciliationStrategy::CatchUp)] // catchup > reorg priority
    // HandleReorg
    #[case(Some(100), Some(110), 102, 10, true, ReconciliationStrategy::HandleReorg)]
    #[case(Some(100), Some(130), 120, 10, true, ReconciliationStrategy::HandleReorg)] // reorg > depth priority
    // DepthLimitExceeded
    #[case(Some(100), Some(120), 115, 10, false, ReconciliationStrategy::DepthLimitExceeded { depth: 15, max_depth: 10 })]
    #[case(Some(100), Some(105), 101, 0, false, ReconciliationStrategy::DepthLimitExceeded { depth: 1, max_depth: 0 })]
    // Continue
    #[case(Some(100), Some(110), 105, 10, false, ReconciliationStrategy::Continue)]
    #[case(Some(100), Some(120), 110, 10, false, ReconciliationStrategy::Continue)] // depth exactly at limit
    #[case(Some(100), Some(105), 100, 10, false, ReconciliationStrategy::Continue)]
    #[case(Some(100), Some(105), 100, 0, false, ReconciliationStrategy::Continue)] // zero depth ok with max_depth=0
    #[case(Some(100), Some(100), 99, 10, false, ReconciliationStrategy::Continue)] // single pending block
    fn test_reconciler(
        #[case] earliest: Option<u64>,
        #[case] latest: Option<u64>,
        #[case] canonical: u64,
        #[case] max_depth: u64,
        #[case] reorg: bool,
        #[case] expected: ReconciliationStrategy,
    ) {
        let result = CanonicalBlockReconciler::reconcile(earliest, latest, canonical, max_depth, reorg);
        assert_eq!(result, expected);
    }
}
