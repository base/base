//! The [`L2Finalizer`] for the derivation actor.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::B256;
use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};

/// An internal type alias for L1 block numbers.
type L1BlockNumber = u64;

/// An internal type alias for L2 block numbers.
type L2BlockNumber = u64;

/// The L2 blocks that can be finalized now, together with every L1 inclusion block that has to be
/// canonical for that to be safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFinalization {
    /// The highest L2 block that can be finalized.
    pub l2_block: L2BlockNumber,
    /// The distinct L1 inclusion blocks recorded for the L2 blocks this finalization covers.
    ///
    /// Finalizing `l2_block` finalizes every L2 block below it, so each of these has to be
    /// canonical, not just the highest. [`L2Finalizer::try_finalize_next`] refuses to finalize
    /// until every one of them was marked verified.
    pub inclusions: Vec<BlockInfo>,
}

/// The [`L2Finalizer`] is responsible for tracking L2 blocks derived from L1 blocks and
/// determining which L2 blocks can be finalized when L1 blocks are finalized.
///
/// # Safety contract
///
/// An L2 block is finalized only when every L2 block the finalizer has a record for at or below
/// it was derived from an L1 inclusion block that is **final** (at or below the finalized L1
/// height) and **canonical** (verified by the actor against the L1 chain, by block hash).
/// [`Self::try_finalize_next`] enforces this itself: it returns nothing while any inclusion in the
/// finalized prefix is unverified.
///
/// Known evidence is never turned into permission. A record is removed only when
///
/// - the L2 block it describes was finalized, or
/// - the L2 block is above the safe head a reset restarted from, so derivation produces it
///   again.
///
/// A rejected inclusion block ([`Self::reject_inclusion`]) and a block a reset cannot vouch for
/// (see [`Self::retain_valid_after_reset`]) both leave the L2 block on a barrier: nothing at or
/// above the lowest barrier block finalizes. A barrier is removed only when the block is derived
/// again or a reset restarts from a safe head below it, so it can hold finalization back
/// indefinitely if neither happens.
///
/// # Keys are L1 inclusion blocks, not L2 epochs
///
/// Records hold [`AttributesWithParent::derived_from`], the L1 block whose data (calldata or
/// blobs) carried the batch that produced the L2 block. That is the L1 **inclusion** block. It is
/// not the L2 block's [`L2BlockInfo::l1_origin`], the epoch the L2 block references: a batch can
/// be included in any L1 block at or after its epoch, so the inclusion block is always at or after
/// the epoch origin. Finalization has to follow the inclusion block, because only it proves that
/// the L1 inputs were finalized; following the epoch would finalize L2 blocks up to several L1
/// blocks before their data was actually final.
///
/// [`L2BlockInfo::l1_origin`]: base_protocol::L2BlockInfo::l1_origin
///
/// # Inclusion order does not follow L2 order
///
/// A reset replay can derive a higher L2 block from a lower L1 block than a retained lower block
/// (L2 500 from L1 120, then L2 501 from L1 119). Every L2 block therefore keeps its own record,
/// and an L2 block waits for the inclusion block of every block below it.
///
/// # One verification per inclusion block
///
/// A finalized L1 height cannot be replaced, so one verification of an inclusion identity covers
/// every L2 block a batch in that block produced. [`Self::mark_inclusion_verified`] records the
/// identity so the actor does not repeat the L1 request for each L2 block, which a node catching
/// up would otherwise do once per derived block. The record holds the hash, so a different block at
/// a verified height is still checked.
///
/// # Limits
///
/// The finalizer only knows the blocks it recorded. It records nothing for blocks derived before
/// it started, or blocks the engine adopted without derivation. Finalizing a higher block
/// finalizes those too, and the engine resolves the target by number. Retained records also assume
/// that the engine's safe chain at or below the retained bound is the chain derivation confirmed.
#[derive(Debug, Default)]
pub struct L2Finalizer {
    /// The L1 inclusion block recorded for each derived L2 block awaiting finalization.
    records: BTreeMap<L2BlockNumber, BlockInfo>,
    /// Barrier L2 blocks: blocks the finalizer holds no usable evidence for. An entry is added when
    /// the block's inclusion block was rejected as not canonical ([`Self::reject_inclusion`]), or
    /// when a reset leaves a block at or below its safe head that it cannot vouch for
    /// ([`Self::retain_valid_after_reset`]), such as one that was dispatched but never confirmed.
    ///
    /// Nothing at or above the lowest of them is finalized. An entry is removed when the block is
    /// derived again, or when a reset moves the safe head below it.
    unproven: BTreeSet<L2BlockNumber>,
    /// The highest finalized L1 block observed by the finalizer.
    ///
    /// This survives derivation resets so that re-derived L2 blocks can be finalized without
    /// waiting for the L1 watcher to emit the same finalized block again.
    finalized_l1_block: Option<BlockInfo>,
    /// The safe head the engine last reported from derivation, or the head the last reset
    /// restarted from. It identifies the engine's own echo of a reset, and a reset to a different
    /// block at the same height.
    safe_head: Option<L2BlockInfo>,
    /// The highest L2 block whose record describes a block the engine holds. A reset keeps the
    /// records at or below it (and at or below the reset safe head). Records above it that are at
    /// or below the reset safe head were dispatched but never confirmed, so they become barriers;
    /// records above the reset safe head are dropped. `None` before the first confirmation, after
    /// the engine adopted a safe head derivation did not produce, and after a reset that left no
    /// confirmed block to keep.
    confirmed_through: Option<L2BlockNumber>,
    /// Inclusion blocks already verified canonical, keyed by L1 block number.
    ///
    /// Pruned in [`Self::try_finalize_next`] once derivation has advanced past them.
    verified_inclusions: BTreeMap<L1BlockNumber, B256>,
}

