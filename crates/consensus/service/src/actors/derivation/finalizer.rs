//! The [`L2Finalizer`] for the derivation actor.

use std::collections::BTreeMap;

use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};

/// An internal type alias for L1 block numbers.
type L1BlockNumber = u64;

/// An internal type alias for L2 block numbers.
type L2BlockNumber = u64;

/// The [`L2Finalizer`] is responsible for tracking L2 blocks derived from L1 blocks and
/// determining which L2 blocks can be finalized when L1 blocks are finalized.
///
/// It maintains a queue of derived L2 blocks that are awaiting finalization, and returns
/// the L2 block numbers that can be finalized as new finalized L1 blocks are received.
///
/// # Keys are L1 inclusion blocks, not L2 epochs
///
/// Entries are keyed by [`AttributesWithParent::derived_from`], the L1 block whose data (calldata
/// or blobs) carried the batch that produced the L2 block. That is the L1 **inclusion** block. It
/// is not the L2 block's [`L2BlockInfo::l1_origin`], the epoch the L2 block references: a batch
/// can be included in any L1 block at or after its epoch, so the inclusion block is always at or
/// after the epoch origin. Finalization has to key on the inclusion block, because only the
/// inclusion block proves that the L1 inputs were finalized; keying on the epoch would finalize L2
/// blocks up to several L1 blocks before their data was actually finalized.
///
/// [`L2BlockInfo::l1_origin`]: base_protocol::L2BlockInfo::l1_origin
#[derive(Debug, Default)]
pub struct L2Finalizer {
    /// A map of `L1 inclusion block number -> highest derived L2 block number` included in that
    /// block, used to track derived [`AttributesWithParent`] awaiting finalization. When a new
    /// finalized L1 block is received, the highest L2 block whose inputs are contained within the
    /// finalized L1 chain is finalized.
    awaiting_finalization: BTreeMap<L1BlockNumber, L2BlockNumber>,
    /// The highest finalized L1 block observed by the finalizer.
    ///
    /// This survives derivation resets so that re-derived L2 blocks can be finalized without
    /// waiting for the L1 watcher to emit the same finalized block again.
    finalized_l1_block: Option<BlockInfo>,
}

impl L2Finalizer {
    /// Enqueues a derived [`AttributesWithParent`] for finalization. When a new finalized L1
    /// block is observed that is `>=` the height of [`AttributesWithParent::derived_from`], the
    /// L2 block associated with the payload attributes will be finalized.
    pub fn enqueue_for_finalization(&mut self, attributes: &AttributesWithParent) {
        self.awaiting_finalization
            .entry(
                attributes.derived_from.map(|b| b.number).expect(
                    "Fatal: Cannot enqueue attributes for finalization that weren't derived",
                ),
            )
            .and_modify(|n| *n = (*n).max(attributes.block_number()))
            .or_insert_with(|| attributes.block_number());
    }

    /// Records a finalized L1 block and attempts to finalize any eligible L2 blocks.
    ///
    /// Finalized L1 signals are monotonic by block number. Older signals are ignored, while a
    /// signal at the same height is accepted so that its block identity remains current.
    pub fn process_finalized_l1_block(
        &mut self,
        finalized_l1_block: BlockInfo,
    ) -> Option<L2BlockNumber> {
        if self
            .finalized_l1_block
            .as_ref()
            .is_some_and(|previous| finalized_l1_block.number < previous.number)
        {
            return None;
        }

        self.finalized_l1_block = Some(finalized_l1_block);
        self.try_finalize_pending()
    }

    /// Attempts to finalize an eligible L2 block using the latest finalized L1 signal.
    pub fn try_finalize_pending(&mut self) -> Option<L2BlockNumber> {
        self.try_finalize_next(self.finalized_l1_block?)
    }

    /// Drops the entries that a derivation reset invalidated, keeping every entry that the reset
    /// still allows to be finalized.
    ///
    /// A reset does not roll back derivation's whole history. The pipeline restarts from
    /// `l2_safe_head`, so every L2 block at or below it stays safe and is never re-derived: if its
    /// entry is dropped, nothing recreates it, and the L2 chain can only be finalized again once
    /// new derivation happens to reach a *later* L1 inclusion block. Repeated resets therefore
    /// starve finalization whenever they arrive faster than L1 finality advances, so the queue is
    /// filtered instead of cleared.
    ///
    /// An entry survives only when both of these hold:
    ///
    /// - Its L2 block is at or below `l2_safe_head.block_info.number`. Entries above the safe head
    ///   belong to blocks the reset rolled back; they are re-derived and re-queued once the
    ///   pipeline confirms them again, and finalizing one now would finalize a reorged-out block.
    /// - Its L1 inclusion block is at or below `l2_safe_head.l1_origin.number`. The pipeline
    ///   resumes L1 traversal at that origin, so inclusion blocks above it are re-read from the
    ///   L1 provider and their L2 mapping may move to a different inclusion block; those entries
    ///   are dropped and recreated from canonical data. Inclusion blocks at or below the origin
    ///   are not revisited by the pipeline, so the recorded mapping stays the one derivation
    ///   produced for them.
    ///
    /// The two bounds are independent. Retaining on the inclusion bound alone would still allow a
    /// rolled-back block to be finalized, and retaining on the L2 bound alone would trust an
    /// inclusion block the reset pipeline is about to re-derive.
    ///
    /// # Canonicity of retained inclusion blocks
    ///
    /// A retained entry can only be finalized once its inclusion block number is at or below the
    /// finalized L1 block, and finalized L1 blocks are never reorged. An L1 reorg below the reset
    /// origin can therefore only move inclusion blocks that sit *above* finalized L1, where
    /// [`Self::try_finalize_next`] can never select them. That is why the entries need no separate
    /// canonicality check on their inclusion hash.
    pub fn retain_valid_after_reset(&mut self, l2_safe_head: L2BlockInfo) {
        let safe_l2_number = l2_safe_head.block_info.number;
        let reset_l1_origin = l2_safe_head.l1_origin.number;
        self.awaiting_finalization
            .retain(|&inclusion, &mut l2| l2 <= safe_l2_number && inclusion <= reset_l1_origin);
    }

