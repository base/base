//! State-keyed transaction invalidation index.
//!
//! Each pooled transaction's invalidation surfaces are known the moment it is
//! validated (see `MEMPOOL_HARDENING.md`). This module records those surfaces as
//! a [`WatchSet`] of [`InvalidationKey`]s and maintains a reverse index from
//! each key to the transactions depending on it, so per-block invalidation is a
//! lookup of exactly the affected transactions rather than a scan of the whole
//! pool.
//!
//! Keys carry one of two invalidation semantics:
//!
//! * **Exact-match** ([`InvalidationKey::ProtocolNonce`],
//!   [`InvalidationKey::Slot`]): the transaction validated against a specific
//!   value; the instant that value changes the transaction is dead and is
//!   dropped unconditionally.
//! * **Threshold** ([`InvalidationKey::Balance`]): balance moves every block, so
//!   a change triggers a re-evaluation against a running reservation rather than
//!   an automatic drop. The threshold policy itself lives in the payer
//!   accounting layer; this index only reports which transactions watch a given
//!   balance.
//!
//! The index is a pure data structure: it does not decide *whether* a watched
//! transaction is still valid, only *which* transactions watch a changed key.

use std::collections::{HashMap, HashSet};

use alloy_primitives::{Address, B256, TxHash};

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
    /// A raw storage slot `(contract address, slot)` — e.g. an actor-config or
    /// account-state/lock slot in the EIP-8130 `AccountConfiguration` contract,
    /// or a 2D nonce-manager channel slot. Exact-match semantics.
    Slot {
        /// Contract whose storage slot is watched.
        address: Address,
        /// Storage slot key within `address`.
        slot: B256,
    },
    /// A coarse wall-clock expiry bucket (`effective_expiry / EXPIRY_BUCKET_SECS`).
    ///
    /// A transaction's effective expiry is the minimum of its own `expiry` and
    /// the expiries of the sender/payer key authorizations it relies on. Expiry
    /// is a time surface, not a state surface: no storage diff reports it, so the
    /// pool fires the due bucket(s) each block. Bucketing avoids an ordered
    /// structure — insertion is O(1) and each block fires only the newly-due
    /// bucket(s). Exact-match semantics (a fired bucket drops every member).
    ExpiryBucket(u64),
}

impl InvalidationKey {
    /// Width, in seconds, of an [`InvalidationKey::ExpiryBucket`] chunk —
    /// approximately one block-time. Coarse enough that each block fires only the
    /// newly-due bucket(s); fine enough that eviction is at most ~one chunk early
    /// under the proactive (one-block-ahead) firing policy.
    pub const EXPIRY_BUCKET_SECS: u64 = 2;

    /// Returns the [`InvalidationKey::ExpiryBucket`] for an absolute Unix-seconds
    /// expiry timestamp.
    #[must_use]
    pub const fn expiry_bucket(expiry_secs: u64) -> Self {
        Self::ExpiryBucket(expiry_secs / Self::EXPIRY_BUCKET_SECS)
    }

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

/// The bounded set of [`InvalidationKey`]s a single transaction depends on.
///
/// Keys are de-duplicated so a transaction is registered at most once per key.
/// Backed by a `Vec`; the set is small (≤ ~7 for EIP-8130) so a flat scan on
/// insertion is cheaper than a hashed container.
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

    /// Builds a watch set from an iterator of keys, de-duplicating as it goes.
    pub fn from_keys<I>(keys: I) -> Self
    where
        I: IntoIterator<Item = InvalidationKey>,
    {
        let mut set = Self::new();
        for key in keys {
            set.push(key);
        }
        set
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

impl<'a> IntoIterator for &'a WatchSet {
    type Item = &'a InvalidationKey;
    type IntoIter = std::slice::Iter<'a, InvalidationKey>;

    fn into_iter(self) -> Self::IntoIter {
        self.keys.iter()
    }
}

/// Reverse index from [`InvalidationKey`] to the transactions watching it.
///
/// Each transaction's [`WatchSet`] is stored so removal is O(watch-set size)
/// and needs only the transaction hash. Empty key buckets are pruned on
/// removal, so the index never retains dangling entries.
#[derive(Debug, Default)]
pub struct InvalidationIndex {
    by_key: HashMap<InvalidationKey, HashSet<TxHash>>,
    watch_sets: HashMap<TxHash, WatchSet>,
}

impl InvalidationIndex {
    /// Creates an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `hash` under every key in `watch`. If `hash` is already present
    /// its previous watch set is removed first, so the index always reflects the
    /// latest registration. Returns `true` if this was a fresh insertion (i.e.
    /// the hash was not previously registered).
    pub fn insert(&mut self, hash: TxHash, watch: WatchSet) -> bool {
        let was_present = self.remove(&hash).is_some();
        for key in watch.keys() {
            self.by_key.entry(*key).or_default().insert(hash);
        }
        self.watch_sets.insert(hash, watch);
        !was_present
    }

    /// Removes `hash` from the index, unlinking it from each of its watched
    /// keys and pruning any key bucket left empty. Returns the transaction's
    /// watch set if it was present.
    pub fn remove(&mut self, hash: &TxHash) -> Option<WatchSet> {
        let watch = self.watch_sets.remove(hash)?;
        for key in watch.keys() {
            if let Some(bucket) = self.by_key.get_mut(key) {
                bucket.remove(hash);
                if bucket.is_empty() {
                    self.by_key.remove(key);
                }
            }
        }
        Some(watch)
    }

    /// Returns `true` if `hash` is registered.
    #[must_use]
    pub fn contains(&self, hash: &TxHash) -> bool {
        self.watch_sets.contains_key(hash)
    }

    /// Returns the watch set registered for `hash`, if any.
    #[must_use]
    pub fn watch_set(&self, hash: &TxHash) -> Option<&WatchSet> {
        self.watch_sets.get(hash)
    }

    /// Returns the set of transactions watching `key`, if any.
    #[must_use]
    pub fn watchers(&self, key: &InvalidationKey) -> Option<&HashSet<TxHash>> {
        self.by_key.get(key)
    }

    /// Number of registered transactions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.watch_sets.len()
    }

    /// Returns `true` if no transactions are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.watch_sets.is_empty()
    }

