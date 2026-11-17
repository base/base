//! State-keyed transaction invalidation index.
//!
//! Each pooled transaction's invalidation surfaces are known the moment it is
//! validated. This module records those surfaces as a [`WatchSet`] of
//! [`InvalidationKey`]s and maintains a reverse index from each key to the
//! transactions depending on it, so per-block invalidation is a lookup of
//! exactly the affected transactions rather than a scan of the whole pool.
//!
//! Keys carry one of two invalidation semantics:
//!
//! * **Exact-match** ([`InvalidationKey::ProtocolNonce`],
//!   [`InvalidationKey::CodeHash`], [`InvalidationKey::Slot`]): the transaction
//!   validated against a specific value; the instant that value changes the
//!   transaction is dead and is dropped unconditionally.
//! * **Threshold** ([`InvalidationKey::Balance`]): balance moves every block, so
//!   a change triggers a re-evaluation against a running reservation rather than
//!   an automatic drop. The threshold policy itself lives in the payer
//!   accounting layer; this index only reports which transactions watch a given
//!   balance.
//!
//! The index is a pure data structure: it does not decide *whether* a watched
//! transaction is still valid, only *which* transactions watch a changed key.

use std::collections::BTreeSet;

use alloy_primitives::{
    Address, B256, TxHash,
    map::{B256Set, Entry, HashMap, HashSet},
};

use crate::UnixMillis;

/// A single piece of on-chain state a pooled transaction depends on.
///
/// Two transactions that depend on the same state produce equal keys, so they
/// share a single reverse-index entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InvalidationKey {
    /// Balance of the account that pays for the transaction (the payer for an
    /// EIP-8130 sponsored transaction, otherwise the sender). Threshold
    /// semantics: a change is re-evaluated against the payer's reservation.
    Balance(Address),
    /// Protocol (EOA) nonce of an account. Exact-match semantics.
    ProtocolNonce(Address),
    /// Account bytecode hash. Exact-match semantics; used when admission relies
    /// on a sender's code still accepting its EIP-8130 delegation. The index is
    /// deliberately address-keyed: every reported code change invalidates all
    /// watchers, even if the account later returns to a previously observed
    /// hash. This permits conservative false-positive invalidation rather than
    /// retaining a transaction across an unobserved intermediate code state.
    CodeHash(Address),
    /// A raw storage slot `(contract address, slot)` — e.g. a 2D nonce-manager
    /// channel slot, or the nonce-manager slot recording a nonce-free
    /// transaction's `replay_id`. Exact-match semantics.
    Slot {
        /// Contract whose storage slot is watched.
        address: Address,
        /// Storage slot key within `address`.
        slot: B256,
    },
    /// The last block time, in Unix milliseconds, at which the transaction
    /// still holds (inclusive).
    ///
    /// A transaction's effective expiry is the minimum of its own `valid_before`
    /// bound and the expiries of the sender/payer key authorizations it relies
    /// on, all on the block's exact millisecond time. Expiry is a time surface,
    /// not a state surface: no storage diff reports it, so the pool fires the
    /// keys strictly before each canonical block's millisecond time. Occupied
    /// deadlines are kept in a sparse ordered set, so canonical timestamp jumps
    /// visit only deadlines that have transactions. A passed deadline drops
    /// every watcher, but only through [`InvalidationIndex::take_expired`]:
    /// expiry keys are not in the reverse index that `affected_exact` reads.
    Expiry(UnixMillis),
}

impl InvalidationKey {
    /// Returns `true` for keys with threshold semantics (currently only
    /// [`InvalidationKey::Balance`]), where a change must be re-evaluated rather
    /// than triggering an unconditional drop.
    #[must_use]
    pub const fn is_threshold(&self) -> bool {
        matches!(self, Self::Balance(_))
    }

