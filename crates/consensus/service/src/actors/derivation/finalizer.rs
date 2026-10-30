//! The [`L2Finalizer`] for the derivation actor.

use std::collections::BTreeMap;

use alloy_primitives::B256;
use base_protocol::{AttributesWithParent, BlockInfo, L2BlockInfo};

/// An internal type alias for L1 block numbers.
type L1BlockNumber = u64;

/// An internal type alias for L2 block numbers.
type L2BlockNumber = u64;

/// A derived L2 block that can be finalized, together with the L1 block whose finality allows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingFinalization {
    /// The L1 inclusion block that has to be canonical before the block is finalized.
    pub inclusion: BlockInfo,
    /// The L2 block number to finalize.
    pub l2_block: L2BlockNumber,
}

/// The L2 blocks derived from one L1 inclusion block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InclusionBatch {
    /// The L1 inclusion block recorded when the attributes were derived.
    pub inclusion: BlockInfo,
    /// The lowest L2 block derived from the inclusion block.
    pub first_l2_block: L2BlockNumber,
    /// The highest L2 block derived from the inclusion block.
    pub last_l2_block: L2BlockNumber,
}

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
///
/// # One verification per finalized inclusion block
///
/// The actor promotes a candidate only after confirming that its inclusion block is canonical on
/// the finalized L1 chain, and that answer is final: an inclusion block at or below the finalized
/// L1 height cannot be replaced afterwards. [`Self::mark_inclusion_verified`] records the verified
/// identity so the actor does not repeat the L1 request for every L2 block the same batch
/// produced, which a node catching up would otherwise do once per derived block. The record is
/// keyed by height but holds the hash, so a different block at a verified height is still checked,
/// and [`Self::try_finalize_next`] prunes the entries derivation has moved past.
#[derive(Debug, Default)]
pub struct L2Finalizer {
    /// A map of `L1 inclusion block number -> range of derived L2 blocks` included in that block,
    /// used to track derived [`AttributesWithParent`] awaiting finalization. When a new finalized
    /// L1 block is received, the highest L2 block whose inputs, and the inputs of every L2 block
    /// below it, are contained within the finalized L1 chain is finalized.
    awaiting_finalization: BTreeMap<L1BlockNumber, InclusionBatch>,
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
    /// Inclusion blocks already verified canonical, keyed by L1 block number.
    ///
    /// A finalized L1 height is immutable, so one verification covers every L2 block a batch in
    /// that block produced, however many safe-head confirmations arrive afterwards. Entries are
    /// pruned in [`Self::try_finalize_next`] once derivation has advanced past them.
    verified_inclusions: BTreeMap<L1BlockNumber, B256>,
}

impl L2Finalizer {
    /// Enqueues a derived [`AttributesWithParent`] for finalization. When a new finalized L1
    /// block is observed that is `>=` the height of [`AttributesWithParent::derived_from`], the
    /// L2 block associated with the payload attributes will be finalized.
    pub fn enqueue_for_finalization(&mut self, attributes: &AttributesWithParent) {
        let inclusion = attributes
            .derived_from
            .expect("Fatal: Cannot enqueue attributes for finalization that weren't derived");
        let l2_block = attributes.block_number();
        self.awaiting_finalization
            .entry(inclusion.number)
            .and_modify(|batch| {
                if batch.inclusion.hash == inclusion.hash {
                    batch.first_l2_block = batch.first_l2_block.min(l2_block);
                    batch.last_l2_block = batch.last_l2_block.max(l2_block);
                } else if l2_block >= batch.last_l2_block {
                    // A different block at the same height is a different inclusion identity, and
                    // the entry has to describe the L1 block that produced its highest L2 block.
                    // Keeping the first hash would make the canonicality check verify the wrong
                    // block, discarding candidates that are valid or promoting ones that are not.
                    *batch = InclusionBatch {
                        inclusion,
                        first_l2_block: l2_block,
                        last_l2_block: l2_block,
                    };
                }
            })
            .or_insert(InclusionBatch {
                inclusion,
                first_l2_block: l2_block,
                last_l2_block: l2_block,
            });
    }

    /// Records a finalized L1 block.
    ///
    /// Finalized L1 signals are monotonic by block number. Older signals are ignored, while a
    /// signal at the same height is accepted so that its block identity remains current. Returns
    /// whether the signal was recorded.
    ///
    /// Recording does not finalize anything: the actor verifies the inclusion block of the
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

