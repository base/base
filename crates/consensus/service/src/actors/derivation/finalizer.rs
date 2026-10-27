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
    /// The safe head the engine last confirmed, paired with the L1 inclusion block derivation
    /// produced it from.
    ///
    /// Survives derivation resets so that a reset which keeps this safe head can also keep the
    /// candidates derived from the L1 blocks the safe head itself was built from. `None` before
    /// the first confirmation, and after a reset moved the safe head to a block this finalizer did
    /// not confirm.
    confirmed_safe_head: Option<(L2BlockInfo, BlockInfo)>,
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

    /// Records the safe head the engine confirmed, and the L1 inclusion block derivation produced
    /// it from, so that a later reset can tell which candidates are still trustworthy.
    ///
    /// `inclusion` is [`None`] for safe heads derivation did not produce, such as a safe head
    /// adopted during EL sync; the reset path then keeps no candidate, because none has a
    /// confirmed block behind it.
    pub fn confirm_safe_head(&mut self, safe_head: &L2BlockInfo, inclusion: Option<BlockInfo>) {
        self.confirmed_safe_head = inclusion.map(|inclusion| (*safe_head, inclusion));
    }

    /// Drops the entries that a derivation reset invalidated, keeping every entry that the reset
    /// still allows to be finalized.
    ///
    /// A reset does not roll back derivation's whole history. The pipeline restarts from
    /// `l2_safe_head`, and every L2 block at or below the last confirmed safe head stays safe and
    /// is never re-derived: if its entry is dropped, nothing recreates it, and the L2 chain can
    /// only be finalized again once new derivation happens to reach a *later* L1 inclusion block.
    /// Repeated resets therefore starve finalization whenever they arrive faster than L1 finality
    /// advances, so the queue is filtered instead of cleared.
    ///
    /// A candidate survives when both hold:
    ///
    /// - Its L2 block is at or below the last confirmed safe head, and at or below `l2_safe_head`.
    ///   The confirmed-safe-head bound is what keeps a candidate that was dispatched to the engine
    ///   but never confirmed from being finalized: a reset walks back from the unsafe head, so it
    ///   can move the safe head above the last confirmed block, and the block the engine holds at
    ///   that height need not be the one derivation produced. Above the reset safe head a block was
    ///   rolled back and is re-derived.
    /// - Its L1 inclusion block is at or below the L1 block the reset can still vouch for: the
    ///   inclusion block recorded for the confirmed safe head when the reset keeps that safe head,
    ///   otherwise the reset safe head's epoch origin. Inclusion blocks only grow as derivation
    ///   advances, so a candidate at or below the confirmed safe head always satisfies the bound
    ///   when the safe head is kept; the bound binds when the reset moved the safe head and the L1
    ///   range behind the engine's safe chain is no longer known.
    ///
    /// A reset with no recorded confirmation keeps nothing.
    ///
    /// # Canonicity of retained inclusion blocks
    ///
    /// Finalized L1 is never reorged, so a candidate is only selected once its recorded inclusion
    /// height is final. A reorg can still replace a recorded inclusion block before its height
    /// finalizes, so retention stops at the inclusion block of the confirmed safe head: the L1
    /// range the engine's safe chain was built from. A reorg below that range abandons the safe
    /// head itself, which derivation cannot roll back and the finalizer cannot repair; the block
    /// that gets finalized is still the one the engine holds at or below its safe head.
    pub fn retain_valid_after_reset(&mut self, l2_safe_head: &L2BlockInfo) {
        let Some((confirmed_safe_head, confirmed_inclusion)) = self.confirmed_safe_head else {
            self.awaiting_finalization.clear();
            return;
        };

        // Candidates above the last confirmed safe head were never confirmed, so their content
        // after the reset is not the content derivation sent.
        let safe_l2_number =
            confirmed_safe_head.block_info.number.min(l2_safe_head.block_info.number);

        let trusted_inclusion = if confirmed_safe_head.block_info.number
            == l2_safe_head.block_info.number
            && confirmed_safe_head.block_info.hash == l2_safe_head.block_info.hash
        {
            confirmed_inclusion.number
        } else {
            // The reset moved the safe head to a block this finalizer did not confirm, so its own
            // inputs are being rebuilt and only its epoch origin is trusted.
            self.confirmed_safe_head = None;
            l2_safe_head.l1_origin.number
        };

        self.awaiting_finalization
            .retain(|&inclusion, &mut l2| l2 <= safe_l2_number && inclusion <= trusted_inclusion);
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
    use alloy_primitives::B256;
    use base_common_rpc_types_engine::BasePayloadAttributes;
    use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};

    use super::L2Finalizer;

    /// Build a minimal [`AttributesWithParent`] whose derived L2 block number is
    /// `l2_parent_number + 1`, whose L1 epoch origin is `epoch_number`, and whose L1 inclusion
    /// block is `inclusion_number`.
    fn attrs_at(
        l2_parent_number: u64,
        epoch_number: u64,
        inclusion_number: u64,
    ) -> AttributesWithParent {
        let parent = L2BlockInfo {
            block_info: BlockInfo { number: l2_parent_number, ..Default::default() },
            l1_origin: BlockNumHash { number: epoch_number, ..Default::default() },
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

    /// Build attributes with a zero epoch origin.
    fn attrs(l2_parent_number: u64, inclusion_number: u64) -> AttributesWithParent {
        attrs_at(l2_parent_number, 0, inclusion_number)
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
        f.confirm_safe_head(&safe_head_at(100, 10), Some(l1_at(10)));

        f.retain_valid_after_reset(&safe_head_at(100, 10));
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

            // A batch lands a block or two after the epoch of the block it carries, so the safe
            // head's epoch origin trails the inclusion block of its own batch.
            let safe_head = safe_head_at(501 + step, inclusion - 2);
            f.confirm_safe_head(&safe_head, Some(l1_at(inclusion)));
            f.retain_valid_after_reset(&safe_head);

            finalized_l1_number += 1;
            if let Some(l2) = f.process_finalized_l1_block(l1_at(finalized_l1_number)) {
                highest_finalized = highest_finalized.max(l2);
            }
        }

        // Finalized L1 reached 180, so the block included at 180 (derived at step 16) is final.
        assert_eq!(highest_finalized, 517);
    }

    #[test]
    fn confirmed_inclusion_keeps_candidates_above_the_epoch_origin() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(495, 119)); // l2=496
        f.enqueue_for_finalization(&attrs(499, 120)); // l2=500 is the safe head itself

        // The safe head's epoch origin (118) trails the inclusion block of its own batch (120),
        // so an epoch-only bound would drop every candidate derived from the last L1 blocks.
        let safe_head = safe_head_at(500, 118);
        f.confirm_safe_head(&safe_head, Some(l1_at(120)));
        f.retain_valid_after_reset(&safe_head);

        assert_eq!(f.try_finalize_next(l1_at(1_000)), Some(500));
    }

    #[test]
    fn reset_without_a_confirmation_keeps_nothing() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(495, 119));
        f.enqueue_for_finalization(&attrs(499, 120));

        // No safe head was ever confirmed, so no candidate has a confirmed block behind it.
        f.retain_valid_after_reset(&safe_head_at(500, 118));

        assert!(f.try_finalize_next(l1_at(1_000)).is_none());
    }

    #[test]
    fn reset_drops_candidates_that_were_never_confirmed() {
        let mut f = L2Finalizer::default();
        f.confirm_safe_head(&safe_head_at(500, 110), Some(l1_at(112)));
        f.enqueue_for_finalization(&attrs(499, 110)); // l2=500, confirmed safe head
        f.enqueue_for_finalization(&attrs(500, 112)); // l2=501, dispatched but not confirmed

        // A reset walks back from the unsafe head, so it can move the safe head above the last
        // confirmed block; the block the engine holds there need not be the one derivation sent.
        f.retain_valid_after_reset(&safe_head_at(900, 300));

        assert_eq!(f.try_finalize_next(l1_at(1_000)), Some(500));
    }

    #[test]
    fn reset_at_the_same_height_with_a_different_hash_drops_the_confirmation() {
        let mut f = L2Finalizer::default();
        f.confirm_safe_head(&safe_head_at(500, 118), Some(l1_at(120)));
        f.enqueue_for_finalization(&attrs(499, 120)); // l2=500, included at the confirmation

        // Same height, different block: the confirmed safe head is not the one the reset kept.
        let mut other_head = safe_head_at(500, 118);
        other_head.block_info.hash = B256::with_last_byte(1);
        f.retain_valid_after_reset(&other_head);

        assert!(f.try_finalize_next(l1_at(1_000)).is_none());
    }

    #[test]
    fn reset_that_moves_the_safe_head_trusts_only_the_reset_origin() {
        let mut f = L2Finalizer::default();
        f.confirm_safe_head(&safe_head_at(600, 100), Some(l1_at(101)));
        f.enqueue_for_finalization(&attrs(479, 100)); // l2=480, included at the new origin
        f.enqueue_for_finalization(&attrs(489, 101)); // l2=490, included above it

        // The reset moved the safe head to a block this finalizer never confirmed, so the L1 range
        // behind the engine's safe chain is unknown and only the new epoch origin is trusted.
        f.retain_valid_after_reset(&safe_head_at(500, 100));

        assert_eq!(f.try_finalize_next(l1_at(1_000)), Some(480));
    }

    #[test]
    fn reset_drops_candidates_above_the_new_safe_head() {
        // A reset that rolls the safe head back must not leave candidates above it behind.
        let mut f = L2Finalizer::default();
        f.confirm_safe_head(&safe_head_at(600, 100), Some(l1_at(108)));
        f.enqueue_for_finalization(&attrs(479, 105)); // l2=480, within the rolled-back chain
        f.enqueue_for_finalization(&attrs(599, 108)); // l2=600, reorged out

        f.retain_valid_after_reset(&safe_head_at(500, 110));

        assert_eq!(f.try_finalize_next(l1_at(1_000)), Some(480));
    }

    #[test]
    fn reset_that_moves_the_safe_head_drops_candidates_included_above_its_origin() {
        // A reset that moves the safe head no longer knows which L1 range the engine's safe chain
        // was built from, so it keeps only candidates included at or below the reset origin. Such
        // a candidate is never re-derived - blocks at or below the confirmed safe head are not -
        // so dropping one costs finalization latency rather than safety.
        let mut f = L2Finalizer::default();
        f.confirm_safe_head(&safe_head_at(600, 100), Some(l1_at(101)));
        f.enqueue_for_finalization(&attrs(479, 100)); // inclusion == origin
        f.enqueue_for_finalization(&attrs(489, 101)); // inclusion above origin

        f.retain_valid_after_reset(&safe_head_at(500, 100));

        assert_eq!(f.try_finalize_next(l1_at(1_000)), Some(480));
    }

    #[test]
    fn inclusion_block_bounds_the_epoch_origin() {
        // A batch may be included in any L1 block at or after its epoch origin, so the epoch
        // origin never proves that the L1 inputs are final on its own. Finalization follows the
        // inclusion block: the entry is not eligible when finalized L1 has covered the block's
        // epoch but not the L1 block its batch was actually included in.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_at(9, 18, 20)); // l2=10, epoch 18, included at L1 20

        assert!(f.try_finalize_next(l1_at(19)).is_none());
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
