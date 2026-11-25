//! What the node still holds locally, for routing reads between the node and the archive.

use std::{
    sync::{Mutex, PoisonError},
    time::{Duration, Instant},
};

use alloy_eips::{BlockId, BlockNumberOrTag};
use reth_prune_types::PruneSegment;
use reth_storage_api::{BlockNumReader, PruneCheckpointReader};

/// History domain a state read needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateDomain {
    /// Account history: balance, nonce, code.
    Account,
    /// Storage history: storage slots.
    Storage,
}

/// The node's earliest body and prune checkpoints.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalRetention {
    /// Earliest block whose body the node holds.
    pub earliest_body: u64,
    /// Highest block whose account history is pruned.
    pub account_history_pruned: Option<u64>,
    /// Highest block whose storage history is pruned.
    pub storage_history_pruned: Option<u64>,
    /// Highest block whose receipts are pruned.
    pub receipts_pruned: Option<u64>,
    /// Highest block whose transaction-hash lookup is pruned.
    pub tx_lookup_pruned: Option<u64>,
}

impl LocalRetention {
    /// Reads the node's earliest body and prune checkpoints. A failed read counts as "not
    /// pruned", which keeps the request local (the node then answers as it would without an
    /// archive).
    pub fn read<P: BlockNumReader + PruneCheckpointReader>(provider: &P) -> Self {
        let pruned = |segment| {
            provider.get_prune_checkpoint(segment).ok().flatten().and_then(|c| c.block_number)
        };
        Self {
            earliest_body: provider.earliest_block_number().unwrap_or_default(),
            account_history_pruned: pruned(PruneSegment::AccountHistory),
            storage_history_pruned: pruned(PruneSegment::StorageHistory),
            receipts_pruned: pruned(PruneSegment::Receipts),
            tx_lookup_pruned: pruned(PruneSegment::TransactionLookup),
        }
    }

    /// Whether state at `block` can be outside the node's retention. The head and forkchoice
    /// tags (and an omitted block, which means `latest`) are always retained.
    pub const fn may_be_pruned(block: Option<BlockId>) -> bool {
        !matches!(
            block,
            None | Some(BlockId::Number(
                BlockNumberOrTag::Latest
                    | BlockNumberOrTag::Pending
                    | BlockNumberOrTag::Safe
                    | BlockNumberOrTag::Finalized
            ))
        )
    }

    /// Whether the node lacks `domain` history at `number`.
    pub fn state_pruned(&self, number: u64, domain: StateDomain) -> bool {
        let pruned = match domain {
            StateDomain::Account => self.account_history_pruned,
            StateDomain::Storage => self.storage_history_pruned,
        };
        pruned.is_some_and(|pruned| number <= pruned)
    }

    /// Whether a local block, transaction or receipt answer for `number` is incomplete: the
    /// body is gone (reth's block cache may still answer) or the receipts that carry Base's
    /// deposit fields are.
    pub fn answer_incomplete(&self, number: u64) -> bool {
        number < self.earliest_body || self.receipts_pruned.is_some_and(|pruned| number <= pruned)
    }

    /// Lowest block whose body and receipts the node holds.
    pub fn first_complete(&self) -> u64 {
        self.earliest_body.max(self.receipts_pruned.map_or(0, |pruned| pruned.saturating_add(1)))
    }

    /// Whether a local miss may be history the node pruned. A number qualifies only below what
    /// the node fully holds, never above its head; a hash (`None`) only when the node pruned
    /// bodies, receipts or its transaction lookup. Polling for an unmined receipt therefore
    /// never reaches the archive on an unpruned node.
    pub fn archive_may_hold(&self, number: Option<u64>) -> bool {
        number.map_or_else(
            || {
                self.earliest_body > 0
                    || self.receipts_pruned.is_some()
                    || self.tx_lookup_pruned.is_some()
            },
            |number| self.answer_incomplete(number),
        )
    }
}

/// The node's [`LocalRetention`], reused for `ttl` so high-volume RPCs do not open several
/// provider transactions each. A block pruned inside the TTL is still caught: the node
/// refuses it as pruned, and that refusal goes to the archive.
#[derive(Debug)]
pub struct RetentionCache {
    /// How long one read is reused.
    pub ttl: Duration,
    /// Last read and when it was taken.
    pub cached: Mutex<Option<(Instant, LocalRetention)>>,
}