    /// Returns the candidate that finalizing up to `finalized_l1_number` would promote, without
    /// removing it from the queue.
    ///
    /// Finalizing an L2 block finalizes every block below it, so a block is only a candidate when
    /// the inclusion block of **every** L2 block at or below it is final, not just its own. L2
    /// order and inclusion order agree while derivation runs forward, but a reset replay can derive
    /// a higher L2 block from a lower L1 block than a retained lower block. With L2 500 included
    /// in L1 120 and L2 501 in L1 119, finalizing at L1 119 must not promote 501, because that
    /// finalizes 500 before L1 120 is final. The candidate is therefore capped just below the lowest
    /// L2 block whose inclusion block is not final yet.
    ///
    /// The returned inclusion is the highest eligible inclusion block among those that gate the
    /// candidate. Callers must confirm that it is canonical before calling
    /// [`Self::try_finalize_next`], which drains every entry the candidate covers. They can skip
    /// the L1 request when [`Self::verified_inclusion`] already answered for that block.
    pub fn pending_candidate(
        &self,
        finalized_l1_number: L1BlockNumber,
    ) -> Option<PendingFinalization> {
        let cap = self
            .awaiting_finalization
            .range(finalized_l1_number.saturating_add(1)..)
            .map(|(_, batch)| batch.first_l2_block.saturating_sub(1))
            .min()
            .unwrap_or(L2BlockNumber::MAX);
        let eligible = || {
            self.awaiting_finalization
                .range(..=finalized_l1_number)
                .map(|(_, batch)| batch)
                .filter(|batch| batch.first_l2_block <= cap)
        };
        let l2_block = eligible().map(|batch| batch.last_l2_block).max()?.min(cap);
        let inclusion = eligible()
            .filter(|batch| batch.first_l2_block <= l2_block)
            .map(|batch| batch.inclusion)
            .max_by_key(|inclusion| inclusion.number)?;
        Some(PendingFinalization { inclusion, l2_block })
    }