impl L2Finalizer {
    /// Enqueues a derived [`AttributesWithParent`] for finalization. When a new finalized L1
    /// block is observed that is `>=` the height of [`AttributesWithParent::derived_from`], and
    /// the same holds for every L2 block below it, the L2 block associated with the payload
    /// attributes can be finalized.
    ///
    /// Deriving a block again replaces its record and releases its barrier, if any: the new
    /// record is fresh evidence for that block.
    pub fn enqueue_for_finalization(&mut self, attributes: &AttributesWithParent) {
        let inclusion = attributes
            .derived_from
            .expect("Fatal: Cannot enqueue attributes for finalization that weren't derived");
        let l2_block = attributes.block_number();
        self.records.insert(l2_block, inclusion);
        self.unproven.remove(&l2_block);
    }

    /// Records a finalized L1 block.
    ///
    /// Finalized L1 signals are monotonic by block number. Older signals are ignored, while a
    /// signal at the same height is accepted so that its block identity remains current. Returns
    /// whether the signal was recorded.
    ///
    /// Recording does not finalize anything: the actor verifies the inclusion blocks of the
    /// candidate it is about to promote first.
    pub fn record_finalized_l1_block(&mut self, finalized_l1_block: BlockInfo) -> bool {
        if self
            .finalized_l1_block
            .as_ref()
            .is_some_and(|previous| finalized_l1_block.number < previous.number)
        {
            return false;
        }

        self.finalized_l1_block = Some(finalized_l1_block);
        true
    }

    /// Returns the highest finalized L1 block observed, if any.
    pub const fn finalized_l1_block(&self) -> Option<BlockInfo> {
        self.finalized_l1_block
    }

    /// Returns what finalizing up to `finalized_l1_number` would promote, without removing it
    /// from the queue.
    ///
    /// Finalizing an L2 block finalizes every block below it, so the candidate stops before the
    /// first recorded L2 block whose inclusion block is not final yet, and before the lowest
    /// block on the barrier. Everything below that point is covered, and the candidate lists the
    /// inclusion block of each of those blocks.
    ///
    /// The actor verifies every listed inclusion. Blocks derived from several L1 blocks cost one
    /// request per distinct block, and [`Self::verified_inclusion`] answers repeats.
    pub fn pending_candidate(
        &self,
        finalized_l1_number: L1BlockNumber,
    ) -> Option<PendingFinalization> {
        let barrier = self.unproven.first().copied();
        let mut l2_block = None;
        let mut inclusions: Vec<BlockInfo> = Vec::new();
        for (&number, &inclusion) in &self.records {
            if barrier.is_some_and(|barrier| number >= barrier)
                || inclusion.number > finalized_l1_number
            {
                break;
            }
            l2_block = Some(number);
            if !inclusions.contains(&inclusion) {
                inclusions.push(inclusion);
            }
        }
        Some(PendingFinalization { l2_block: l2_block?, inclusions })
    }

    /// Rejects `inclusion`, whose L1 block turned out not to be canonical.
    ///
    /// Every L2 block recorded under it loses its record and goes on the barrier: its inputs are
    /// gone, so neither it nor any block above it finalizes until it is derived again or a reset
    /// rolls the safe head back below it. Blocks below the barrier still finalize.
    pub fn reject_inclusion(&mut self, inclusion: BlockInfo) {
        let rejected: Vec<L2BlockNumber> = self
            .records
            .iter()
            .filter(|(_, recorded)| **recorded == inclusion)
            .map(|(&number, _)| number)
            .collect();
        for number in rejected {
            self.records.remove(&number);
            self.unproven.insert(number);
        }
    }

    /// Returns whether `inclusion` was already verified canonical, so the caller can skip the
    /// L1 request.
    ///
    /// The hit requires the same hash, not just the same height: a different block at a verified
    /// height is a different inclusion identity and has to be checked on its own.
    pub fn verified_inclusion(&self, inclusion: BlockInfo) -> bool {
        self.verified_inclusions
            .get(&inclusion.number)
            .is_some_and(|verified| *verified == inclusion.hash)
    }

    /// Records that `inclusion` is canonical on the finalized L1 chain.
    pub fn mark_inclusion_verified(&mut self, inclusion: BlockInfo) {
        self.verified_inclusions.insert(inclusion.number, inclusion.hash);
    }

    /// Records a safe head the engine reported, so that a later reset can tell which records are
    /// still trustworthy.
    ///
    /// `inclusion` is the L1 inclusion block derivation produced the safe head from. It is
    /// [`None`] for safe heads derivation did not produce: the engine echoing the head a reset
    /// restarted from, which changes nothing, or a head at another height that derivation did not
    /// produce, such as one adopted after EL sync, which forgets the confirmation because the
    /// engine's chain was replaced. A head at the same height is treated as the echo, so the
    /// finalizer cannot tell a replaced block there from the reset head.
    pub fn confirm_safe_head(&mut self, safe_head: &L2BlockInfo, inclusion: Option<BlockInfo>) {
        if inclusion.is_some() {
            self.safe_head = Some(*safe_head);
            self.confirmed_through = Some(safe_head.block_info.number);
        } else if self
            .safe_head
            .is_none_or(|known| known.block_info.number != safe_head.block_info.number)
        {
            self.safe_head = None;
            self.confirmed_through = None;
        }
    }

