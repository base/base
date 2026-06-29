//! Per-account admission limits: payer balance accounting and inflight counters.
//!
//! Two independent dimensions gate admission (see `MEMPOOL_HARDENING.md`):
//!
//! * the **sender limit**, bounded by signature/auth-surface stability, and
//! * the **payer limit**, bounded by balance-drain risk.
//!
//! Both are mechanisms here; the *policy* that picks a sender's cap or routes a
//! payer to count-based vs balance-based limiting (locked / trusted-bytecode
//! classification) lives in a later layer.
//!
//! [`InflightCounters`] backs the count-based dimension (default cap, and the
//! "unlimited" sender cap for locked accounts). [`PayerBook`] backs the
//! balance-bounded payer dimension used by trusted accounts, where the sum of
//! the max cost of all sponsored transactions must not exceed the payer's
//! balance. For a trusted account ETH can only leave through gas it sponsors, so
//! `Σ max_cost ≤ balance` is a true upper bound on the only drain.

use std::collections::{BTreeMap, HashMap};

use alloy_primitives::{Address, TxHash, U256};

/// Per-account inflight transaction counters with a caller-supplied cap.
///
/// The counter is dimension-agnostic: callers use one instance for the sender
/// dimension and another for the count-based payer dimension, passing the
/// appropriate cap (e.g. `u32::MAX` for a locked account's unlimited sender
/// limit).
#[derive(Debug, Default)]
pub struct InflightCounters {
    counts: HashMap<Address, u32>,
}

impl InflightCounters {
    /// Creates empty counters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the current inflight count for `account`.
    #[must_use]
    pub fn get(&self, account: &Address) -> u32 {
        self.counts.get(account).copied().unwrap_or(0)
    }

    /// Attempts to reserve one slot for `account` against `cap`. Returns `true`
    /// and increments the count if the account is below `cap`; returns `false`
    /// and leaves the count unchanged otherwise.
    pub fn try_increment(&mut self, account: Address, cap: u32) -> bool {
        let entry = self.counts.entry(account).or_insert(0);
        if *entry >= cap {
            return false;
        }
        *entry += 1;
        true
    }

    /// Releases one slot for `account`, saturating at zero. Prunes the entry
    /// when it reaches zero so idle accounts do not retain memory.
    pub fn decrement(&mut self, account: Address) {
        if let Some(entry) = self.counts.get_mut(&account) {
            *entry = entry.saturating_sub(1);
            if *entry == 0 {
                self.counts.remove(&account);
            }
        }
    }

    /// Number of accounts with at least one inflight transaction.
    #[must_use]
    pub fn tracked_accounts(&self) -> usize {
        self.counts.len()
    }
}

/// Aggregate balance accounting for a single balance-bounded (trusted) payer.
///
/// Tracks the payer's cached balance, the reserved sum of sponsored
/// transactions' max cost, and the sponsored transactions ordered by ascending
/// priority so the least valuable are evicted first when the balance can no
/// longer cover the reservation.
#[derive(Debug)]
pub struct PayerBook {
    balance: U256,
    reserved: U256,
    /// Ordered by `(priority, hash)` ascending; the front is evicted first.
    by_priority: BTreeMap<(u128, TxHash), U256>,
    /// `hash -> (priority, max_cost)` for O(1) removal and idempotency checks.
    entries: HashMap<TxHash, (u128, U256)>,
}

impl PayerBook {
    /// Creates a payer book with the given current balance and no reservations.
    #[must_use]
    pub fn new(balance: U256) -> Self {
        Self {
            balance,
            reserved: U256::ZERO,
            by_priority: BTreeMap::new(),
            entries: HashMap::new(),
        }
    }

    /// Current cached balance.
    #[must_use]
    pub const fn balance(&self) -> U256 {
        self.balance
    }

    /// Sum of the max cost of all currently reserved transactions.
    #[must_use]
    pub const fn reserved(&self) -> U256 {
        self.reserved
    }

    /// Balance not yet committed to reservations (saturating at zero).
    #[must_use]
    pub const fn available(&self) -> U256 {
        self.balance.saturating_sub(self.reserved)
    }

    /// Number of reserved transactions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if no transactions are reserved.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns `true` if `hash` is reserved in this book.
    #[must_use]
    pub fn contains(&self, hash: &TxHash) -> bool {
        self.entries.contains_key(hash)
    }

    /// Attempts to reserve `max_cost` for `hash` at `priority`. Succeeds (and is
    /// a no-op returning `true`) if `hash` is already reserved. Otherwise
    /// succeeds only if the new reservation still fits within the balance:
    /// `reserved + max_cost ≤ balance`.
    pub fn try_reserve(&mut self, hash: TxHash, max_cost: U256, priority: u128) -> bool {
        if self.entries.contains_key(&hash) {
            return true;
        }
        let Some(new_reserved) = self.reserved.checked_add(max_cost) else {
            return false;
        };
        if new_reserved > self.balance {
            return false;
        }
        self.by_priority.insert((priority, hash), max_cost);
        self.entries.insert(hash, (priority, max_cost));
        self.reserved = new_reserved;
        true
    }

    /// Releases the reservation for `hash`, if present. Returns `true` if a
    /// reservation was removed.
    pub fn remove(&mut self, hash: &TxHash) -> bool {
        let Some((priority, max_cost)) = self.entries.remove(hash) else {
            return false;
        };
        self.by_priority.remove(&(priority, *hash));
        self.reserved = self.reserved.saturating_sub(max_cost);
        true
    }