impl RetentionCache {
    /// Creates an empty cache that reuses a read for `ttl`.
    pub const fn new(ttl: Duration) -> Self {
        Self { ttl, cached: Mutex::new(None) }
    }

    /// Returns the cached retention, or calls `read` once the cached value is older than
    /// [`Self::ttl`].
    pub fn get(&self, read: impl FnOnce() -> LocalRetention) -> LocalRetention {
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, retention)) = *cached
            && at.elapsed() < self.ttl
        {
            return retention;
        }
        let retention = read();
        *cached = Some((Instant::now(), retention));
        retention
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// State reads at head and forkchoice tags never consult retention; numbers, hashes and
    /// `earliest` do.
    #[test]
    fn recent_tags_skip_the_retention_read() {
        for tag in [
            BlockNumberOrTag::Latest,
            BlockNumberOrTag::Pending,
            BlockNumberOrTag::Safe,
            BlockNumberOrTag::Finalized,
        ] {
            assert!(!LocalRetention::may_be_pruned(Some(BlockId::Number(tag))), "{tag:?} skips");
        }
        assert!(!LocalRetention::may_be_pruned(None), "omitted block means latest");
        assert!(LocalRetention::may_be_pruned(Some(BlockId::number(5))), "a number reads it");
        assert!(
            LocalRetention::may_be_pruned(Some(BlockId::Number(BlockNumberOrTag::Earliest))),
            "earliest reads it"
        );
        assert!(LocalRetention::may_be_pruned(Some(BlockId::hash(Default::default()))), "a hash reads it");
    }

    /// Routing is per domain, the checkpoint block itself is pruned, and receipt-only pruning
    /// makes local answers incomplete.
    #[test]
    fn routing_follows_each_checkpoint() {
        let storage_only = LocalRetention { storage_history_pruned: Some(100), ..Default::default() };
        assert!(!storage_only.state_pruned(50, StateDomain::Account), "account history is local");
        assert!(storage_only.state_pruned(100, StateDomain::Storage), "checkpoint block is pruned");
        assert!(!storage_only.state_pruned(101, StateDomain::Storage), "101 is local");
        let receipts = LocalRetention { receipts_pruned: Some(40), ..Default::default() };
        assert!(receipts.answer_incomplete(40), "receipts pruned at 40");
        assert!(!receipts.answer_incomplete(41), "receipts kept from 41");
        assert_eq!(receipts.first_complete(), 41, "first complete block");
        let bodies = LocalRetention { earliest_body: 10, receipts_pruned: Some(4), ..Default::default() };
        assert_eq!(bodies.first_complete(), 10, "bodies bound the complete range");
    }

    /// Misses reach the archive only for pruned history: never above what the node holds, and
    /// for hashes only once something hash-addressable was pruned.
    #[test]
    fn misses_reach_the_archive_only_for_pruned_history() {
        let unpruned = LocalRetention::default();
        assert!(!unpruned.archive_may_hold(None), "unknown hash on an unpruned node stays local");
        assert!(!unpruned.archive_may_hold(Some(1_000)), "head + 1 on an unpruned node");
        let lookup = LocalRetention { tx_lookup_pruned: Some(3), ..Default::default() };
        assert!(lookup.archive_may_hold(None), "pruned tx lookup qualifies hashes");
        let bodies = LocalRetention { earliest_body: 10, ..Default::default() };
        assert!(bodies.archive_may_hold(Some(9)), "block 9 is pruned");
        assert!(!bodies.archive_may_hold(Some(10)), "block 10 is local");
    }

    /// Every call inside the TTL reuses one read; the first after it reads again.
    #[test]
    fn retention_is_read_once_per_ttl() {
        let cache = RetentionCache::new(Duration::from_millis(100));
        let reads = std::cell::Cell::new(0u64);
        let read = |earliest_body| {
            reads.set(reads.get() + 1);
            LocalRetention { earliest_body, ..Default::default() }
        };
        for call in 0..50 {
            assert_eq!(cache.get(|| read(10)).earliest_body, 10, "call {call} sees the first read");
        }
        assert_eq!(reads.get(), 1, "one provider read inside the TTL");
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(cache.get(|| read(20)).earliest_body, 20, "a call after the TTL reads again");
        assert_eq!(reads.get(), 2, "exactly one refresh");
    }
}