    /// Number of distinct keys with at least one watcher.
    #[must_use]
    pub fn key_count(&self) -> usize {
        self.by_key.len()
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
        let mut affected = HashSet::new();
        for key in changed {
            if key.is_exact()
                && let Some(bucket) = self.by_key.get(&key)
            {
                affected.extend(bucket.iter().copied());
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
        assert!(InvalidationKey::ExpiryBucket(5).is_exact());
    }

    #[test]
    fn expiry_bucket_quantizes_by_chunk() {
        let chunk = InvalidationKey::EXPIRY_BUCKET_SECS;
        // Expiries within the same chunk share a bucket; the next chunk differs.
        assert_eq!(InvalidationKey::expiry_bucket(0), InvalidationKey::ExpiryBucket(0));
        assert_eq!(InvalidationKey::expiry_bucket(chunk - 1), InvalidationKey::expiry_bucket(0));
        assert_eq!(InvalidationKey::expiry_bucket(chunk), InvalidationKey::ExpiryBucket(1));
        assert_ne!(InvalidationKey::expiry_bucket(0), InvalidationKey::expiry_bucket(chunk));
    }

    #[test]
    fn watch_set_dedups_keys() {
        let key = InvalidationKey::Balance(addr(1));
        let watch = WatchSet::new().watch(key).watch(key);
        assert_eq!(watch.len(), 1);
        assert!(watch.contains(&key));

        let mut watch = WatchSet::from_keys([key, key, InvalidationKey::ProtocolNonce(addr(1))]);
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

        assert!(index.insert(tx, watch.clone()));
        assert!(index.contains(&tx));
        assert_eq!(index.len(), 1);
        assert_eq!(index.key_count(), 2);
        assert_eq!(index.watch_set(&tx), Some(&watch));

        let removed = index.remove(&tx);
        assert_eq!(removed, Some(watch));
        assert!(!index.contains(&tx));
        assert!(index.is_empty());
        assert_eq!(index.key_count(), 0, "empty key buckets must be pruned");
    }

    #[test]
    fn multiple_txs_share_a_key() {
        let mut index = InvalidationIndex::new();
        let balance = InvalidationKey::Balance(addr(1));
        let (a, b) = (hash(1), hash(2));

        index.insert(a, WatchSet::new().watch(balance));
        index.insert(b, WatchSet::new().watch(balance));

        let watchers = index.watchers(&balance).expect("bucket exists");
        assert_eq!(watchers.len(), 2);
        assert!(watchers.contains(&a));
        assert!(watchers.contains(&b));

        // Removing one leaves the other registered under the shared key.
        index.remove(&a);
        let watchers = index.watchers(&balance).expect("bucket still exists");
        assert_eq!(watchers.len(), 1);
        assert!(watchers.contains(&b));
    }

    #[test]
    fn reinsert_replaces_previous_watch_set() {
        let mut index = InvalidationIndex::new();
        let tx = hash(1);
        let old_slot = InvalidationKey::Slot { address: addr(1), slot: B256::repeat_byte(1) };
        let new_slot = InvalidationKey::Slot { address: addr(1), slot: B256::repeat_byte(2) };

        assert!(index.insert(tx, WatchSet::new().watch(old_slot)));
        // Re-insertion with a different watch set is not a fresh insert.
        assert!(!index.insert(tx, WatchSet::new().watch(new_slot)));

        assert!(index.watchers(&old_slot).is_none(), "stale key must be unlinked");
        assert!(index.watchers(&new_slot).expect("new key linked").contains(&tx));
        assert_eq!(index.len(), 1);
        assert_eq!(index.key_count(), 1);
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

        index.insert(on_balance, WatchSet::new().watch(balance));
        index.insert(on_nonce, WatchSet::new().watch(nonce));
        index.insert(on_slot, WatchSet::new().watch(slot));
        index.insert(on_balance_and_slot, WatchSet::new().watch(balance).watch(slot));

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

        index.insert(tx, WatchSet::new().watch(nonce).watch(slot));

        // The tx watches two changed keys but must appear once.
        let affected = index.affected_exact([nonce, slot]);
        assert_eq!(affected.len(), 1);
        assert!(affected.contains(&tx));
    }

    #[test]
    fn remove_absent_hash_is_none() {
        let mut index = InvalidationIndex::new();
        assert!(index.remove(&hash(1)).is_none());
    }
}