    /// Attempts to find L2 blocks that can be finalized based on the new finalized L1 block.
    ///
    /// Returns `Some(l2_block_number)` if there is an L2 block that can be finalized,
    /// or `None` if no blocks are ready for finalization.
    pub fn try_finalize_next(
        &mut self,
        new_finalized_l1_block: BlockInfo,
    ) -> Option<L2BlockNumber> {
        // Find the highest safe L2 block that is contained within the finalized chain,
        // that the finalizer is aware of.
        let result =
            *self.awaiting_finalization.range(..=new_finalized_l1_block.number).next_back()?.1;
        // Drain the queue of all L1 blocks contained in the finalized L1 chain.
        self.awaiting_finalization.retain(|&number, _| number > new_finalized_l1_block.number);
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for [`L2Finalizer`] queue management.

    use alloy_eips::BlockNumHash;
    use base_common_rpc_types_engine::BasePayloadAttributes;
    use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};

    use super::L2Finalizer;

    /// Build a minimal [`AttributesWithParent`] whose derived L2 block number is
    /// `l2_parent_number + 1` and whose L1 inclusion block is `inclusion_number`.
    fn attrs(l2_parent_number: u64, inclusion_number: u64) -> AttributesWithParent {
        let parent = L2BlockInfo {
            block_info: BlockInfo { number: l2_parent_number, ..Default::default() },
            l1_origin: BlockNumHash::default(),
            seq_num: 0,
        };
        let derived_from = BlockInfo { number: inclusion_number, ..Default::default() };
        AttributesWithParent::new(
            BasePayloadAttributes::default(),
            parent,
            Some(derived_from),
            false,
        )
    }

    /// Build a [`BlockInfo`] representing a finalized L1 block at `number`.
    fn l1_at(number: u64) -> BlockInfo {
        BlockInfo { number, ..Default::default() }
    }