    /// Updates the cached balance and, if it can no longer cover the
    /// reservation, evicts transactions from the low-priority end until
    /// `reserved ≤ balance`. Returns the evicted hashes in eviction order
    /// (lowest priority first).
    pub fn set_balance(&mut self, balance: U256) -> Vec<TxHash> {
        self.balance = balance;
        let mut evicted = Vec::new();
        while self.reserved > self.balance {
            let Some((&(priority, hash), &max_cost)) = self.by_priority.iter().next() else {
                break;
            };
            self.by_priority.remove(&(priority, hash));
            self.entries.remove(&hash);
            self.reserved = self.reserved.saturating_sub(max_cost);
            evicted.push(hash);
        }
        evicted
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
    fn counters_enforce_cap() {
        let mut counters = InflightCounters::new();
        let account = addr(1);

        assert!(counters.try_increment(account, 2));
        assert!(counters.try_increment(account, 2));
        assert_eq!(counters.get(&account), 2);
        assert!(!counters.try_increment(account, 2), "at cap");
        assert_eq!(counters.get(&account), 2, "rejected increment must not bump count");
    }

    #[test]
    fn counters_decrement_prunes_at_zero() {
        let mut counters = InflightCounters::new();
        let account = addr(1);

        counters.try_increment(account, 4);
        counters.decrement(account);
        assert_eq!(counters.get(&account), 0);
        assert_eq!(counters.tracked_accounts(), 0, "idle account must be pruned");

        // Decrement below zero is saturating and safe.
        counters.decrement(account);
        assert_eq!(counters.get(&account), 0);
    }

    #[test]
    fn unlimited_cap_admits_freely() {
        let mut counters = InflightCounters::new();
        let account = addr(1);
        for _ in 0..1000 {
            assert!(counters.try_increment(account, u32::MAX));
        }
        assert_eq!(counters.get(&account), 1000);
    }

    #[test]
    fn payer_book_reserves_within_balance() {
        let mut book = PayerBook::new(U256::from(100u64));

        assert!(book.try_reserve(hash(1), U256::from(60u64), 10));
        assert!(book.try_reserve(hash(2), U256::from(40u64), 10));
        assert_eq!(book.reserved(), U256::from(100u64));
        assert_eq!(book.available(), U256::ZERO);

        // Exceeding the balance is rejected and leaves state unchanged.
        assert!(!book.try_reserve(hash(3), U256::from(1u64), 10));
        assert_eq!(book.len(), 2);
        assert_eq!(book.reserved(), U256::from(100u64));
    }

    #[test]
    fn payer_book_reserve_is_idempotent() {
        let mut book = PayerBook::new(U256::from(100u64));
        assert!(book.try_reserve(hash(1), U256::from(60u64), 10));
        // Re-reserving the same hash is a no-op success, not double counting.
        assert!(book.try_reserve(hash(1), U256::from(60u64), 10));
        assert_eq!(book.reserved(), U256::from(60u64));
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn payer_book_remove_releases_reservation() {
        let mut book = PayerBook::new(U256::from(100u64));
        book.try_reserve(hash(1), U256::from(60u64), 10);
        book.try_reserve(hash(2), U256::from(30u64), 10);

        assert!(book.remove(&hash(1)));
        assert_eq!(book.reserved(), U256::from(30u64));
        assert_eq!(book.available(), U256::from(70u64));
        assert!(!book.remove(&hash(1)), "double remove is a no-op");
    }

    #[test]
    fn set_balance_evicts_lowest_priority_first() {
        let mut book = PayerBook::new(U256::from(100u64));
        // Three equal-cost txs with distinct priorities.
        book.try_reserve(hash(1), U256::from(30u64), 5); // lowest priority
        book.try_reserve(hash(2), U256::from(30u64), 15); // highest priority
        book.try_reserve(hash(3), U256::from(30u64), 10); // middle
        assert_eq!(book.reserved(), U256::from(90u64));

        // Balance drops to 50: must evict until reserved ≤ 50, dropping the two
        // lowest-priority txs (prio 5 then 10), keeping the highest (prio 15).
        let evicted = book.set_balance(U256::from(50u64));
        assert_eq!(evicted, vec![hash(1), hash(3)]);
        assert_eq!(book.reserved(), U256::from(30u64));
        assert!(book.contains(&hash(2)));
        assert!(!book.contains(&hash(1)));
        assert!(!book.contains(&hash(3)));
    }

    #[test]
    fn set_balance_increase_evicts_nothing() {
        let mut book = PayerBook::new(U256::from(100u64));
        book.try_reserve(hash(1), U256::from(90u64), 5);
        let evicted = book.set_balance(U256::from(1_000u64));
        assert!(evicted.is_empty());
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn set_balance_to_zero_evicts_all() {
        let mut book = PayerBook::new(U256::from(100u64));
        book.try_reserve(hash(1), U256::from(40u64), 5);
        book.try_reserve(hash(2), U256::from(40u64), 9);
        let evicted = book.set_balance(U256::ZERO);
        assert_eq!(evicted.len(), 2);
        assert!(book.is_empty());
        assert_eq!(book.reserved(), U256::ZERO);
    }
}
