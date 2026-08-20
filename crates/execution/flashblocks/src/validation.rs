//! Flashblock sequence validation and reorganization detection.
//!
//! Provides stateless validation logic for flashblock sequencing and chain reorg detection.

use alloy_eips::merge::EPOCH_SLOTS;
use alloy_primitives::B256;
use base_common_flashblocks::FlashblockId;

/// Result of validating a flashblock's position in the sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SequenceValidationResult {
    /// Next consecutive flashblock within the current block (same block, index + 1).
    NextInSequence,
    /// First flashblock (index 0) of the next block (block + 1).
    FirstOfNextBlock,
    /// Duplicate flashblock (same block and index) - should be ignored.
    Duplicate,
    /// Non-sequential index within the same block - indicates missed flashblocks.
    NonSequentialGap {
        /// Expected flashblock index.
        expected: u64,
        /// Actual incoming flashblock index.
        actual: u64,
    },
    /// New block received with non-zero index - missed the base flashblock.
    InvalidNewBlockIndex {
        /// Block number of the incoming flashblock.
        block_number: u64,
        /// The invalid (non-zero) index received.
        index: u64,
    },
    /// Incoming flashblock does not link back to the currently tracked latest flashblock.
    NonSequentialPredecessor {
        /// Expected predecessor flashblock id.
        expected: FlashblockId,
        /// Actual predecessor flashblock id reported by the incoming flashblock.
        actual: FlashblockId,
    },
}

/// Stateless validator for flashblock sequence ordering.
#[derive(Debug, Clone, Copy, Default)]
pub struct FlashblockSequenceValidator;

impl FlashblockSequenceValidator {
    /// Validates whether an incoming flashblock links to the current latest flashblock.
    pub fn validate(
        latest_block_number: u64,
        latest_flashblock_index: u64,
        incoming_block_number: u64,
        incoming_index: u64,
        incoming_prev_flashblock_id: FlashblockId,
    ) -> SequenceValidationResult {
        let latest_flashblock_id =
            FlashblockId { block_number: latest_block_number, index: latest_flashblock_index };

        if incoming_block_number == latest_block_number && incoming_index == latest_flashblock_index
        {
            return SequenceValidationResult::Duplicate;
        }

        // We can remove this `incoming_prev_flashblock_id != FlashblockId::default()` check later
        // but it is currently necessary as client nodes may be updated before the builder is
        // and they need to be able to handle the lack of `prev_flashblock_id` in the stream
        if incoming_prev_flashblock_id != FlashblockId::default()
            && incoming_prev_flashblock_id != latest_flashblock_id
        {
            return SequenceValidationResult::NonSequentialPredecessor {
                expected: latest_flashblock_id,
                actual: incoming_prev_flashblock_id,
            };
        }

        let next_flashblock_index = latest_flashblock_index.saturating_add(1);
        if incoming_block_number == latest_block_number && incoming_index == next_flashblock_index {
            return SequenceValidationResult::NextInSequence;
        }

        if incoming_block_number == latest_block_number + 1 && incoming_index == 0 {
            return SequenceValidationResult::FirstOfNextBlock;
        }

        if incoming_block_number == latest_block_number {
            return SequenceValidationResult::NonSequentialGap {
                expected: next_flashblock_index,
                actual: incoming_index,
            };
        }

        SequenceValidationResult::InvalidNewBlockIndex {
            block_number: incoming_block_number,
            index: incoming_index,
        }
    }
}

/// Result of a reorganization detection check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReorgDetectionResult {
    /// Transaction sets match exactly.
    NoReorg,
    /// Transaction sets differ (counts included for diagnostics).
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

/// Detects chain reorganizations by comparing transaction hash sets.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReorgDetector;

impl ReorgDetector {
    /// Compares tracked vs canonical transaction hashes to detect reorgs.
    ///
    /// Returns `ReorgDetected` if counts differ, hashes differ, or order differs.
    pub fn detect(
        tracked_tx_hashes: &[B256],
        canonical_tx_hashes: &[B256],
    ) -> ReorgDetectionResult {
        if tracked_tx_hashes != canonical_tx_hashes {
            ReorgDetectionResult::ReorgDetected {
                tracked_count: tracked_tx_hashes.len(),
                canonical_count: canonical_tx_hashes.len(),
            }
        } else {
            ReorgDetectionResult::NoReorg
        }
    }
}

/// Strategy for reconciling pending state with canonical state on new canonical blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconciliationStrategy {
    /// Canonical caught up or passed pending (canonical >= latest pending). Clear pending state.
    CatchUp,
    /// Reorg detected (tx mismatch). Rebuild pending from canonical.
    HandleReorg,
    /// Pending too far ahead of canonical.
    DepthLimitExceeded {
        /// Current depth of pending blocks.
        depth: u64,
        /// Configured maximum depth.
        max_depth: u64,
    },
    /// No issues - continue building on pending state.
    Continue,
    /// No pending state exists (startup or after clear).
    NoPendingState,
}