    /// Applies a derivation reset to the records, keeping every record the reset still allows to
    /// be finalized and turning every record it cannot vouch for into a barrier.
    ///
    /// A reset does not roll back derivation's whole history. The pipeline restarts from
    /// `l2_safe_head`, and every L2 block at or below it stays safe and is never derived again.
    /// If the record of a block the engine confirmed were dropped, nothing would recreate it, and
    /// the L2 chain could only be finalized once new derivation happens to reach a later L1
    /// inclusion block. Repeated resets would then starve finalization whenever they arrive
    /// faster than L1 finality advances, so the queue is filtered instead of cleared.
    ///
    /// - A record is **kept** when its L2 block is at or below both the last confirmed safe head
    ///   and `l2_safe_head`. When the reset lands on a different block than the one confirmed at
    ///   the same height, that height is not kept.
    /// - A record at or below `l2_safe_head` that is not kept becomes a **barrier**: the block
    ///   was dispatched but never confirmed, or was confirmed as a different block, so the
    ///   finalizer cannot vouch for what the engine holds there, and the pipeline does not derive
    ///   it again. Treating the unknown block as proof would let a later block finalize it
    ///   without its inclusion block being final and canonical. With no recorded confirmation at
    ///   all, every record at or below `l2_safe_head` becomes a barrier.
    /// - Barriers at or below `l2_safe_head` stay.
    /// - Records and barriers above `l2_safe_head` are **dropped**: the blocks were rolled back
    ///   and are derived again.
    ///
    /// Kept records say nothing about whether their L1 inclusion blocks still exist. An unchanged
    /// L2 safe head says which L2 chain the engine holds, not whether the L1 batch behind it is
    /// still canonical: sync-start validates a block's epoch origin, so an L1 reorg can replace
    /// the block a batch landed in while the reset returns the very same safe head. No kept
    /// record is trusted for finalization: the actor verifies the identity of every inclusion
    /// block in the finalized prefix first. A kept record is never moved to another inclusion
    /// block.
    ///
    /// The cost of a barrier is liveness. It is removed only by deriving that block again or by
    /// a later reset to a lower safe head, so finalization can stop at a barrier the pipeline
    /// never revisits, until one of those happens or the node restarts.
    pub fn retain_valid_after_reset(&mut self, l2_safe_head: &L2BlockInfo) {
        let head = l2_safe_head.block_info.number;
        let kept_through = self.confirmed_through.and_then(|confirmed_through| {
            let bound = confirmed_through.min(head);
            if self.safe_head.is_some_and(|known| {
                known.block_info.number == head
                    && known.block_info.hash != l2_safe_head.block_info.hash
            }) {
                bound.checked_sub(1)
            } else {
                Some(bound)
            }
        });

        let first_dropped = head.saturating_add(1);
        self.records.split_off(&first_dropped);
        self.unproven.split_off(&first_dropped);
        let unvouched = match kept_through {
            Some(kept_through) => self.records.split_off(&kept_through.saturating_add(1)),
            None => std::mem::take(&mut self.records),
        };
        self.unproven.extend(unvouched.into_keys());

        self.confirmed_through = kept_through;
        self.safe_head = Some(*l2_safe_head);
    }

    /// Attempts to find L2 blocks that can be finalized based on the new finalized L1 block.
    ///
    /// Returns `Some(l2_block_number)` if there is an L2 block that can be finalized, or `None`
    /// if no block is ready or an inclusion block in the finalized prefix was not verified.
    pub fn try_finalize_next(
        &mut self,
        new_finalized_l1_block: BlockInfo,
    ) -> Option<L2BlockNumber> {
        let candidate = self.pending_candidate(new_finalized_l1_block.number)?;
        if !candidate.inclusions.iter().all(|inclusion| self.verified_inclusion(*inclusion)) {
            return None;
        }

        // Every record the promoted block covers is drained.
        self.records = self.records.split_off(&(candidate.l2_block + 1));
        // Verifications below the highest promoted inclusion block are no longer needed: the
        // blocks they covered were drained with it. A replay that inverts inclusion order can
        // still ask about one of them, which only costs another L1 lookup. The highest stays:
        // further L2 blocks can still arrive from it.
        if let Some(highest) = candidate.inclusions.iter().map(|inclusion| inclusion.number).max() {
            self.verified_inclusions = self.verified_inclusions.split_off(&highest);
        }
        Some(candidate.l2_block)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for [`L2Finalizer`] queue management.

    use alloy_eips::BlockNumHash;
    use alloy_primitives::B256;
    use base_common_rpc_types_engine::BasePayloadAttributes;
    use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};

    use super::{L2Finalizer, PendingFinalization};

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

    /// Build an L1 inclusion block at `number` with a distinct hash, so two blocks at the same
    /// height can be told apart the way an L1 reorg does.
    fn inclusion_at(number: u64, hash_byte: u8) -> BlockInfo {
        BlockInfo { number, hash: B256::with_last_byte(hash_byte), ..Default::default() }
    }

