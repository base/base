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

/// Local data a successful local answer is built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerNeeds {
    /// Headers and bodies only: hashes-only blocks and transaction counts.
    Body,
    /// Bodies plus receipts and the transaction lookup: anything with deposit fields.
    Deposits,
}

/// Route of a transaction-hash read the node answered with `null`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashMiss {
    /// Keep the node's answer; the store is not consulted.
    Local,
    /// The node's lookup places the transaction in this block: serve that block's copy.
    Mined(u64),
    /// Resolve the hash through the archive's locator.
    Archive,
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

    /// Whether a local answer of kind `needs` about block `number` may be incomplete.
    /// [`AnswerNeeds::Body`] answers (hashes-only blocks, transaction counts) need only the
    /// body. [`AnswerNeeds::Deposits`] answers carry Base deposit fields, which the node fills
    /// from receipts found through the transaction lookup and silently defaults when either is
    /// gone, so they also need receipts and the lookup.
    pub fn incomplete(&self, number: u64, needs: AnswerNeeds) -> bool {
        let covered = |pruned: Option<u64>| pruned.is_some_and(|pruned| number <= pruned);
        number < self.earliest_body
            || (needs == AnswerNeeds::Deposits
                && (covered(self.receipts_pruned) || covered(self.tx_lookup_pruned)))
    }

    /// Lowest block whose body and receipts the node holds.
    pub fn first_complete(&self) -> u64 {
        self.earliest_body.max(self.receipts_pruned.map_or(0, |pruned| pruned.saturating_add(1)))
    }

    /// Where a transaction-hash read the node answered with `null` goes. `pooled` says the
    /// hash is in the local transaction pool; `located` is the block the node places it in, if
    /// any; `indexed` says the node's transaction lookup has an entry for it (the entry
    /// outlives a pruned body).
    ///
    /// A pooled transaction is pending, and an unpruned lookup covers every transaction the
    /// archive could hold, so a pooled hash or one the unpruned lookup has never indexed
    /// answers locally without consulting the store; this keeps pending-transaction polling
    /// off the archive. A transaction the node still places is served at that identity. Only
    /// an unplaced hash that a pruned lookup may have dropped, or that an unpruned lookup
    /// indexes over a pruned body, goes to the archive's locator.
    pub const fn hash_miss(&self, pooled: bool, located: Option<u64>, indexed: bool) -> HashMiss {
        if pooled {
            return HashMiss::Local;
        }
        if let Some(number) = located {
            return HashMiss::Mined(number);
        }
        if self.tx_lookup_pruned.is_none() && !indexed {
            return HashMiss::Local;
        }
        HashMiss::Archive
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
            |number| self.incomplete(number, AnswerNeeds::Deposits),
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

    /// Whether a successful local answer about block `number` must be replaced. Only an answer
    /// that is visibly incomplete (`visibly_incomplete`: it lacks a deposit field its fork
    /// requires) is checked against the boundaries, the cached one first and then one read
    /// now; a complete answer never reads retention.
    pub fn replace_local(
        &self,
        visibly_incomplete: bool,
        number: u64,
        needs: AnswerNeeds,
        read: impl Fn() -> LocalRetention,
    ) -> bool {
        visibly_incomplete
            && (self.get(&read).incomplete(number, needs)
                || self.refresh(read).incomplete(number, needs))
    }

    /// Calls `read` now, bypassing the TTL, and caches the result. Used after a local read
    /// that may be incomplete: pruning only moves forward, so a boundary read after the local
    /// read proves the local read happened before the data was pruned.
    pub fn refresh(&self, read: impl FnOnce() -> LocalRetention) -> LocalRetention {
        let retention = read();
        *self.cached.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((Instant::now(), retention));
        retention
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
        assert!(
            LocalRetention::may_be_pruned(Some(BlockId::hash(Default::default()))),
            "a hash reads it"
        );
    }

    /// Routing is per domain, the checkpoint block itself is pruned, and receipt-only pruning
    /// makes local answers incomplete.
    #[test]
    fn routing_follows_each_checkpoint() {
        let storage_only =
            LocalRetention { storage_history_pruned: Some(100), ..Default::default() };
        assert!(!storage_only.state_pruned(50, StateDomain::Account), "account history is local");
        assert!(storage_only.state_pruned(100, StateDomain::Storage), "checkpoint block is pruned");
        assert!(!storage_only.state_pruned(101, StateDomain::Storage), "101 is local");
        let receipts = LocalRetention { receipts_pruned: Some(40), ..Default::default() };
        assert!(receipts.incomplete(40, AnswerNeeds::Deposits), "receipts pruned at 40");
        assert!(!receipts.incomplete(41, AnswerNeeds::Deposits), "receipts kept from 41");
        assert!(!receipts.incomplete(40, AnswerNeeds::Body), "hashes-only answers ignore receipts");
        let lookup = LocalRetention { tx_lookup_pruned: Some(30), ..Default::default() };
        assert!(
            lookup.incomplete(30, AnswerNeeds::Deposits),
            "lookup-only pruning defaults deposit fields"
        );
        assert!(!lookup.incomplete(30, AnswerNeeds::Body), "lookup pruning leaves counts intact");
        let bodies = LocalRetention { earliest_body: 10, ..Default::default() };
        assert!(
            bodies.incomplete(9, AnswerNeeds::Body),
            "a pruned body is incomplete for every kind"
        );
        assert_eq!(receipts.first_complete(), 41, "first complete block");
        let bodies =
            LocalRetention { earliest_body: 10, receipts_pruned: Some(4), ..Default::default() };
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

    /// P1: a complete local answer performs zero retention reads; a visibly incomplete one
    /// reads the boundary fresh even inside the TTL, and is replaced only when it is pruned.
    #[test]
    fn complete_answers_never_read_retention() {
        let cache = RetentionCache::new(Duration::from_secs(60));
        let reads = std::cell::Cell::new(0u64);
        let read = |receipts_pruned| {
            let reads = &reads;
            move || {
                reads.set(reads.get() + 1);
                LocalRetention { receipts_pruned, ..Default::default() }
            }
        };
        for number in 0..100 {
            assert!(
                !cache.replace_local(false, number, AnswerNeeds::Deposits, read(Some(1_000))),
                "complete answer {number} kept"
            );
        }
        assert_eq!(reads.get(), 0, "complete answers never read retention");
        assert!(
            !cache.replace_local(true, 50, AnswerNeeds::Deposits, read(None)),
            "incomplete but unpruned: kept"
        );
        assert_eq!(reads.get(), 2, "first call fills the cache, then reads fresh");
        assert!(
            cache.replace_local(true, 50, AnswerNeeds::Deposits, read(Some(60))),
            "pruned after the cached read: replaced"
        );
        assert_eq!(
            reads.get(),
            3,
            "the cached boundary said kept, so one fresh read inside the TTL"
        );
        assert!(
            cache.replace_local(true, 40, AnswerNeeds::Deposits, read(Some(0))),
            "cached boundary already covers 40"
        );
        assert_eq!(reads.get(), 3, "a cached pruned boundary needs no fresh read");
    }

    /// P2: pooled hashes and unpruned lookups never reach the store; a placed transaction
    /// keeps its block; only an unplaced hash with a pruned lookup goes to the archive.
    #[test]
    fn transaction_misses_route_by_pool_and_lookup() {
        let unpruned =
            LocalRetention { earliest_body: 100, receipts_pruned: Some(99), ..Default::default() };
        let pruned = LocalRetention { tx_lookup_pruned: Some(99), ..unpruned };
        assert_eq!(
            pruned.hash_miss(true, None, false),
            HashMiss::Local,
            "pooled hash stays local on a pruned node"
        );
        assert_eq!(
            pruned.hash_miss(true, Some(5), true),
            HashMiss::Local,
            "pool wins over a stale lookup"
        );
        assert_eq!(
            unpruned.hash_miss(false, None, false),
            HashMiss::Local,
            "unpruned lookup without an entry: never mined"
        );
        assert_eq!(
            unpruned.hash_miss(false, None, true),
            HashMiss::Archive,
            "indexed over a pruned body: archive"
        );
        assert_eq!(
            unpruned.hash_miss(false, Some(5), true),
            HashMiss::Mined(5),
            "a placed hash keeps its block"
        );
        assert_eq!(
            pruned.hash_miss(false, Some(5), true),
            HashMiss::Mined(5),
            "placed on a pruned node too"
        );
        assert_eq!(
            pruned.hash_miss(false, None, false),
            HashMiss::Archive,
            "an unplaced hash with a pruned lookup"
        );
    }

    /// `refresh` bypasses the TTL and seeds the cache for later `get` calls.
    #[test]
    fn refresh_bypasses_the_ttl() {
        let cache = RetentionCache::new(Duration::from_secs(60));
        assert_eq!(
            cache.get(|| LocalRetention { earliest_body: 1, ..Default::default() }).earliest_body,
            1,
            "first read"
        );
        assert_eq!(
            cache
                .refresh(|| LocalRetention { earliest_body: 5, ..Default::default() })
                .earliest_body,
            5,
            "refresh reads inside the TTL"
        );
        assert_eq!(
            cache.get(|| unreachable!("cached")).earliest_body,
            5,
            "later reads see the refreshed value"
        );
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