    /// Returns `true` for keys with exact-match semantics, where any change to
    /// the watched state invalidates every watching transaction.
    #[must_use]
    pub const fn is_exact(&self) -> bool {
        !self.is_threshold()
    }
}

/// The set of [`InvalidationKey`]s a single transaction depends on.
///
/// Keys are de-duplicated so a transaction is registered at most once per key.
/// The container does not impose a protocol-independent capacity; callers must
/// bound key production during validation. It is backed by a `Vec` because
/// protocol-valid sets are small enough that a flat scan is cheaper than a
/// hashed container.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchSet {
    keys: Vec<InvalidationKey>,
}

impl WatchSet {
    /// Creates an empty watch set.
    #[must_use]
    pub const fn new() -> Self {
        Self { keys: Vec::new() }
    }

    /// Adds `key` to the set if not already present and returns `self`, enabling
    /// a builder-style `WatchSet::new().watch(a).watch(b)`.
    #[must_use]
    pub fn watch(mut self, key: InvalidationKey) -> Self {
        self.push(key);
        self
    }

    /// Adds `key` to the set if not already present. Returns `true` if the key
    /// was newly inserted.
    pub fn push(&mut self, key: InvalidationKey) -> bool {
        if self.keys.contains(&key) {
            return false;
        }
        self.keys.push(key);
        true
    }

    /// Returns the keys in this watch set.
    #[must_use]
    pub fn keys(&self) -> &[InvalidationKey] {
        &self.keys
    }

    /// Iterates over the keys in this watch set.
    pub fn iter(&self) -> impl Iterator<Item = &InvalidationKey> {
        self.keys.iter()
    }

    /// Returns the number of keys.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.keys.len()
    }

    /// Returns `true` if the watch set has no keys.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Returns `true` if `key` is in the set.
    #[must_use]
    pub fn contains(&self, key: &InvalidationKey) -> bool {
        self.keys.contains(key)
    }
}

/// The transactions watching one key. Most keys have a single watcher, which
/// is stored inline rather than in a set allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watchers {
    /// The key's only watcher.
    One(TxHash),
    /// Two or more watchers.
    Many(B256Set),
}

impl Watchers {
    /// Adds `hash`, switching to [`Self::Many`] when it is a second watcher.
    pub fn insert(&mut self, hash: TxHash) {
        match self {
            Self::One(one) if *one == hash => {}
            Self::One(one) => *self = Self::Many([*one, hash].into_iter().collect()),
            Self::Many(many) => {
                many.insert(hash);
            }
        }
    }

    /// Removes `hash`, returning `true` if no watcher is left, in which case
    /// the caller drops the key. A [`Self::One`] holding another hash is
    /// unchanged.
    pub fn remove(&mut self, hash: &TxHash) -> bool {
        match self {
            Self::One(one) => one == hash,
            Self::Many(many) => {
                many.remove(hash);
                many.is_empty()
            }
        }
    }

    /// Iterates the watching hashes.
    pub fn iter(&self) -> impl Iterator<Item = &TxHash> {
        let (one, many) = match self {
            Self::One(one) => (Some(one), None),
            Self::Many(many) => (None, Some(many)),
        };
        one.into_iter().chain(many.into_iter().flatten())
    }
}

/// Reverse index from [`InvalidationKey`] to the transactions watching it.
///
/// The caller keeps each transaction's [`WatchSet`] and passes it back on
/// removal, so removal is O(watch-set size). Empty key buckets are pruned on
/// removal, so the index never retains dangling entries.
#[derive(Debug, Default)]
pub struct InvalidationIndex {
    by_key: HashMap<InvalidationKey, Watchers>,
    /// `(deadline, hash)` per [`InvalidationKey::Expiry`]; never in `by_key`.
    expiries: BTreeSet<(UnixMillis, TxHash)>,
}