    /// Build attributes whose derived L2 block number is `l2_parent_number + 1` and whose L1
    /// inclusion block is `inclusion`.
    fn attrs_included_at(l2_parent_number: u64, inclusion: BlockInfo) -> AttributesWithParent {
        let parent = L2BlockInfo {
            block_info: BlockInfo { number: l2_parent_number, ..Default::default() },
            l1_origin: BlockNumHash::default(),
            seq_num: 0,
        };
        AttributesWithParent::new(BasePayloadAttributes::default(), parent, Some(inclusion), false)
    }

    /// Build a [`BlockInfo`] representing a finalized L1 block at `number`.
    fn l1_at(number: u64) -> BlockInfo {
        BlockInfo { number, ..Default::default() }
    }

    /// Build the [`L2BlockInfo`] of safe head `number` whose L1 epoch origin is `origin_number`.
    fn safe_head_at(number: u64, origin_number: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo { number, ..Default::default() },
            l1_origin: BlockNumHash { number: origin_number, ..Default::default() },
            seq_num: 0,
        }
    }

    /// Verifies every inclusion in the candidate for `finalized_l1` and promotes it, the way the
    /// actor does when the L1 chain agrees with every recorded block.
    fn finalize_verified(f: &mut L2Finalizer, finalized_l1: BlockInfo) -> Option<u64> {
        promote(f, finalized_l1, |_| true)
    }

    /// Records `finalized_l1` and promotes whatever it makes eligible, with every inclusion
    /// canonical.
    fn record_and_finalize(f: &mut L2Finalizer, finalized_l1: BlockInfo) -> Option<u64> {
        f.record_finalized_l1_block(finalized_l1);
        finalize_verified(f, finalized_l1)
    }

    /// Promotes whatever the recorded finalized L1 signal makes eligible, with every inclusion
    /// canonical.
    fn finalize_pending(f: &mut L2Finalizer) -> Option<u64> {
        let finalized_l1 = f.finalized_l1_block()?;
        finalize_verified(f, finalized_l1)
    }

    /// Mirrors the actor's loop: verifies each inclusion of the candidate against `canonical`,
    /// rejects the ones that are not, and promotes once every inclusion passed.
    fn promote(
        f: &mut L2Finalizer,
        finalized_l1: BlockInfo,
        canonical: impl Fn(BlockInfo) -> bool,
    ) -> Option<u64> {
        loop {
            let candidate = f.pending_candidate(finalized_l1.number)?;
            let mut rejected = false;
            for inclusion in candidate.inclusions {
                if f.verified_inclusion(inclusion) {
                    continue;
                }
                if canonical(inclusion) {
                    f.mark_inclusion_verified(inclusion);
                } else {
                    f.reject_inclusion(inclusion);
                    rejected = true;
                    break;
                }
            }
            if !rejected {
                return f.try_finalize_next(finalized_l1);
            }
        }
    }

    /// An inclusion check that rejects every block whose hash byte is `0xa`.
    fn not_a(block: BlockInfo) -> bool {
        block.hash != B256::with_last_byte(0xa)
    }

    #[test]
    fn empty_queue_returns_none() {
        let mut f = L2Finalizer::default();
        assert!(f.try_finalize_next(l1_at(100)).is_none());
    }

    #[test]
    fn finalized_l1_signal_is_retained_until_candidates_are_ready() {
        let mut f = L2Finalizer::default();
        assert!(record_and_finalize(&mut f, l1_at(5)).is_none());

        f.enqueue_for_finalization(&attrs(9, 5));

        assert_eq!(finalize_pending(&mut f), Some(10));
    }

    #[test]
    fn single_entry_l1_not_yet_finalized() {
        let mut f = L2Finalizer::default();
        // L2 block 10 was included in L1 block 5. Finalizing at L1=3 must not include it.
        f.enqueue_for_finalization(&attrs(9, 5)); // l2=10, inclusion=5
        assert!(finalize_verified(&mut f, l1_at(3)).is_none());
        // The record must still be in the queue.
        assert!(finalize_verified(&mut f, l1_at(5)).is_some());
    }

    #[test]
    fn single_entry_l1_exactly_at_finalized() {
        // Boundary: the inclusion block equals finalized L1, which is final.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(9, 5)); // l2=10, inclusion=5
        assert_eq!(finalize_verified(&mut f, l1_at(5)), Some(10));
    }

    #[test]
    fn multiple_l2_per_inclusion_block_keeps_highest() {
        // Three L2 blocks included in L1 block 1. Only the highest (3) should be returned.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(0, 1)); // l2=1, inclusion=1
        f.enqueue_for_finalization(&attrs(1, 1)); // l2=2, inclusion=1
        f.enqueue_for_finalization(&attrs(2, 1)); // l2=3, inclusion=1
        assert_eq!(finalize_verified(&mut f, l1_at(1)), Some(3));
    }

    #[test]
    fn partial_finalization_drains_lower_entries() {
        // Blocks at L1=1,2,3. Finalizing at L1=2 returns the highest across L1<=2 and drains it.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(4, 1)); // l2=5,  inclusion=1
        f.enqueue_for_finalization(&attrs(7, 2)); // l2=8,  inclusion=2
        f.enqueue_for_finalization(&attrs(10, 3)); // l2=11, inclusion=3

        assert_eq!(finalize_verified(&mut f, l1_at(2)), Some(8));
        // The L1=3 record must still be present.
        assert_eq!(finalize_verified(&mut f, l1_at(3)), Some(11));
    }

    #[test]
    fn reset_preserves_finalized_l1_signal() {
        let mut f = L2Finalizer::default();
        record_and_finalize(&mut f, l1_at(5));
        f.enqueue_for_finalization(&attrs(4, 1));
        f.confirm_safe_head(&safe_head_at(100, 10), Some(l1_at(10)));

        f.retain_valid_after_reset(&safe_head_at(100, 10));
        f.enqueue_for_finalization(&attrs(9, 5));

        assert_eq!(finalize_pending(&mut f), Some(10));
    }

    #[test]
    fn repeated_resets_do_not_starve_finalization() {
        // Every derived block is included roughly one finality window past the current finalized
        // L1 block, and the pipeline resets before each new block. Clearing the queue on reset
        // left only the newest record, which was never old enough to finalize, so finalization
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
            if let Some(l2) = record_and_finalize(&mut f, l1_at(finalized_l1_number)) {
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

        assert_eq!(finalize_verified(&mut f, l1_at(1_000)), Some(500));
    }

    #[test]
    fn reset_without_a_confirmation_turns_retained_records_into_barriers() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(495, 119)); // l2=496
        f.enqueue_for_finalization(&attrs(499, 120)); // l2=500, the reset safe head
        f.enqueue_for_finalization(&attrs(509, 121)); // l2=510, rolled back by the reset

        // No safe head was ever confirmed, so the reset cannot vouch for 496 and 500. They stay
        // as barriers instead of disappearing, and 510 is derived again.
        f.retain_valid_after_reset(&safe_head_at(500, 118));
        assert!(finalize_verified(&mut f, l1_at(1_000)).is_none());

        // A block derived afterwards does not finalize them.
        f.enqueue_for_finalization(&attrs(500, 119)); // l2=501
        assert!(finalize_verified(&mut f, l1_at(1_000)).is_none());
        assert_eq!(f.pending_candidate(1_000), None);
    }

    #[test]
    fn a_barrier_from_a_reset_without_a_confirmation_is_released_by_deriving_the_block_again() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xa)));
        f.retain_valid_after_reset(&safe_head_at(500, 118));

        // The pipeline derives 500 again from a canonical batch.
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(121, 0xb)));

        assert_eq!(finalize_verified(&mut f, l1_at(121)), Some(500));
    }

    #[test]
    fn a_barrier_from_a_reset_without_a_confirmation_is_released_by_a_lower_reset() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xa)));
        f.retain_valid_after_reset(&safe_head_at(500, 118));

        // A later reset restarts below the barrier, so derivation produces 500 again.
        f.retain_valid_after_reset(&safe_head_at(499, 118));
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(121, 0xb)));

        assert_eq!(finalize_verified(&mut f, l1_at(121)), Some(500));
    }

    #[test]
    fn a_reset_without_a_confirmation_keeps_a_rejected_barrier() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xa)));
        assert_eq!(promote(&mut f, l1_at(120), not_a), None);

        f.retain_valid_after_reset(&safe_head_at(500, 118));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(121, 0xb)));

        assert_eq!(promote(&mut f, l1_at(121), |_| true), None);
    }

    #[test]
    fn reset_turns_a_dispatched_unconfirmed_block_at_the_reset_head_into_a_barrier() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(498, inclusion_at(119, 0xb))); // l2=499
        f.confirm_safe_head(&safe_head_at(499, 118), Some(inclusion_at(119, 0xb)));
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xa))); // l2=500

        // 500 was dispatched but not confirmed, and the reset restarts from it, so derivation
        // never produces it again. A successor from an earlier inclusion block must not
        // finalize it.
        f.retain_valid_after_reset(&safe_head_at(500, 118));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(119, 0xb))); // l2=501

        assert_eq!(finalize_verified(&mut f, l1_at(119)), Some(499));
        assert_eq!(f.pending_candidate(1_000), None);
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

        assert_eq!(finalize_verified(&mut f, l1_at(1_000)), Some(500));
    }

    #[test]
    fn reset_at_the_same_height_with_a_different_hash_drops_that_height() {
        let mut f = L2Finalizer::default();
        f.confirm_safe_head(&safe_head_at(500, 118), Some(l1_at(120)));
        f.enqueue_for_finalization(&attrs(498, 119)); // l2=499
        f.enqueue_for_finalization(&attrs(499, 120)); // l2=500, included at the confirmation

        // Same height, different block: the engine does not hold the block derivation confirmed
        // at 500, so its record goes, while the block below it is unaffected.
        let mut other_head = safe_head_at(500, 118);
        other_head.block_info.hash = B256::with_last_byte(1);
        f.retain_valid_after_reset(&other_head);

        assert_eq!(finalize_verified(&mut f, l1_at(1_000)), Some(499));
    }

    #[test]
    fn reset_that_moves_the_safe_head_keeps_every_record_below_it() {
        let mut f = L2Finalizer::default();
        f.confirm_safe_head(&safe_head_at(600, 100), Some(l1_at(101)));
        f.enqueue_for_finalization(&attrs(479, 100)); // l2=480
        f.enqueue_for_finalization(&attrs(489, 101)); // l2=490, included above the new origin

        // The reset rolls back to 500. Both blocks are at or below it and are never derived
        // again, so both records stay and wait for their inclusion blocks.
        f.retain_valid_after_reset(&safe_head_at(500, 100));

        assert_eq!(finalize_verified(&mut f, l1_at(100)), Some(480));
        assert_eq!(finalize_verified(&mut f, l1_at(101)), Some(490));
    }

    #[test]
    fn reset_drops_candidates_above_the_new_safe_head() {
        // A reset that rolls the safe head back must not leave candidates above it behind.
        let mut f = L2Finalizer::default();
        f.confirm_safe_head(&safe_head_at(600, 100), Some(l1_at(108)));
        f.enqueue_for_finalization(&attrs(479, 105)); // l2=480, within the rolled-back chain
        f.enqueue_for_finalization(&attrs(599, 108)); // l2=600, reorged out

        f.retain_valid_after_reset(&safe_head_at(500, 110));

        // L2 600 is gone with the rolled-back chain. The queue holds no record for 500, so only
        // 480 is promoted; the next block derived above 500 covers 500 once its own inclusion
        // block, and every lower one, is final and canonical.
        assert_eq!(finalize_verified(&mut f, l1_at(1_000)), Some(480));
    }

    #[test]
    fn pending_candidate_carries_the_inclusion_identity() {
        let mut f = L2Finalizer::default();
        let inclusion = inclusion_at(120, 0xa);
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion));

        // The candidate exposes the recorded inclusion hash, not just its height: finalization
        // has to tell 120/A from 120/B.
        assert_eq!(
            f.pending_candidate(120),
            Some(PendingFinalization { l2_block: 500, inclusions: vec![inclusion] })
        );
        assert_eq!(f.pending_candidate(119), None);
    }

    #[test]
    fn a_rejected_inclusion_does_not_hold_back_lower_blocks() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(479, inclusion_at(110, 0xb)));
        f.enqueue_for_finalization(&attrs_included_at(489, inclusion_at(112, 0xa)));

        // The higher block's inclusion block was replaced, so it and everything above it wait.
        assert_eq!(promote(&mut f, l1_at(200), not_a), Some(480));
        assert_eq!(f.pending_candidate(200), None);
    }

    #[test]
    fn inclusion_block_bounds_the_epoch_origin() {
        // A batch may be included in any L1 block at or after its epoch origin, so the epoch
        // origin never proves that the L1 inputs are final on its own. Finalization follows the
        // inclusion block: the record is not eligible when finalized L1 has covered the block's
        // epoch but not the L1 block its batch was actually included in.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_at(9, 18, 20)); // l2=10, epoch 18, included at L1 20

        assert!(finalize_verified(&mut f, l1_at(19)).is_none());
        assert_eq!(finalize_verified(&mut f, l1_at(20)), Some(10));
    }

    #[test]
    fn drain_preserves_future_entries() {
        // After finalizing up to L1=2, records at L1=5 must survive.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(4, 2)); // l2=5,  inclusion=2
        f.enqueue_for_finalization(&attrs(9, 5)); // l2=10, inclusion=5

        assert_eq!(finalize_verified(&mut f, l1_at(2)), Some(5));
        // The L1=5 record is still present; finalizing it now returns l2=10.
        assert_eq!(finalize_verified(&mut f, l1_at(5)), Some(10));
    }

    #[test]
    fn old_finalized_signal_returns_none_after_drain() {
        // After draining all records up to L1=5, a later signal for L1=2 finds nothing.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs(19, 5)); // l2=20, inclusion=5

        assert_eq!(finalize_verified(&mut f, l1_at(5)), Some(20));
        // The queue is now empty; an older signal cannot regress to a stale record.
        assert!(finalize_verified(&mut f, l1_at(2)).is_none());
    }

    #[test]
    fn reset_keeps_confirmed_work_under_an_inclusion_with_unconfirmed_blocks() {
        let mut f = L2Finalizer::default();
        // L2 500 and 501 both came from the batch in L1 120; the engine confirmed 500 only.
        let inclusion = inclusion_at(120, 0xb);
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion));
        f.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion));

        // The reset rolls back to 500, which is never derived again, so its record must survive
        // even though the unconfirmed block above it does not.
        f.retain_valid_after_reset(&safe_head_at(500, 118));

        assert_eq!(record_and_finalize(&mut f, l1_at(120)), Some(500));

        // A reset to 501 keeps 500 and drops 501: only 500 was confirmed.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion));
        f.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion));
        f.retain_valid_after_reset(&safe_head_at(501, 118));
        assert_eq!(f.pending_candidate(200).map(|candidate| candidate.l2_block), Some(500));
    }

    #[test]
    fn reset_does_not_move_a_confirmed_block_to_an_earlier_inclusion() {
        let mut f = L2Finalizer::default();
        // L2 500 was confirmed from the batch in L1 120.
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xb)));
        f.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_at(120, 0xb)));
        f.retain_valid_after_reset(&safe_head_at(500, 118));

        // The replay derives L2 501 from the earlier L1 119, but the engine never confirms it, so
        // the next reset rolls back to 500 again.
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(119, 0xb)));
        f.retain_valid_after_reset(&safe_head_at(500, 118));

        // 500 still depends on L1 120. Rewriting the replayed record (119, 501) to (119, 500)
        // would finalize 500 once L1 119 is final.
        assert_eq!(f.pending_candidate(119), None);
        assert_eq!(record_and_finalize(&mut f, l1_at(119)), None);
        assert_eq!(record_and_finalize(&mut f, l1_at(120)), Some(500));
    }

    #[test]
    fn a_block_waits_for_the_inclusion_of_every_block_below_it() {
        let mut f = L2Finalizer::default();
        // After a replay, L2 501 came from L1 119 but the block below it, 500, from L1 120.
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xb)));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(119, 0xb)));

        // Finalizing 501 would finalize 500 with it, whose inputs are not final yet.
        assert_eq!(f.pending_candidate(119), None);
        assert_eq!(finalize_verified(&mut f, l1_at(119)), None);

        // Both blocks finalize together once L1 120 is final.
        assert_eq!(finalize_verified(&mut f, l1_at(120)), Some(501));
    }

    #[test]
    fn a_block_does_not_wait_for_the_inclusion_of_blocks_above_it() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(118, 0xb)));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(120, 0xb)));
        f.enqueue_for_finalization(&attrs_included_at(501, inclusion_at(119, 0xb)));

        // L2 500 (L1 118) is final with L1 119. 501 (L1 120) is not, and 502 (L1 119) cannot be
        // finalized without it.
        assert_eq!(finalize_verified(&mut f, l1_at(119)), Some(500));
        assert_eq!(finalize_verified(&mut f, l1_at(119)), None);
        assert_eq!(finalize_verified(&mut f, l1_at(120)), Some(502));
    }

    #[test]
    fn reset_that_keeps_the_safe_head_keeps_a_lower_block_included_later() {
        let mut f = L2Finalizer::default();
        // L2 500 was confirmed from L1 120. The replay derives 501 from L1 119 and the engine
        // confirms it. Inclusion order is inverted, which must not cost the record for 500.
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xb)));
        f.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_at(120, 0xb)));
        f.retain_valid_after_reset(&safe_head_at(500, 118));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(119, 0xb)));
        f.confirm_safe_head(&safe_head_at(501, 118), Some(inclusion_at(119, 0xb)));

        f.retain_valid_after_reset(&safe_head_at(501, 118));

        // The record for 500 survived, so 501 waits for L1 120 and so does a block derived later.
        f.enqueue_for_finalization(&attrs_included_at(501, inclusion_at(119, 0xb)));
        assert_eq!(f.pending_candidate(119), None);
        assert_eq!(record_and_finalize(&mut f, l1_at(120)), Some(502));
    }

    #[test]
    fn a_retained_block_whose_inclusion_was_replaced_blocks_the_blocks_above_it() {
        let mut f = L2Finalizer::default();
        // L2 500 came from L1 120/A, which a reorg replaced. The reset kept 500 because its epoch
        // origin is still canonical, and 501 arrives from the canonical 121/B.
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xa)));
        f.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_at(120, 0xa)));
        f.retain_valid_after_reset(&safe_head_at(500, 118));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(121, 0xb)));

        // Verifying 121/B alone would finalize 500 on a batch that is gone.
        assert_eq!(f.pending_candidate(121).map(|candidate| candidate.inclusions.len()), Some(2));
        assert_eq!(promote(&mut f, l1_at(121), not_a), None);
    }

    #[test]
    fn a_new_identity_at_a_height_does_not_vouch_for_a_retained_block() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xa)));
        f.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_at(120, 0xa)));
        f.retain_valid_after_reset(&safe_head_at(500, 118));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(120, 0xb)));

        // Both identities at height 120 stay on record, and 120/A is rejected.
        assert_eq!(
            f.pending_candidate(120).map(|candidate| candidate.inclusions),
            Some(vec![inclusion_at(120, 0xa), inclusion_at(120, 0xb)])
        );
        assert_eq!(promote(&mut f, l1_at(120), not_a), None);
    }

    #[test]
    fn a_rejected_inclusion_keeps_blocking_later_blocks() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xa)));
        f.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_at(120, 0xa)));
        f.retain_valid_after_reset(&safe_head_at(500, 118));

        // 500's batch is rejected, and a block derived afterwards from a canonical inclusion
        // cannot finalize it.
        assert_eq!(promote(&mut f, l1_at(120), not_a), None);
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(121, 0xb)));
        assert_eq!(promote(&mut f, l1_at(121), |_| true), None);
        assert_eq!(f.pending_candidate(121), None);
    }

    #[test]
    fn a_rejected_block_is_released_when_it_is_derived_again() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xa)));
        f.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_at(120, 0xa)));
        f.retain_valid_after_reset(&safe_head_at(500, 118));
        assert_eq!(promote(&mut f, l1_at(120), not_a), None);
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(121, 0xb)));

        // The pipeline derives 500 again from a canonical batch.
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(121, 0xb)));

        assert_eq!(promote(&mut f, l1_at(121), not_a), Some(501));
    }

    #[test]
    fn a_reset_below_a_rejected_block_releases_it() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(498, inclusion_at(119, 0xb)));
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xa)));
        f.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_at(120, 0xa)));
        assert_eq!(promote(&mut f, l1_at(120), not_a), Some(499));

        // The reset rolls the safe head back below 500, so derivation produces 500 again.
        f.retain_valid_after_reset(&safe_head_at(499, 118));
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(121, 0xb)));

        assert_eq!(promote(&mut f, l1_at(121), not_a), Some(500));
    }

    #[test]
    fn two_resets_before_a_confirmation_keep_the_retained_block_gated() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(479, inclusion_at(105, 0xb)));
        for l2_block in 481..=500 {
            f.enqueue_for_finalization(&attrs_included_at(l2_block - 1, inclusion_at(110, 0xb)));
        }
        for l2_block in 501..=600 {
            f.enqueue_for_finalization(&attrs_included_at(l2_block - 1, inclusion_at(112, 0xb)));
        }
        f.confirm_safe_head(&safe_head_at(600, 111), Some(inclusion_at(112, 0xb)));

        // Each reset is followed by the engine reporting the moved safe head, which carries no
        // inclusion block because derivation did not produce it.
        for reset_head in [safe_head_at(500, 110), safe_head_at(480, 100)] {
            f.retain_valid_after_reset(&reset_head);
            f.confirm_safe_head(&reset_head, None);
        }
        f.enqueue_for_finalization(&attrs_included_at(480, inclusion_at(100, 0xb)));
        f.confirm_safe_head(&safe_head_at(481, 100), Some(inclusion_at(100, 0xb)));

        // L2 481 is final with L1 100, but 480 below it was included at L1 105.
        assert_eq!(finalize_verified(&mut f, l1_at(100)), None);
        assert_eq!(finalize_verified(&mut f, l1_at(105)), Some(481));
    }

    #[test]
    fn a_lost_inclusion_is_not_vouched_for_by_another_block_at_its_height() {
        let mut f = L2Finalizer::default();
        // L2 490 came from L1 101/A, which a reorg replaced. A block derived later from 101/B
        // must not finalize it just because L1 101 is final.
        f.enqueue_for_finalization(&attrs_included_at(489, inclusion_at(101, 0xa)));
        f.enqueue_for_finalization(&attrs_included_at(599, inclusion_at(112, 0xc)));
        f.confirm_safe_head(&safe_head_at(600, 100), Some(inclusion_at(112, 0xc)));
        f.retain_valid_after_reset(&safe_head_at(500, 100));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(100, 0xb)));

        assert_eq!(promote(&mut f, l1_at(101), not_a), None);
        assert_eq!(f.pending_candidate(101), None);
    }

    #[test]
    fn a_safe_head_derivation_did_not_produce_forgets_the_confirmation() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xb)));
        f.confirm_safe_head(&safe_head_at(500, 118), Some(inclusion_at(120, 0xb)));

        // The engine adopted a head at another height without derivation, such as after EL sync.
        f.confirm_safe_head(&safe_head_at(900, 300), None);
        f.retain_valid_after_reset(&safe_head_at(900, 300));

        assert_eq!(f.pending_candidate(1_000), None);
    }

    #[test]
    fn try_finalize_next_does_not_finalize_an_unverified_inclusion() {
        let mut f = L2Finalizer::default();
        let inclusion = inclusion_at(120, 0xb);
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion));

        assert_eq!(f.try_finalize_next(l1_at(120)), None);

        f.mark_inclusion_verified(inclusion);
        assert_eq!(f.try_finalize_next(l1_at(120)), Some(500));
    }

    #[test]
    fn replaying_a_block_from_another_inclusion_keeps_both_identities_gated() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xb)));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(120, 0xc)));

        assert_eq!(
            f.pending_candidate(200),
            Some(PendingFinalization {
                l2_block: 501,
                inclusions: vec![inclusion_at(120, 0xb), inclusion_at(120, 0xc)],
            })
        );
    }

    #[test]
    fn reset_replay_that_inverts_inclusion_and_l2_order_promotes_the_highest_block() {
        let mut f = L2Finalizer::default();
        // Retained: L2 500 came from the batch in L1 120. A reset replay then re-derives 501 from
        // a lower inclusion block, so inclusion order no longer follows L2 order.
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xb)));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(119, 0xb)));

        assert_eq!(finalize_verified(&mut f, l1_at(120)), Some(501));
    }

    #[test]
    fn verified_inclusion_requires_the_same_hash() {
        let mut f = L2Finalizer::default();
        let inclusion_a = inclusion_at(120, 0xa);

        assert!(!f.verified_inclusion(inclusion_a));
        f.mark_inclusion_verified(inclusion_a);

        assert!(f.verified_inclusion(inclusion_a));
        // A different block at the verified height is a different identity: it has to be checked.
        assert!(!f.verified_inclusion(inclusion_at(120, 0xb)));
    }

    #[test]
    fn verified_inclusions_are_pruned_once_derivation_advances() {
        let mut f = L2Finalizer::default();
        let kept = inclusion_at(120, 0xb);
        f.mark_inclusion_verified(inclusion_at(118, 0xb));
        f.mark_inclusion_verified(kept);
        f.enqueue_for_finalization(&attrs_included_at(499, kept));

        assert_eq!(f.try_finalize_next(l1_at(120)), Some(500));

        // The promoted inclusion block can still produce more L2 blocks; the verification below
        // it is no longer needed.
        assert!(!f.verified_inclusion(inclusion_at(118, 0xb)));
        assert!(f.verified_inclusion(kept));
    }

    #[test]
    fn older_finalized_signal_does_not_lower_high_water_mark() {
        let mut f = L2Finalizer::default();
        record_and_finalize(&mut f, l1_at(5));
        assert!(record_and_finalize(&mut f, l1_at(3)).is_none());

        f.enqueue_for_finalization(&attrs(9, 4));

        assert_eq!(finalize_pending(&mut f), Some(10));
    }
}
