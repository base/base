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

    // ==================== FlashblockSequenceValidator Tests ====================

    /// Test the first flashblock ever (bootstrap case).
    /// When starting fresh, we expect index 0 to be valid for any block.
    #[test]
    fn test_first_flashblock_bootstrap() {
        // Simulating bootstrap: latest is block 0, index 0 (initial state)
        // Incoming is block 1, index 0 (first real flashblock)
        let result = FlashblockSequenceValidator::validate(0, 0, 1, 0);
        assert_eq!(result, SequenceValidationResult::FirstOfNextBlock);
    }

    /// Test normal sequential flashblocks within the same block.
    #[test]
    fn test_next_in_sequence() {
        // Block 100, index 2 -> Block 100, index 3
        let result = FlashblockSequenceValidator::validate(100, 2, 100, 3);
        assert_eq!(result, SequenceValidationResult::NextInSequence);

        // Block 100, index 0 -> Block 100, index 1
        let result = FlashblockSequenceValidator::validate(100, 0, 100, 1);
        assert_eq!(result, SequenceValidationResult::NextInSequence);

        // Large index values
        let result = FlashblockSequenceValidator::validate(100, 999, 100, 1000);
        assert_eq!(result, SequenceValidationResult::NextInSequence);
    }

    /// Test first flashblock of a new block.
    #[test]
    fn test_first_of_next_block() {
        // Block 100, index 5 -> Block 101, index 0
        let result = FlashblockSequenceValidator::validate(100, 5, 101, 0);
        assert_eq!(result, SequenceValidationResult::FirstOfNextBlock);

        // Block 100, index 0 -> Block 101, index 0
        let result = FlashblockSequenceValidator::validate(100, 0, 101, 0);
        assert_eq!(result, SequenceValidationResult::FirstOfNextBlock);

        // Large block numbers
        let result = FlashblockSequenceValidator::validate(999999, 10, 1000000, 0);
        assert_eq!(result, SequenceValidationResult::FirstOfNextBlock);
    }

    /// Test duplicate detection.
    #[test]
    fn test_duplicate() {
        // Same block and same index
        let result = FlashblockSequenceValidator::validate(100, 5, 100, 5);
        assert_eq!(result, SequenceValidationResult::Duplicate);

        // Duplicate at index 0
        let result = FlashblockSequenceValidator::validate(100, 0, 100, 0);
        assert_eq!(result, SequenceValidationResult::Duplicate);
    }

    /// Test gap detection within the same block.
    #[test]
    fn test_non_sequential_gap() {
        // Skipping an index: 2 -> 4 (expected 3)
        let result = FlashblockSequenceValidator::validate(100, 2, 100, 4);
        assert_eq!(result, SequenceValidationResult::NonSequentialGap { expected: 3, actual: 4 });

        // Large gap: 0 -> 10 (expected 1)
        let result = FlashblockSequenceValidator::validate(100, 0, 100, 10);
        assert_eq!(result, SequenceValidationResult::NonSequentialGap { expected: 1, actual: 10 });

        // Going backwards within same block: 5 -> 3 (expected 6)
        let result = FlashblockSequenceValidator::validate(100, 5, 100, 3);
        assert_eq!(result, SequenceValidationResult::NonSequentialGap { expected: 6, actual: 3 });
    }

    /// Test non-zero index on a new block.
    #[test]
    fn test_invalid_new_block_index() {
        // New block with non-zero index
        let result = FlashblockSequenceValidator::validate(100, 5, 101, 1);
        assert_eq!(
            result,
            SequenceValidationResult::InvalidNewBlockIndex { block_number: 101, index: 1 }
        );

        // Skipping blocks with non-zero index
        let result = FlashblockSequenceValidator::validate(100, 5, 105, 3);
        assert_eq!(
            result,
            SequenceValidationResult::InvalidNewBlockIndex { block_number: 105, index: 3 }
        );

        // Future block with index 0 is NOT first of next block (block gap)
        let result = FlashblockSequenceValidator::validate(100, 5, 102, 0);
        assert_eq!(
            result,
            SequenceValidationResult::InvalidNewBlockIndex { block_number: 102, index: 0 }
        );
    }

    /// Test edge case: block number going backwards.
    #[test]
    fn test_block_number_regression() {
        // Incoming block number is less than current
        let result = FlashblockSequenceValidator::validate(100, 5, 99, 0);
        assert_eq!(
            result,
            SequenceValidationResult::InvalidNewBlockIndex { block_number: 99, index: 0 }
        );

        let result = FlashblockSequenceValidator::validate(100, 5, 99, 5);
        assert_eq!(
            result,
            SequenceValidationResult::InvalidNewBlockIndex { block_number: 99, index: 5 }
        );
    }

    /// Test edge case: maximum u64 values.
    #[test]
    fn test_max_values() {
        // Near max block number
        let result = FlashblockSequenceValidator::validate(u64::MAX - 1, 0, u64::MAX, 0);
        assert_eq!(result, SequenceValidationResult::FirstOfNextBlock);

        // Near max index
        let result = FlashblockSequenceValidator::validate(100, u64::MAX - 1, 100, u64::MAX);
        assert_eq!(result, SequenceValidationResult::NextInSequence);
    }

    /// Test edge case: zero block number.
    #[test]
    fn test_zero_block_number() {
        // Block 0 to block 1
        let result = FlashblockSequenceValidator::validate(0, 5, 1, 0);
        assert_eq!(result, SequenceValidationResult::FirstOfNextBlock);

        // Sequential within block 0
        let result = FlashblockSequenceValidator::validate(0, 0, 0, 1);
        assert_eq!(result, SequenceValidationResult::NextInSequence);
    }

    /// Test that the validator is stateless and consistent.
    #[test]
    fn test_validator_is_stateless() {
        // Same inputs should always produce the same output
        for _ in 0..100 {
            let result = FlashblockSequenceValidator::validate(100, 5, 100, 6);
            assert_eq!(result, SequenceValidationResult::NextInSequence);
        }
    }

    /// Test comprehensive sequence of flashblocks.
    #[test]
    fn test_full_sequence() {
        // Simulate a full sequence of flashblocks across two blocks
        let test_cases = vec![
            // Block 100: index 0 -> 1 -> 2 -> 3
            ((100, 0, 100, 1), SequenceValidationResult::NextInSequence),
            ((100, 1, 100, 2), SequenceValidationResult::NextInSequence),
            ((100, 2, 100, 3), SequenceValidationResult::NextInSequence),
            // Block 100 -> Block 101 (first flashblock)
            ((100, 3, 101, 0), SequenceValidationResult::FirstOfNextBlock),
            // Block 101: index 0 -> 1
            ((101, 0, 101, 1), SequenceValidationResult::NextInSequence),
        ];

        for ((latest_block, latest_idx, incoming_block, incoming_idx), expected) in test_cases {
            let result = FlashblockSequenceValidator::validate(
                latest_block,
                latest_idx,
                incoming_block,
                incoming_idx,
            );
            assert_eq!(
                result, expected,
                "Failed for latest=({}, {}), incoming=({}, {})",
                latest_block, latest_idx, incoming_block, incoming_idx
            );
        }
    }

    // ==================== ReorgDetector Tests ====================

    #[test]
    fn test_reorg_identical_transaction_sets_no_reorg() {
        let hash1 = B256::repeat_byte(0x01);
        let hash2 = B256::repeat_byte(0x02);
        let hash3 = B256::repeat_byte(0x03);

        let tracked = vec![hash1, hash2, hash3];
        let canonical = vec![hash1, hash2, hash3];

        let result = ReorgDetector::detect(tracked.iter(), canonical.iter());

        assert_eq!(result, ReorgDetectionResult::NoReorg);
        assert!(result.is_no_reorg());
        assert!(!result.is_reorg());
    }

    #[test]
    fn test_reorg_identical_sets_different_order_no_reorg() {
        let hash1 = B256::repeat_byte(0x01);
        let hash2 = B256::repeat_byte(0x02);
        let hash3 = B256::repeat_byte(0x03);

        // Different order should still be considered equal (set comparison)
        let tracked = vec![hash1, hash2, hash3];
        let canonical = vec![hash3, hash1, hash2];

        let result = ReorgDetector::detect(tracked.iter(), canonical.iter());

        assert_eq!(result, ReorgDetectionResult::NoReorg);
        assert!(result.is_no_reorg());
    }

    #[test]
    fn test_reorg_different_counts_reorg_detected() {
        let hash1 = B256::repeat_byte(0x01);
        let hash2 = B256::repeat_byte(0x02);
        let hash3 = B256::repeat_byte(0x03);

        let tracked = vec![hash1, hash2, hash3];
        let canonical = vec![hash1, hash2];

        let result = ReorgDetector::detect(tracked.iter(), canonical.iter());

        assert_eq!(
            result,
            ReorgDetectionResult::ReorgDetected { tracked_count: 3, canonical_count: 2 }
        );
        assert!(result.is_reorg());
        assert!(!result.is_no_reorg());
    }

    #[test]
    fn test_reorg_canonical_has_more_transactions() {
        let hash1 = B256::repeat_byte(0x01);
        let hash2 = B256::repeat_byte(0x02);
        let hash3 = B256::repeat_byte(0x03);

        let tracked = vec![hash1];
        let canonical = vec![hash1, hash2, hash3];

        let result = ReorgDetector::detect(tracked.iter(), canonical.iter());

        assert_eq!(
            result,
            ReorgDetectionResult::ReorgDetected { tracked_count: 1, canonical_count: 3 }
        );
        assert!(result.is_reorg());
    }

    #[test]
    fn test_reorg_same_count_different_hashes() {
        let hash1 = B256::repeat_byte(0x01);
        let hash2 = B256::repeat_byte(0x02);
        let hash3 = B256::repeat_byte(0x03);
        let hash4 = B256::repeat_byte(0x04);

        let tracked = vec![hash1, hash2];
        let canonical = vec![hash3, hash4];

        let result = ReorgDetector::detect(tracked.iter(), canonical.iter());

        assert_eq!(
            result,
            ReorgDetectionResult::ReorgDetected { tracked_count: 2, canonical_count: 2 }
        );
        assert!(result.is_reorg());
    }

    #[test]
    fn test_reorg_partial_overlap_different_hashes() {
        let hash1 = B256::repeat_byte(0x01);
        let hash2 = B256::repeat_byte(0x02);
        let hash3 = B256::repeat_byte(0x03);

        // One hash in common, but different overall sets
        let tracked = vec![hash1, hash2];
        let canonical = vec![hash1, hash3];

        let result = ReorgDetector::detect(tracked.iter(), canonical.iter());

        assert_eq!(
            result,
            ReorgDetectionResult::ReorgDetected { tracked_count: 2, canonical_count: 2 }
        );
        assert!(result.is_reorg());
    }

    #[test]
    fn test_reorg_empty_sets_no_reorg() {
        let tracked: Vec<B256> = vec![];
        let canonical: Vec<B256> = vec![];

        let result = ReorgDetector::detect(tracked.iter(), canonical.iter());

        assert_eq!(result, ReorgDetectionResult::NoReorg);
        assert!(result.is_no_reorg());
    }

    #[test]
    fn test_reorg_empty_tracked_non_empty_canonical() {
        let hash1 = B256::repeat_byte(0x01);

        let tracked: Vec<B256> = vec![];
        let canonical = vec![hash1];

        let result = ReorgDetector::detect(tracked.iter(), canonical.iter());

        assert_eq!(
            result,
            ReorgDetectionResult::ReorgDetected { tracked_count: 0, canonical_count: 1 }
        );
        assert!(result.is_reorg());
    }

    #[test]
    fn test_reorg_non_empty_tracked_empty_canonical() {
        let hash1 = B256::repeat_byte(0x01);

        let tracked = vec![hash1];
        let canonical: Vec<B256> = vec![];

        let result = ReorgDetector::detect(tracked.iter(), canonical.iter());

        assert_eq!(
            result,
            ReorgDetectionResult::ReorgDetected { tracked_count: 1, canonical_count: 0 }
        );
        assert!(result.is_reorg());
    }

    #[test]
    fn test_reorg_single_transaction_match_no_reorg() {
        let hash = B256::repeat_byte(0x42);

        let tracked = vec![hash];
        let canonical = vec![hash];

        let result = ReorgDetector::detect(tracked.iter(), canonical.iter());

        assert_eq!(result, ReorgDetectionResult::NoReorg);
        assert!(result.is_no_reorg());
    }

    #[test]
    fn test_reorg_single_transaction_mismatch() {
        let hash1 = B256::repeat_byte(0x42);
        let hash2 = B256::repeat_byte(0x43);

        let tracked = vec![hash1];
        let canonical = vec![hash2];

        let result = ReorgDetector::detect(tracked.iter(), canonical.iter());

        assert_eq!(
            result,
            ReorgDetectionResult::ReorgDetected { tracked_count: 1, canonical_count: 1 }
        );
        assert!(result.is_reorg());
    }

    #[test]
    fn test_reorg_duplicate_hashes_are_deduplicated() {
        let hash1 = B256::repeat_byte(0x01);
        let hash2 = B256::repeat_byte(0x02);

        // Duplicates should be deduplicated by the HashSet
        let tracked = vec![hash1, hash1, hash2];
        let canonical = vec![hash1, hash2];

        let result = ReorgDetector::detect(tracked.iter(), canonical.iter());

        // After deduplication, both sets have 2 unique hashes
        assert_eq!(result, ReorgDetectionResult::NoReorg);
    }

    #[test]
    fn test_reorg_detection_result_debug_impl() {
        let result = ReorgDetectionResult::NoReorg;
        assert_eq!(format!("{:?}", result), "NoReorg");

        let result = ReorgDetectionResult::ReorgDetected { tracked_count: 5, canonical_count: 3 };
        assert!(format!("{:?}", result).contains("ReorgDetected"));
        assert!(format!("{:?}", result).contains("5"));
        assert!(format!("{:?}", result).contains("3"));
    }

    #[test]
    fn test_reorg_detector_is_copy() {
        let detector = ReorgDetector;
        let _copied = detector;
        let _also_copied = detector; // Should compile since ReorgDetector is Copy
    }

    #[test]
    fn test_reorg_detector_default() {
        let _detector = ReorgDetector::default();
    }

    // ==================== CanonicalBlockReconciler Tests ====================

    /// Test that canonical catching up to pending returns CatchUp strategy.
    #[test]
    fn test_reconciler_canonical_catches_up_to_pending() {
        // Canonical block equals latest pending block
        let strategy = CanonicalBlockReconciler::reconcile(
            Some(100), // earliest pending
            Some(105), // latest pending
            105,       // canonical (equal to latest pending)
            10,        // max depth
            false,     // no reorg
        );
        assert_eq!(strategy, ReconciliationStrategy::CatchUp);
    }

    /// Test that canonical passing pending returns CatchUp strategy.
    #[test]
    fn test_reconciler_canonical_passes_pending() {
        // Canonical block is ahead of latest pending block
        let strategy = CanonicalBlockReconciler::reconcile(
            Some(100), // earliest pending
            Some(105), // latest pending
            110,       // canonical (ahead of latest pending)
            10,        // max depth
            false,     // no reorg
        );
        assert_eq!(strategy, ReconciliationStrategy::CatchUp);
    }

    /// Test that reorg detection returns HandleReorg strategy.
    #[test]
    fn test_reconciler_reorg_detected() {
        // Canonical is behind pending but reorg detected
        let strategy = CanonicalBlockReconciler::reconcile(
            Some(100), // earliest pending
            Some(110), // latest pending
            102,       // canonical (behind pending)
            10,        // max depth
            true,      // reorg detected!
        );
        assert_eq!(strategy, ReconciliationStrategy::HandleReorg);
    }

    /// Test that exceeding depth limit returns DepthLimitExceeded strategy.
    #[test]
    fn test_reconciler_depth_limit_exceeded() {
        // Pending blocks are too far ahead of canonical
        let strategy = CanonicalBlockReconciler::reconcile(
            Some(100), // earliest pending
            Some(120), // latest pending
            115,       // canonical
            10,        // max depth (115 - 100 = 15 > 10)
            false,     // no reorg
        );
        assert_eq!(
            strategy,
            ReconciliationStrategy::DepthLimitExceeded { depth: 15, max_depth: 10 }
        );
    }

    /// Test that normal operation returns Continue strategy.
    #[test]
    fn test_reconciler_continue_no_issues() {
        // Everything is fine, continue building
        let strategy = CanonicalBlockReconciler::reconcile(
            Some(100), // earliest pending
            Some(110), // latest pending
            105,       // canonical (behind pending)
            10,        // max depth (105 - 100 = 5 <= 10)
            false,     // no reorg
        );
        assert_eq!(strategy, ReconciliationStrategy::Continue);
    }

    /// Test that missing pending state returns NoPendingState strategy.
    #[test]
    fn test_reconciler_no_pending_state() {
        // No pending state exists
        let strategy = CanonicalBlockReconciler::reconcile(None, None, 100, 10, false);
        assert_eq!(strategy, ReconciliationStrategy::NoPendingState);

        // Only earliest is Some
        let strategy = CanonicalBlockReconciler::reconcile(Some(100), None, 100, 10, false);
        assert_eq!(strategy, ReconciliationStrategy::NoPendingState);

        // Only latest is Some
        let strategy = CanonicalBlockReconciler::reconcile(None, Some(100), 100, 10, false);
        assert_eq!(strategy, ReconciliationStrategy::NoPendingState);
    }

    /// Test edge case: depth exactly at limit should continue.
    #[test]
    fn test_reconciler_depth_at_limit_continues() {
        // Depth exactly equals max_depth (not exceeded)
        let strategy = CanonicalBlockReconciler::reconcile(
            Some(100), // earliest pending
            Some(120), // latest pending
            110,       // canonical (110 - 100 = 10, exactly at limit)
            10,        // max depth
            false,     // no reorg
        );
        assert_eq!(strategy, ReconciliationStrategy::Continue);
    }

    /// Test that reorg takes priority over depth limit.
    #[test]
    fn test_reconciler_reorg_priority_over_depth() {
        // Both reorg and depth limit exceeded - reorg should take priority
        let strategy = CanonicalBlockReconciler::reconcile(
            Some(100), // earliest pending
            Some(130), // latest pending
            120,       // canonical (120 - 100 = 20 > 10)
            10,        // max depth
            true,      // reorg detected!
        );
        // Reorg is checked before depth limit
        assert_eq!(strategy, ReconciliationStrategy::HandleReorg);
    }

    /// Test that CatchUp takes priority over reorg.
    #[test]
    fn test_reconciler_catchup_priority_over_reorg() {
        // Canonical caught up and reorg detected - CatchUp should take priority
        let strategy = CanonicalBlockReconciler::reconcile(
            Some(100), // earliest pending
            Some(105), // latest pending
            105,       // canonical (caught up)
            10,        // max depth
            true,      // reorg detected (but doesn't matter, canonical caught up)
        );
        // CatchUp is checked before reorg
        assert_eq!(strategy, ReconciliationStrategy::CatchUp);
    }

    /// Test with zero depth.
    #[test]
    fn test_reconciler_zero_depth_canonical_at_earliest() {
        let strategy = CanonicalBlockReconciler::reconcile(
            Some(100), // earliest pending
            Some(105), // latest pending
            100,       // canonical at earliest
            10,        // max depth
            false,     // no reorg
        );
        // Canonical is behind latest, depth is 0, should continue
        assert_eq!(strategy, ReconciliationStrategy::Continue);
    }

    /// Test with max_depth of zero (strictest setting).
    #[test]
    fn test_reconciler_zero_max_depth() {
        // Any depth > 0 should exceed limit
        let strategy = CanonicalBlockReconciler::reconcile(
            Some(100), // earliest pending
            Some(105), // latest pending
            101,       // canonical (101 - 100 = 1 > 0)
            0,         // max depth of 0
            false,     // no reorg
        );
        assert_eq!(strategy, ReconciliationStrategy::DepthLimitExceeded { depth: 1, max_depth: 0 });

        // Depth of 0 should still work
        let strategy = CanonicalBlockReconciler::reconcile(
            Some(100), // earliest pending
            Some(105), // latest pending
            100,       // canonical (100 - 100 = 0, not exceeded)
            0,         // max depth of 0
            false,     // no reorg
        );
        assert_eq!(strategy, ReconciliationStrategy::Continue);
    }

    /// Test that earliest equals latest (single pending block).
    #[test]
    fn test_reconciler_single_pending_block() {
        // Single pending block, canonical behind
        let strategy = CanonicalBlockReconciler::reconcile(
            Some(100), // earliest = latest
            Some(100), // single pending block
            99,        // canonical behind
            10,        // max depth
            false,     // no reorg
        );
        assert_eq!(strategy, ReconciliationStrategy::Continue);

        // Single pending block, canonical caught up
        let strategy = CanonicalBlockReconciler::reconcile(
            Some(100), // earliest = latest
            Some(100), // single pending block
            100,       // canonical caught up
            10,        // max depth
            false,     // no reorg
        );
        assert_eq!(strategy, ReconciliationStrategy::CatchUp);
    }
}