/// Determines reconciliation strategy for canonical block updates.
#[derive(Debug, Clone, Copy, Default)]
pub struct CanonicalBlockReconciler;

impl CanonicalBlockReconciler {
    /// Returns the appropriate [`ReconciliationStrategy`] based on pending vs canonical state.
    ///
    /// Priority: `NoPendingState` → `CatchUp` → `HandleReorg` → `DepthLimitExceeded` → `Continue`
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

/// Why a pending flashblock window is too stale to overlay for metering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingWindowStaleReason {
    /// Pending latest is at or behind local canonical tip ([`ReconciliationStrategy::CatchUp`]).
    CatchUp,
    /// Local tip is more than `max_depth` past pending earliest.
    DepthLimitExceeded {
        /// `tip.saturating_sub(earliest)`.
        depth: u64,
        /// Configured maximum pending-blocks depth.
        max_depth: u64,
    },
    /// Pending parent is more than [`EPOCH_SLOTS`] behind local tip.
    HistoricalDistance {
        /// `tip.saturating_sub(parent)` where `parent = earliest.saturating_sub(1)`.
        distance: u64,
    },
}

/// Returns whether a pending flashblock window is fresh enough to overlay for metering.
///
/// `parent < tip` alone is not stale: continue-after-canonical is expected while
/// `latest > tip` and depth is within `max_depth`.
pub const fn pending_window_is_fresh(earliest: u64, latest: u64, tip: u64, max_depth: u64) -> bool {
    pending_window_stale_reason(earliest, latest, tip, max_depth).is_none()
}