impl InvalidationIndex {
    /// Creates an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Upper bound on the bytes the index holds for one transaction registered
    /// with `watch`: the watch set its caller keeps and, for each key, the transaction's
    /// hash plus a key bucket in case no other transaction shares the key, and,
    /// for each [`InvalidationKey::Expiry`], an entry in the ordered deadline
    /// set instead.
    #[must_use]
    pub fn registration_size(watch: &WatchSet) -> usize {
        let watch_set = core::mem::size_of::<TxHash>()
            + core::mem::size_of::<WatchSet>()
            + core::mem::size_of_val(watch.keys());
        let per_key = core::mem::size_of::<TxHash>()
            + core::mem::size_of::<InvalidationKey>()
            + core::mem::size_of::<Watchers>();
        // B-tree nodes other than the root stay at least about half full, so an
        // entry costs under four times its own size in node storage.
        let per_expiry = 4 * core::mem::size_of::<(UnixMillis, TxHash)>();
        let expiries =
            watch.keys().iter().filter(|key| matches!(key, InvalidationKey::Expiry(_))).count();
        watch_set + (watch.len() - expiries) * per_key + expiries * per_expiry
    }

    /// Registers `hash` under every key in `watch`. The caller keeps `watch` and
    /// passes it back to [`Self::remove`].
    pub fn insert(&mut self, hash: TxHash, watch: &WatchSet) {
        for key in watch.keys() {
            if let InvalidationKey::Expiry(deadline) = key {
                self.expiries.insert((*deadline, hash));
            } else {
                match self.by_key.entry(*key) {
                    Entry::Occupied(mut watchers) => watchers.get_mut().insert(hash),
                    Entry::Vacant(slot) => {
                        slot.insert(Watchers::One(hash));
                    }
                }
            }
        }
    }

    /// Unregisters `hash` from every key in `watch`, the set it was inserted
    /// with, pruning any key left with no watcher.
    pub fn remove(&mut self, hash: &TxHash, watch: &WatchSet) {
        for key in watch.keys() {
            if let InvalidationKey::Expiry(deadline) = key {
                self.expiries.remove(&(*deadline, *hash));
            } else if self.by_key.get_mut(key).is_some_and(|watchers| watchers.remove(hash)) {
                self.by_key.remove(key);
            }
        }
    }

    /// Returns the transactions watching `key`.
    pub fn watchers(&self, key: &InvalidationKey) -> impl Iterator<Item = &TxHash> {
        self.by_key.get(key).into_iter().flat_map(Watchers::iter)
    }

    /// Number of distinct keys with at least one watcher.
    #[must_use]
    pub fn key_count(&self) -> usize {
        self.by_key.len()
    }

    /// Unregisters and returns, in deadline order, the expiry registrations
    /// passed at block time `now`; their other keys stay registered.
    pub fn take_expired(&mut self, now: UnixMillis) -> Vec<(UnixMillis, TxHash)> {
        let live = self.expiries.split_off(&(now, TxHash::ZERO));
        core::mem::replace(&mut self.expiries, live).into_iter().collect()
    }