    /// Drops the candidate included in `inclusion_block_number`, whose L1 block turned out not to
    /// be canonical.
    pub fn drop_candidate(&mut self, inclusion_block_number: L1BlockNumber) {
        self.awaiting_finalization.remove(&inclusion_block_number);
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
    /// - Its lowest L2 block is at or below the last confirmed safe head, and at or below
    ///   `l2_safe_head`; the entry keeps only the blocks up to that bound.
    ///   The confirmed-safe-head bound is what keeps a candidate that was dispatched to the engine
    ///   but never confirmed from being finalized: a reset walks back from the unsafe head, so it
    ///   can move the safe head above the last confirmed block, and the block the engine holds at
    ///   that height need not be the one derivation produced. Above the reset safe head a block was
    ///   rolled back and is re-derived.
    /// - Its L1 inclusion block is at or below the L1 block the reset can still vouch for: the
    ///   inclusion block recorded for the confirmed safe head when the reset keeps that safe head,
    ///   otherwise the reset safe head's epoch origin. The bound binds when the reset moved the
    ///   safe head and the L1 range behind the engine's safe chain is no longer known.
    ///
    /// Retention only ever shrinks an entry's range; it never records a block under an inclusion
    /// block that did not carry it. Inclusion order does not always follow L2 order across resets:
    /// a replay can derive L2 501 from L1 119 while the retained L2 500 came from L1 120. If a
    /// reset to 500 rewrote the replayed entry `(119, 501)` to `(119, 500)`, 500 would finalize
    /// once L1 119 is final, before the L1 block that really carries it. An entry with no
    /// confirmed block left is dropped instead.
    ///
    /// This bound decides which candidates to keep, not which are safe to finalize: a retained
    /// candidate still has to prove its inclusion block is canonical before it is promoted.
    ///
    /// A reset with no recorded confirmation keeps nothing.
    ///
    /// # Retained candidates are not proof of canonical inclusion
    ///
    /// Retention preserves candidates across a reset; it does not prove that their L1 inclusion
    /// blocks still exist. An unchanged L2 safe head says which L2 chain the engine holds, not
    /// whether the L1 batch behind it is still on the canonical chain: sync-start validates a
    /// block's epoch-origin hash, so an L1 reorg can replace the block a batch landed in while the
    /// reset returns the very same safe head. A finalizer that promoted such a candidate would
    /// finalize an L2 block whose inputs are no longer derivable.
    ///
    /// Each candidate therefore keeps the full [`BlockInfo`] of its inclusion block, and the actor
    /// verifies that block against the canonical L1 chain before promoting the candidate. A
    /// candidate whose inclusion block was replaced, or whose lookup failed, is never finalized.
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

        // A batch can carry blocks above the last confirmed safe head as well as below it. The
        // blocks above it are rolled back and re-derived, but the ones at or below it are never
        // re-derived, so the entry keeps its confirmed range instead of being dropped along with
        // the unconfirmed block that happened to share its inclusion block.
        //
        // The entry only ever shrinks, and an entry with no confirmed block is dropped. It must
        // not be rewritten to end at the safe head while holding a different range: that would
        // record the confirmed block under this entry's inclusion block, and a replay can derive
        // an unconfirmed block from a lower L1 block than the one the confirmed block came from.
        // Clamping (119, 501) to (119, 500) would then finalize 500 once L1 119 is final, before
        // its real inclusion block, L1 120, is.
        let mut retained = BTreeMap::new();
        for (inclusion, batch) in std::mem::take(&mut self.awaiting_finalization) {
            if inclusion > trusted_inclusion || batch.first_l2_block > safe_l2_number {
                continue;
            }
            retained.insert(
                inclusion,
                InclusionBatch { last_l2_block: batch.last_l2_block.min(safe_l2_number), ..batch },
            );
        }
        self.awaiting_finalization = retained;
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
        let candidate = self.pending_candidate(new_finalized_l1_block.number)?;
        // Drain every L2 block the promoted block covers. An entry whose L2 blocks are all covered
        // is removed. An entry that reaches above the promoted block keeps the part above it, which
        // still waits for the inclusion block that held the promoted block back.
        self.awaiting_finalization.retain(|_, batch| {
            if batch.last_l2_block <= candidate.l2_block {
                return false;
            }
            batch.first_l2_block = batch.first_l2_block.max(candidate.l2_block + 1);
            true
        });
        // Derivation produces batches in L1 order, so no candidate below the promoted inclusion
        // block can appear again and its verification is no longer needed. The promoted block
        // stays: further L2 blocks can still arrive from it.
        self.verified_inclusions = self.verified_inclusions.split_off(&candidate.inclusion.number);
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

    /// Build a confirmed safe head at `number` whose epoch origin is `origin_number`.
    fn safe_head(number: u64, origin_number: u64) -> L2BlockInfo {
        L2BlockInfo {
            block_info: BlockInfo { number, ..Default::default() },
            l1_origin: BlockNumHash { number: origin_number, ..Default::default() },
            seq_num: 0,
        }
    }

    /// Records `finalized_l1` and promotes whatever it makes eligible, the way the actor does once
    /// the candidate's inclusion block is verified.
    fn record_and_finalize(f: &mut L2Finalizer, finalized_l1: BlockInfo) -> Option<u64> {
        f.record_finalized_l1_block(finalized_l1);
        finalize_pending(f)
    }

    /// Promotes whatever the recorded finalized L1 signal makes eligible.
    fn finalize_pending(f: &mut L2Finalizer) -> Option<u64> {
        let finalized_l1 = f.finalized_l1_block()?;
        f.try_finalize_next(finalized_l1)
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
        assert!(record_and_finalize(&mut f, l1_at(5)).is_none());

        f.enqueue_for_finalization(&attrs(9, 5));

        assert_eq!(finalize_pending(&mut f), Some(10));
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

        // L2 600 is gone with the rolled-back chain. The queue holds no record of which L1 block
        // carried 500, so it is not promoted under the inclusion block of the entry that held 600;
        // the next batch derived above 500 finalizes it.
        assert_eq!(f.pending_candidate(1_000).expect("candidate").l2_block, 480);
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
    fn pending_candidate_carries_the_inclusion_identity() {
        let mut f = L2Finalizer::default();
        let inclusion = inclusion_at(120, 0xa);
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion));

        // The candidate exposes the recorded inclusion hash, not just its height: finalization
        // has to tell 120/A from 120/B.
        assert_eq!(
            f.pending_candidate(120),
            Some(PendingFinalization { inclusion, l2_block: 500 })
        );
        assert_eq!(f.pending_candidate(119), None);
    }