/// Returns why a pending flashblock window should be ignored for metering, if at all.
///
/// Reuses [`CanonicalBlockReconciler::reconcile`] with `reorg_detected: false`, then
/// applies the [`EPOCH_SLOTS`] historical-provider distance backstop against the pending
/// parent (`earliest - 1`).
pub const fn pending_window_stale_reason(
    earliest: u64,
    latest: u64,
    tip: u64,
    max_depth: u64,
) -> Option<PendingWindowStaleReason> {
    match CanonicalBlockReconciler::reconcile(Some(earliest), Some(latest), tip, max_depth, false) {
        ReconciliationStrategy::Continue => {
            let parent = earliest.saturating_sub(1);
            let distance = tip.saturating_sub(parent);
            if distance > EPOCH_SLOTS {
                Some(PendingWindowStaleReason::HistoricalDistance { distance })
            } else {
                None
            }
        }
        ReconciliationStrategy::CatchUp => Some(PendingWindowStaleReason::CatchUp),
        ReconciliationStrategy::DepthLimitExceeded { depth, max_depth } => {
            Some(PendingWindowStaleReason::DepthLimitExceeded { depth, max_depth })
        }
        // Unreachable with `Some`/`Some` and `reorg_detected: false`. Ignore pending if it
        // happens so metering never overlays a window that failed reconciliation.
        ReconciliationStrategy::HandleReorg | ReconciliationStrategy::NoPendingState => {
            Some(PendingWindowStaleReason::CatchUp)
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    // ==================== FlashblockSequenceValidator Tests ====================

    #[rstest]
    #[case(100, 5, 100, 6, FlashblockId { block_number: 100, index: 5 }, SequenceValidationResult::NextInSequence)]
    #[case(100, 5, 100, 6, FlashblockId::default(), SequenceValidationResult::NextInSequence)]
    #[case(100, 5, 101, 0, FlashblockId { block_number: 100, index: 5 }, SequenceValidationResult::FirstOfNextBlock)]
    #[case(100, 5, 101, 0, FlashblockId::default(), SequenceValidationResult::FirstOfNextBlock)]
    #[case(100, 5, 100, 5, FlashblockId::default(), SequenceValidationResult::Duplicate)]
    #[case(
        100,
        5,
        100,
        7,
        FlashblockId { block_number: 100, index: 5 },
        SequenceValidationResult::NonSequentialGap { expected: 6, actual: 7 }
    )]
    #[case(
        100,
        5,
        101,
        3,
        FlashblockId { block_number: 100, index: 5 },
        SequenceValidationResult::InvalidNewBlockIndex { block_number: 101, index: 3 }
    )]
    #[case(
        100,
        5,
        105,
        0,
        FlashblockId { block_number: 100, index: 5 },
        SequenceValidationResult::InvalidNewBlockIndex { block_number: 105, index: 0 }
    )]
    #[case(
        100,
        5,
        101,
        0,
        FlashblockId { block_number: 100, index: 4 },
        SequenceValidationResult::NonSequentialPredecessor {
            expected: FlashblockId { block_number: 100, index: 5 },
            actual: FlashblockId { block_number: 100, index: 4 },
        }
    )]
    #[case(
        100,
        5,
        99,
        0,
        FlashblockId { block_number: 100, index: 5 },
        SequenceValidationResult::InvalidNewBlockIndex { block_number: 99, index: 0 }
    )]
    fn test_sequence_validator(
        #[case] latest_block: u64,
        #[case] latest_idx: u64,
        #[case] incoming_block: u64,
        #[case] incoming_idx: u64,
        #[case] incoming_prev_flashblock_id: FlashblockId,
        #[case] expected: SequenceValidationResult,
    ) {
        let result = FlashblockSequenceValidator::validate(
            latest_block,
            latest_idx,
            incoming_block,
            incoming_idx,
            incoming_prev_flashblock_id,
        );
        assert_eq!(result, expected);
    }

    // ==================== ReorgDetector Tests ====================

    #[rstest]
    // No reorg cases - identical sequences
    #[case(&[], &[], ReorgDetectionResult::NoReorg)]
    #[case(&[0x01], &[0x01], ReorgDetectionResult::NoReorg)]
    #[case(&[0x01, 0x02, 0x03], &[0x01, 0x02, 0x03], ReorgDetectionResult::NoReorg)]
    #[case(&[0x01, 0x01, 0x02], &[0x01, 0x01, 0x02], ReorgDetectionResult::NoReorg)]
    // Reorg cases - different order (order matters!)
    #[case(&[0x01, 0x02, 0x03], &[0x03, 0x01, 0x02], ReorgDetectionResult::ReorgDetected { tracked_count: 3, canonical_count: 3 })]
    #[case(&[0x01, 0x02], &[0x02, 0x01], ReorgDetectionResult::ReorgDetected { tracked_count: 2, canonical_count: 2 })]
    // Reorg cases - different counts
    #[case(&[0x01, 0x02, 0x03], &[0x01, 0x02], ReorgDetectionResult::ReorgDetected { tracked_count: 3, canonical_count: 2 })]
    #[case(&[0x01], &[0x01, 0x02, 0x03], ReorgDetectionResult::ReorgDetected { tracked_count: 1, canonical_count: 3 })]
    #[case(&[], &[0x01], ReorgDetectionResult::ReorgDetected { tracked_count: 0, canonical_count: 1 })]
    #[case(&[0x01], &[], ReorgDetectionResult::ReorgDetected { tracked_count: 1, canonical_count: 0 })]
    #[case(&[0x01, 0x01, 0x02], &[0x01, 0x02], ReorgDetectionResult::ReorgDetected { tracked_count: 3, canonical_count: 2 })]
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
        let result = ReorgDetector::detect(&tracked, &canonical);
        assert_eq!(result, expected);
        assert_eq!(
            result.is_reorg(),
            matches!(expected, ReorgDetectionResult::ReorgDetected { .. })
        );
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
        let result =
            CanonicalBlockReconciler::reconcile(earliest, latest, canonical, max_depth, reorg);
        assert_eq!(result, expected);
    }

    // ==================== Pending window freshness (metering) ====================

    #[rstest]
    // Keep: parent == tip - 1 (continue after one canonical)
    #[case(101, 103, 102, 3, None)]
    // Keep: parent == tip (fresh window on tip)
    #[case(101, 103, 100, 3, None)]
    // Keep: depth exactly at max (`>` not `>=`)
    #[case(100, 110, 103, 3, None)]
    // Keep: parent < tip is not sufficient to ignore
    #[case(101, 104, 102, 3, None)]
    // Keep: EPOCH_SLOTS distance exactly at the limit
    #[case(100, 200, 131, 40, None)]
    // Ignore: latest == tip (CatchUp)
    #[case(100, 105, 105, 3, Some(PendingWindowStaleReason::CatchUp))]
    // Ignore: latest < tip (CatchUp)
    #[case(100, 105, 110, 3, Some(PendingWindowStaleReason::CatchUp))]
    // Ignore: frozen hours-old / thousands behind
    #[case(100, 100, 5000, 3, Some(PendingWindowStaleReason::CatchUp))]
    // Ignore: depth exceeded while still ahead of tip
    #[case(
        100,
        120,
        104,
        3,
        Some(PendingWindowStaleReason::DepthLimitExceeded { depth: 4, max_depth: 3 })
    )]
    // Ignore: EPOCH_SLOTS even when max_depth would Keep
    #[case(100, 200, 132, 40, Some(PendingWindowStaleReason::HistoricalDistance { distance: 33 }))]
    fn test_pending_window_freshness(
        #[case] earliest: u64,
        #[case] latest: u64,
        #[case] tip: u64,
        #[case] max_depth: u64,
        #[case] expected: Option<PendingWindowStaleReason>,
    ) {
        assert_eq!(pending_window_stale_reason(earliest, latest, tip, max_depth), expected);
        assert_eq!(pending_window_is_fresh(earliest, latest, tip, max_depth), expected.is_none());
    }
}