    /// Collects every transaction that must be dropped because one of the given
    /// **exact-match** keys changed. Threshold keys (e.g.
    /// [`InvalidationKey::Balance`]) in `changed` are ignored, since their
    /// invalidation is decided by the payer-accounting layer, not by the mere
    /// fact of a change.
    pub fn affected_exact<I>(&self, changed: I) -> HashSet<TxHash>
    where
        I: IntoIterator<Item = InvalidationKey>,
    {
        let mut affected = HashSet::default();
        for key in changed {
            debug_assert!(
                !matches!(key, InvalidationKey::Expiry(_)),
                "expiry fires via take_expired"
            );
            if key.is_exact() {
                affected.extend(self.watchers(&key).copied());
            }
        }
        affected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(byte: u8) -> Address {
        Address::repeat_byte(byte)
    }

    fn hash(byte: u8) -> TxHash {
        TxHash::repeat_byte(byte)
    }

    #[test]
    fn balance_is_threshold_others_are_exact() {
        assert!(InvalidationKey::Balance(addr(1)).is_threshold());
        assert!(!InvalidationKey::Balance(addr(1)).is_exact());

        assert!(InvalidationKey::ProtocolNonce(addr(1)).is_exact());
        assert!(InvalidationKey::Slot { address: addr(1), slot: B256::repeat_byte(2) }.is_exact());
        assert!(InvalidationKey::Expiry(UnixMillis::from_millis(5)).is_exact());
    }

    #[test]
    fn watch_set_dedups_keys() {
        let key = InvalidationKey::Balance(addr(1));
        let watch = WatchSet::new().watch(key).watch(key);
        assert_eq!(watch.len(), 1);
        assert!(watch.contains(&key));

        let mut watch =
            WatchSet::new().watch(key).watch(key).watch(InvalidationKey::ProtocolNonce(addr(1)));
        assert_eq!(watch.len(), 2);
        assert!(!watch.push(key));
        assert_eq!(watch.len(), 2);
    }

    #[test]
    fn insert_and_remove_roundtrip() {
        let mut index = InvalidationIndex::new();
        let tx = hash(1);
        let balance = InvalidationKey::Balance(addr(1));
        let nonce = InvalidationKey::ProtocolNonce(addr(1));
        let watch = WatchSet::new().watch(balance).watch(nonce);

        index.insert(tx, &watch);
        assert_eq!(index.key_count(), 2);
        assert_eq!(index.watchers(&nonce).collect::<Vec<_>>(), [&tx]);

        index.remove(&tx, &watch);
        assert_eq!(index.key_count(), 0, "empty key buckets must be pruned");
    }

    #[test]
    fn watchers_grow_to_many_and_report_when_empty() {
        let mut watchers = Watchers::One(hash(1));
        watchers.insert(hash(1));
        assert_eq!(watchers, Watchers::One(hash(1)));
        assert!(!watchers.remove(&hash(2)), "another hash leaves a lone watcher in place");

        watchers.insert(hash(2));
        assert!(matches!(watchers, Watchers::Many(_)));
        assert!(!watchers.remove(&hash(1)));
        assert!(watchers.remove(&hash(2)));
    }

    #[test]
    fn multiple_txs_share_a_key() {
        let mut index = InvalidationIndex::new();
        let balance = InvalidationKey::Balance(addr(1));
        let (a, b) = (hash(1), hash(2));

        let watch = WatchSet::new().watch(balance);
        index.insert(a, &watch);
        index.insert(b, &watch);

        let watchers: HashSet<_> = index.watchers(&balance).copied().collect();
        assert_eq!(watchers, [a, b].into_iter().collect());

        // Removing one leaves the other registered under the shared key.
        index.remove(&a, &watch);
        assert_eq!(index.watchers(&balance).collect::<Vec<_>>(), [&b]);
        assert_eq!(index.key_count(), 1);
        index.remove(&b, &watch);
        assert_eq!(index.key_count(), 0, "empty key buckets must be pruned");
    }

    #[test]
    fn registration_size_charges_each_expiry_deadline() {
        let base = WatchSet::new().watch(InvalidationKey::ProtocolNonce(addr(1)));
        let with_expiry = base.clone().watch(InvalidationKey::Expiry(UnixMillis::from_millis(7)));

        // An expiry key costs its stored key plus an ordered-set entry.
        assert_eq!(
            InvalidationIndex::registration_size(&with_expiry),
            InvalidationIndex::registration_size(&base)
                + core::mem::size_of::<InvalidationKey>()
                + 4 * core::mem::size_of::<(UnixMillis, TxHash)>(),
        );
    }

    #[test]
    fn take_expired_is_sparse_across_large_timestamp_gaps() {
        let mut index = InvalidationIndex::new();
        let ms = UnixMillis::from_millis;
        for (byte, deadline) in [(1, 1), (2, 1_700_000_000_000), (3, 1_800_000_000_000)] {
            index.insert(hash(byte), &WatchSet::new().watch(InvalidationKey::Expiry(ms(deadline))));
        }

        assert_eq!(
            index.take_expired(ms(1_700_000_000_001)),
            vec![(ms(1), hash(1)), (ms(1_700_000_000_000), hash(2))]
        );
    }

    #[test]
    fn expiry_fires_only_once_the_deadline_has_passed() {
        let mut index = InvalidationIndex::new();
        let ms = UnixMillis::from_millis;
        let deadline = 1_700_000_000_300;
        index.insert(hash(1), &WatchSet::new().watch(InvalidationKey::Expiry(ms(deadline))));

        // Still valid at the deadline millisecond itself.
        assert!(index.take_expired(ms(deadline - 100)).is_empty());
        assert!(index.take_expired(ms(deadline)).is_empty());
        assert_eq!(index.take_expired(ms(deadline + 1)), vec![(ms(deadline), hash(1))]);
    }

    #[test]
    fn removing_a_watcher_unregisters_its_deadline() {
        let mut index = InvalidationIndex::new();
        let deadline = InvalidationKey::Expiry(UnixMillis::from_millis(7));
        let (first, second) = (hash(1), hash(2));

        let watch = WatchSet::new().watch(deadline);
        index.insert(first, &watch);
        index.insert(second, &watch);
        index.remove(&first, &watch);
        assert_eq!(
            index.take_expired(UnixMillis::from_millis(8)),
            vec![(UnixMillis::from_millis(7), second)]
        );
    }

    #[test]
    fn affected_exact_collects_exact_keys_and_ignores_balance() {
        let mut index = InvalidationIndex::new();
        let balance = InvalidationKey::Balance(addr(9));
        let nonce = InvalidationKey::ProtocolNonce(addr(9));
        let slot = InvalidationKey::Slot { address: addr(9), slot: B256::repeat_byte(3) };

        let on_balance = hash(1);
        let on_nonce = hash(2);
        let on_slot = hash(3);
        let on_balance_and_slot = hash(4);

        index.insert(on_balance, &WatchSet::new().watch(balance));
        index.insert(on_nonce, &WatchSet::new().watch(nonce));
        index.insert(on_slot, &WatchSet::new().watch(slot));
        index.insert(on_balance_and_slot, &WatchSet::new().watch(balance).watch(slot));

        // A block that changed the balance and the slot: only exact-match (slot)
        // watchers are returned; pure balance watchers are left for threshold
        // re-evaluation.
        let affected = index.affected_exact([balance, slot]);
        assert_eq!(affected.len(), 2);
        assert!(affected.contains(&on_slot));
        assert!(affected.contains(&on_balance_and_slot));
        assert!(!affected.contains(&on_balance));
        assert!(!affected.contains(&on_nonce));
    }

    #[test]
    fn affected_exact_dedups_across_keys() {
        let mut index = InvalidationIndex::new();
        let nonce = InvalidationKey::ProtocolNonce(addr(1));
        let slot = InvalidationKey::Slot { address: addr(1), slot: B256::repeat_byte(1) };
        let tx = hash(1);

        index.insert(tx, &WatchSet::new().watch(nonce).watch(slot));

        // The tx watches two changed keys but must appear once.
        let affected = index.affected_exact([nonce, slot]);
        assert_eq!(affected.len(), 1);
        assert!(affected.contains(&tx));
    }

    #[test]
    fn removing_an_unregistered_hash_is_a_no_op() {
        let mut index = InvalidationIndex::new();
        let watch = WatchSet::new().watch(InvalidationKey::ProtocolNonce(addr(1)));
        index.insert(hash(1), &watch);
        index.remove(&hash(2), &watch);
        assert_eq!(
            index.watchers(&InvalidationKey::ProtocolNonce(addr(1))).collect::<Vec<_>>(),
            [&hash(1)]
        );
    }
}