    #[test]
    fn drop_candidate_removes_only_the_replaced_inclusion() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(479, inclusion_at(110, 0xa)));
        f.enqueue_for_finalization(&attrs_included_at(489, inclusion_at(112, 0xb)));

        // The higher candidate's inclusion block was replaced, so only it is dropped.
        f.drop_candidate(112);

        assert_eq!(f.pending_candidate(200).map(|candidate| candidate.l2_block), Some(480));
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
    fn enqueuing_a_replaced_inclusion_at_one_height_keeps_the_new_identity() {
        let mut f = L2Finalizer::default();
        // A retained candidate whose batch landed in 120/A, followed by a post-reorg block from
        // 120/B. The entry has to describe the block that produced its highest L2 block, otherwise
        // the canonicality check verifies the wrong identity and drops the valid candidate.
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xa)));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(120, 0xb)));

        let candidate = f.pending_candidate(200).expect("candidate should be queued");
        assert_eq!(candidate.l2_block, 501);
        assert_eq!(candidate.inclusion.hash, inclusion_at(120, 0xb).hash);
    }

    #[test]
    fn reset_keeps_confirmed_work_under_an_inclusion_with_unconfirmed_blocks() {
        let mut f = L2Finalizer::default();
        // L2 500 and 501 both came from the batch in L1 120; the engine confirmed 500 only.
        let inclusion = inclusion_at(120, 0xa);
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion));
        f.confirm_safe_head(&safe_head(500, 118), Some(inclusion));

        // The reset rolls back to 500, which is never re-derived, so its entry must survive even
        // though the unconfirmed block above it does not.
        f.retain_valid_after_reset(&safe_head(500, 118));

        assert_eq!(record_and_finalize(&mut f, l1_at(120)), Some(500));

        // A reset to 501 keeps nothing: 501 was never confirmed and is re-derived.
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion));
        f.confirm_safe_head(&safe_head(500, 118), Some(inclusion));
        f.retain_valid_after_reset(&safe_head(501, 118));
        assert!(f.pending_candidate(200).is_none(), "unconfirmed work must not survive");
    }

    #[test]
    fn reset_replay_that_inverts_inclusion_and_l2_order_promotes_the_highest_block() {
        let mut f = L2Finalizer::default();
        // Retained: L2 500 came from the batch in L1 120. A reset replay then re-derives 501 from
        // a *lower* inclusion block, so inclusion order no longer follows L2 order.
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xa)));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(119, 0xa)));

        // Finalizing L1 120 makes both eligible; the higher L2 block must be the one promoted, or
        // its entry is drained without ever being finalized.
        assert_eq!(f.try_finalize_next(l1_at(120)), Some(501));
    }

    #[test]
    fn reset_does_not_move_a_confirmed_block_to_an_earlier_inclusion() {
        let mut f = L2Finalizer::default();
        // L2 500 was confirmed from the batch in L1 120.
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xa)));
        f.confirm_safe_head(&safe_head(500, 118), Some(inclusion_at(120, 0xa)));
        f.retain_valid_after_reset(&safe_head(500, 118));

        // The replay derives L2 501 from the earlier L1 119, but the engine never confirms it, so
        // the next reset rolls back to 500 again.
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(119, 0xa)));
        f.retain_valid_after_reset(&safe_head(500, 118));

        // 500 still depends on L1 120. Rewriting the replayed entry (119, 501) to (119, 500) would
        // finalize 500 once L1 119 is final.
        assert_eq!(f.pending_candidate(119), None);
        assert_eq!(record_and_finalize(&mut f, l1_at(119)), None);
        assert_eq!(record_and_finalize(&mut f, l1_at(120)), Some(500));
    }

    #[test]
    fn a_block_waits_for_the_inclusion_of_every_block_below_it() {
        let mut f = L2Finalizer::default();
        // After a replay, L2 501 came from L1 119 but the block below it, 500, from L1 120.
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(120, 0xa)));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(119, 0xa)));

        // Finalizing 501 would finalize 500 with it, whose inputs are not final yet.
        assert_eq!(f.pending_candidate(119), None);
        assert_eq!(f.try_finalize_next(l1_at(119)), None);

        // The entry for 501 is kept, and both blocks finalize together once L1 120 is final.
        assert_eq!(f.try_finalize_next(l1_at(120)), Some(501));
    }

    #[test]
    fn a_block_does_not_wait_for_the_inclusion_of_blocks_above_it() {
        let mut f = L2Finalizer::default();
        f.enqueue_for_finalization(&attrs_included_at(499, inclusion_at(118, 0xa)));
        f.enqueue_for_finalization(&attrs_included_at(500, inclusion_at(120, 0xa)));
        f.enqueue_for_finalization(&attrs_included_at(501, inclusion_at(119, 0xa)));

        // L2 500 (L1 118) is final with L1 119. 501 (L1 120) is not, and 502 (L1 119) cannot be
        // finalized without it.
        assert_eq!(f.try_finalize_next(l1_at(119)), Some(500));
        assert_eq!(f.try_finalize_next(l1_at(119)), None);
        assert_eq!(f.try_finalize_next(l1_at(120)), Some(502));
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
        let kept = inclusion_at(120, 0xa);
        f.mark_inclusion_verified(inclusion_at(118, 0xa));
        f.mark_inclusion_verified(kept);
        f.enqueue_for_finalization(&attrs_included_at(499, kept));

        assert_eq!(f.try_finalize_next(l1_at(120)), Some(500));

        // Batches are derived in L1 order, so nothing below the promoted block can come back; the
        // promoted block itself can still produce more L2 blocks.
        assert!(!f.verified_inclusion(inclusion_at(118, 0xa)));
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
