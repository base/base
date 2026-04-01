//! The [`L2Finalizer`] for the derivation actor.

use std::collections::BTreeMap;

use base_protocol::{BlockInfo, OpAttributesWithParent};

/// An internal type alias for L1 block numbers.
type L1BlockNumber = u64;

/// An internal type alias for L2 block numbers.
type L2BlockNumber = u64;

/// The [`L2Finalizer`] is responsible for tracking L2 blocks derived from L1 blocks and
/// determining which L2 blocks can be finalized when L1 blocks are finalized.
///
/// It maintains a queue of derived L2 blocks that are awaiting finalization, and returns
/// the L2 block numbers that can be finalized as new finalized L1 blocks are received.
#[derive(Debug, Default)]
pub struct L2Finalizer {
    /// A map of `L1 block number -> highest derived L2 block number` within the L1 epoch, used to
    /// track derived [`OpAttributesWithParent`] awaiting finalization. When a new finalized L1
    /// block is received, the highest L2 block whose inputs are contained within the finalized
    /// L1 chain is finalized.
    awaiting_finalization: BTreeMap<L1BlockNumber, L2BlockNumber>,
}

impl L2Finalizer {
    /// Enqueues a derived [`OpAttributesWithParent`] for finalization. When a new finalized L1
    /// block is observed that is `>=` the height of [`OpAttributesWithParent::derived_from`], the
    /// L2 block associated with the payload attributes will be finalized.
    pub(crate) fn enqueue_for_finalization(&mut self, attributes: &OpAttributesWithParent) {
        self.awaiting_finalization
            .entry(
                attributes.derived_from.map(|b| b.number).expect(
                    "Fatal: Cannot enqueue attributes for finalization that weren't derived",
                ),
            )
            .and_modify(|n| *n = (*n).max(attributes.block_number()))
            .or_insert_with(|| attributes.block_number());
    }

    /// Clears the finalization queue.
    pub(crate) fn clear(&mut self) {
        self.awaiting_finalization.clear();
    }

    /// Attempts to find L2 blocks that can be finalized based on the new finalized L1 block.
    ///
    /// Returns `Some(l2_block_number)` if there is an L2 block that can be finalized,
    /// or `None` if no blocks are ready for finalization.
    pub(crate) fn try_finalize_next(
        &mut self,
        new_finalized_l1_block: BlockInfo,
    ) -> Option<L2BlockNumber> {
        // Find the highest safe L2 block that is contained within the finalized chain,
        // that the finalizer is aware of.
        let highest_safe =
            self.awaiting_finalization.range(..=new_finalized_l1_block.number).next_back();

        // If the highest safe block is found, return it and drain the
        // queue of all L1 blocks not contained in the finalized L1 chain.
        if let Some((_, highest_safe_number)) = highest_safe {
            let result = *highest_safe_number;
            self.awaiting_finalization.retain(|&number, _| number > new_finalized_l1_block.number);
            Some(result)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for [`L2Finalizer`] queue management.

    use alloy_eips::BlockNumHash;
    use base_alloy_rpc_types_engine::OpPayloadAttributes;
    use base_protocol::{BlockInfo, L2BlockInfo, OpAttributesWithParent};

    use super::L2Finalizer;

    /// Build a minimal [`OpAttributesWithParent`] whose derived L2 block number is
    /// `l2_parent_number + 1` and whose L1 origin is `l1_origin_number`.
    fn attrs(l2_parent_number: u64, l1_origin_number: u64) -> OpAttributesWithParent {
        let parent = L2BlockInfo {
            block_info: BlockInfo { number: l2_parent_number, ..Default::default() },
            l1_origin: BlockNumHash::default(),
            seq_num: 0,
        };
        let derived_from = BlockInfo { number: l1_origin_number, ..Default::default() };
        OpAttributesWithParent::new(
            OpPayloadAttributes::default(),
            parent,
            Some(derived_from),
            false,
        )
    }

    /// Build a [`BlockInfo`] representing a finalized L1 block at `number`.
    fn l1_at(number: u64) -> BlockInfo {
        BlockInfo { number, ..Default::default() }
    }

    #[test]
    fn empty_queue_returns_none() {
        let mut f = L2Finalizer::default();
        assert!(f.try_finalize_next(l1_at(100)).is_none());
    }

    #[test]
    fn single_entry_l1_not_yet_finalized() {
        let mut f = L2Finalizer::default();
        // L2 block 10 came from L1 origin 5. Finalizing at L1=3 should not include it.
        f.enqueue_for_finalization(&attrs(9, 5)); // l2=10, l1_origin=5
        assert!(f.try_finalize_next(l1_at(3)).is_none());
        // Entry must still be in the queue.
        assert!(f.try_finalize_next(l1_at(5)).is_some());
    }

    #[test]
    fn single_entry_l1_exactly_at_finalized() {
        // Boundary: l1_origin == finalized_l1. The range is `..=`, so this must match.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(9, 5)); // l2=10, l1_origin=5
        assert_eq!(f.try_finalize_next(l1_at(5)), Some(10));
    }

    #[test]
    fn multiple_l2_per_epoch_keeps_highest() {
        // Three L2 blocks all derived from L1 epoch 1. Only the highest (3) should be returned.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(0, 1)); // l2=1, l1_origin=1
        f.enqueue_for_finalization(&attrs(1, 1)); // l2=2, l1_origin=1
        f.enqueue_for_finalization(&attrs(2, 1)); // l2=3, l1_origin=1
        assert_eq!(f.try_finalize_next(l1_at(1)), Some(3));
    }

    #[test]
    fn partial_finalization_drains_lower_entries() {
        // Entries at L1=1,2,3. Finalizing at L1=2 returns highest across L1<=2 and drains those.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(4, 1)); // l2=5,  l1_origin=1
        f.enqueue_for_finalization(&attrs(7, 2)); // l2=8,  l1_origin=2
        f.enqueue_for_finalization(&attrs(10, 3)); // l2=11, l1_origin=3

        assert_eq!(f.try_finalize_next(l1_at(2)), Some(8));
        // L1=3 entry must still be present.
        assert_eq!(f.try_finalize_next(l1_at(3)), Some(11));
    }

    #[test]
    fn clear_empties_queue() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(4, 1));
        f.enqueue_for_finalization(&attrs(7, 2));
        f.clear();
        assert!(f.try_finalize_next(l1_at(100)).is_none());
    }

    #[test]
    fn drain_preserves_future_entries() {
        // After finalizing up to L1=2, entries at L1=5 must survive.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(4, 2)); // l2=5,  l1_origin=2
        f.enqueue_for_finalization(&attrs(9, 5)); // l2=10, l1_origin=5

        assert_eq!(f.try_finalize_next(l1_at(2)), Some(5));
        // L1=5 entry is still present; finalizing it now returns l2=10.
        assert_eq!(f.try_finalize_next(l1_at(5)), Some(10));
    }

    #[test]
    fn old_finalized_signal_returns_none_after_drain() {
        // After draining all entries up to L1=5, a later signal for L1=2 finds nothing.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(19, 5)); // l2=20, l1_origin=5

        assert_eq!(f.try_finalize_next(l1_at(5)), Some(20));
        // Queue is now empty; an older signal cannot regress to a stale entry.
        assert!(f.try_finalize_next(l1_at(2)).is_none());
    }
}