    /// Build the [`L2BlockInfo`] a derivation reset is signalled with: safe head `number` whose
    /// L1 epoch origin is `origin_number`.
    fn safe_head_at(number: u64, origin_number: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo { number, ..Default::default() },
            l1_origin: BlockNumHash { number: origin_number, ..Default::default() },
            seq_num: 0,
        }
    }

    #[test]
    fn empty_queue_returns_none() {
        let mut f = L2Finalizer::default();
        assert!(f.try_finalize_next(l1_at(100)).is_none());
    }

    #[test]
    fn finalized_l1_signal_is_retained_until_candidates_are_ready() {
        let mut f = L2Finalizer::default();
        assert!(f.process_finalized_l1_block(l1_at(5)).is_none());

        f.enqueue_for_finalization(&attrs(9, 5));

        assert_eq!(f.try_finalize_pending(), Some(10));
    }

    #[test]
    fn single_entry_l1_not_yet_finalized() {
        let mut f = L2Finalizer::default();
        // L2 block 10 was included in L1 block 5. Finalizing at L1=3 must not include it.
        f.enqueue_for_finalization(&attrs(9, 5)); // l2=10, inclusion=5
        assert!(f.try_finalize_next(l1_at(3)).is_none());
        // Entry must still be in the queue.
        assert!(f.try_finalize_next(l1_at(5)).is_some());
    }

    #[test]
    fn single_entry_l1_exactly_at_finalized() {
        // Boundary: the inclusion block equals finalized L1. The range is `..=`, so it matches.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(9, 5)); // l2=10, inclusion=5
        assert_eq!(f.try_finalize_next(l1_at(5)), Some(10));
    }

    #[test]
    fn multiple_l2_per_inclusion_block_keeps_highest() {
        // Three L2 blocks included in L1 block 1. Only the highest (3) should be returned.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(0, 1)); // l2=1, inclusion=1
        f.enqueue_for_finalization(&attrs(1, 1)); // l2=2, inclusion=1
        f.enqueue_for_finalization(&attrs(2, 1)); // l2=3, inclusion=1
        assert_eq!(f.try_finalize_next(l1_at(1)), Some(3));
    }

    #[test]
    fn partial_finalization_drains_lower_entries() {
        // Entries at L1=1,2,3. Finalizing at L1=2 returns highest across L1<=2 and drains those.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(4, 1)); // l2=5,  inclusion=1
        f.enqueue_for_finalization(&attrs(7, 2)); // l2=8,  inclusion=2
        f.enqueue_for_finalization(&attrs(10, 3)); // l2=11, inclusion=3

        assert_eq!(f.try_finalize_next(l1_at(2)), Some(8));
        // L1=3 entry must still be present.
        assert_eq!(f.try_finalize_next(l1_at(3)), Some(11));
    }

    #[test]
    fn reset_preserves_finalized_l1_signal() {
        let mut f = L2Finalizer::default();
        f.process_finalized_l1_block(l1_at(5));
        f.enqueue_for_finalization(&attrs(4, 1));

        f.retain_valid_after_reset(safe_head_at(100, 10));
        f.enqueue_for_finalization(&attrs(9, 5));

        assert_eq!(f.try_finalize_pending(), Some(10));
    }

    #[test]
    fn repeated_resets_do_not_starve_finalization() {
        // Every derived block is included roughly one finality window past the current finalized
        // L1 block, and the pipeline resets before each new block. Clearing the queue on reset
        // left only the newest entry, which was never old enough to finalize, so finalization
        // stopped entirely even though L1 finality kept advancing.
        let mut f = L2Finalizer::default();
        let finality_lag = 64;
        let mut finalized_l1_number = 100;
        let mut highest_finalized = 0;

        for step in 0..80u64 {
            let inclusion = finalized_l1_number + finality_lag;
            f.enqueue_for_finalization(&attrs(500 + step, inclusion));

            // Resets keep the safe head and resume L1 traversal from an origin above the
            // inclusion block of everything derived so far.
            f.retain_valid_after_reset(safe_head_at(501 + step, inclusion + 1));

            finalized_l1_number += 1;
            if let Some(l2) = f.process_finalized_l1_block(l1_at(finalized_l1_number)) {
                highest_finalized = highest_finalized.max(l2);
            }
        }

        // Finalized L1 reached 180, so the block included at 180 (derived at step 16) is final.
        assert_eq!(highest_finalized, 517);
    }

    #[test]
    fn reset_drops_candidates_above_the_new_safe_head() {
        // A reset that rolls the safe head back must not leave candidates above it behind.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(479, 105)); // l2=480, within the rolled-back chain
        f.enqueue_for_finalization(&attrs(599, 108)); // l2=600, reorged out

        f.retain_valid_after_reset(safe_head_at(500, 110));

        assert_eq!(f.try_finalize_next(l1_at(1_000)), Some(480));
    }

    #[test]
    fn reset_drops_candidates_included_above_the_reset_origin() {
        // The reset pipeline re-reads L1 from its origin, so inclusion blocks above the origin
        // may move and must be re-derived. The origin itself is the boundary and is kept.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(479, 100)); // inclusion == origin
        f.enqueue_for_finalization(&attrs(489, 101)); // inclusion above origin

        f.retain_valid_after_reset(safe_head_at(500, 100));

        assert_eq!(f.try_finalize_next(l1_at(1_000)), Some(480));
    }

    #[test]
    fn inclusion_block_bounds_the_epoch_origin() {
        // A batch may be included in any L1 block at or after its epoch origin, so the epoch
        // origin never proves that the L1 inputs are final on its own. Finalization follows the
        // inclusion block: an entry included above the finalized height is not eligible even
        // when its epoch is already final.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(9, 20)); // l2=10 included at L1 20

        assert!(f.try_finalize_next(l1_at(15)).is_none());
        assert_eq!(f.try_finalize_next(l1_at(20)), Some(10));
    }

    #[test]
    fn drain_preserves_future_entries() {
        // After finalizing up to L1=2, entries at L1=5 must survive.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(4, 2)); // l2=5,  inclusion=2
        f.enqueue_for_finalization(&attrs(9, 5)); // l2=10, inclusion=5

        assert_eq!(f.try_finalize_next(l1_at(2)), Some(5));
        // L1=5 entry is still present; finalizing it now returns l2=10.
        assert_eq!(f.try_finalize_next(l1_at(5)), Some(10));
    }

    #[test]
    fn old_finalized_signal_returns_none_after_drain() {
        // After draining all entries up to L1=5, a later signal for L1=2 finds nothing.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(19, 5)); // l2=20, inclusion=5

        assert_eq!(f.try_finalize_next(l1_at(5)), Some(20));
        // Queue is now empty; an older signal cannot regress to a stale entry.
        assert!(f.try_finalize_next(l1_at(2)).is_none());
    }

    #[test]
    fn older_finalized_signal_does_not_lower_high_water_mark() {
        let mut f = L2Finalizer::default();
        f.process_finalized_l1_block(l1_at(5));
        assert!(f.process_finalized_l1_block(l1_at(3)).is_none());

        f.enqueue_for_finalization(&attrs(9, 4));

        assert_eq!(f.try_finalize_pending(), Some(10));
    }
}
