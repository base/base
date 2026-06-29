//! Mempool admission and invalidation bookkeeping.
//!
//! [`MempoolGuard`] ties together the three mechanisms built in
//! [`crate::invalidation`] and [`crate::limits`] into the policy described in
//! `MEMPOOL_HARDENING.md`:
//!
//! * the dual **sender / payer** admission check, and
//! * state-keyed **invalidation** (exact-match slot drops and balance
//!   threshold/aggregate re-evaluation).
//!
//! It is a pure in-memory ledger: it never reads chain state. Callers supply the
//! per-transaction classification ([`Admission`]) computed during validation,
//! and feed it changed storage slots and account balances from the block diff
//! stream. The guard returns the hashes it has dropped so the pool can evict the
//! corresponding transactions.

use std::collections::HashMap;

use alloy_primitives::{Address, TxHash, U256};

use crate::{InflightCounters, InvalidationIndex, InvalidationKey, PayerBook, WatchSet};

/// Default cap on inflight transactions per account on each dimension, applied
/// to accounts without an elevated classification.
pub const DEFAULT_SENDER_LIMIT: u32 = 4;
/// Default cap on inflight sponsored transactions per (count-limited) payer.
pub const DEFAULT_PAYER_LIMIT: u32 = 4;

/// Configurable per-account admission caps.
#[derive(Debug, Clone, Copy)]
pub struct GuardLimits {
    /// Cap on inflight transactions naming an account as sender, for accounts
    /// that are not locked. Locked accounts have an unlimited sender dimension.
    pub default_sender: u32,
    /// Cap on inflight sponsored transactions for a count-limited payer, for
    /// payers that are not balance-bounded (trusted).
    pub default_payer: u32,
}

impl Default for GuardLimits {
    fn default() -> Self {
        Self { default_sender: DEFAULT_SENDER_LIMIT, default_payer: DEFAULT_PAYER_LIMIT }
    }
}

/// Why an admission was rejected by the limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitRejection {
    /// The sender's inflight limit is reached.
    SenderLimit,
    /// A count-limited payer's inflight limit is reached.
    PayerLimit,
    /// A balance-bounded payer cannot afford another reservation.
    PayerBalance,
}

/// A validated transaction presented for admission, carrying the classification
/// resolved during validation.
#[derive(Debug, Clone)]
pub struct Admission {
    /// Transaction hash.
    pub hash: TxHash,
    /// Resolved sender account.
    pub sender: Address,
    /// Resolved payer account (equals `sender` for self-paying transactions).
    pub payer: Address,
    /// Whether the sender's owner config is locked (stable auth surface ⇒
    /// unlimited sender dimension).
    pub sender_locked: bool,
    /// Whether the payer is balance-bounded (locked + trusted bytecode ⇒ the
    /// payer dimension is limited by balance rather than a count).
    pub payer_trusted: bool,
    /// The payer's current balance, used to seed a balance-bounded payer book.
    pub payer_balance: U256,
    /// The maximum cost this transaction can charge the payer.
    pub max_cost: U256,
    /// Effective-tip priority used for balance-bounded eviction ordering.
    pub priority: u128,
    /// The invalidation surfaces this transaction depends on.
    pub watch_set: WatchSet,
}

/// How a transaction was charged against the limits, retained so removal can
/// release exactly the dimensions it consumed.
#[derive(Debug, Clone, Copy)]
struct AdmissionRecord {
    sender: Address,
    payer: Address,
    payer_trusted: bool,
    max_cost: U256,
}

/// In-memory admission and invalidation ledger for the pool.
#[derive(Debug)]
pub struct MempoolGuard {
    index: InvalidationIndex,
    sender_counts: InflightCounters,
    payer_counts: InflightCounters,
    payer_books: HashMap<Address, PayerBook>,
    records: HashMap<TxHash, AdmissionRecord>,
    limits: GuardLimits,
}

impl MempoolGuard {
    /// Creates a guard with the given limits.
    #[must_use]
    pub fn new(limits: GuardLimits) -> Self {
        Self {
            index: InvalidationIndex::new(),
            sender_counts: InflightCounters::new(),
            payer_counts: InflightCounters::new(),
            payer_books: HashMap::new(),
            records: HashMap::new(),
            limits,
        }
    }

    /// Number of transactions currently tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Returns `true` if no transactions are tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Returns `true` if `hash` is tracked.
    #[must_use]
    pub fn contains(&self, hash: &TxHash) -> bool {
        self.records.contains_key(hash)
    }

    /// Applies the dual sender/payer admission check and, on success, registers
    /// the transaction's watch set and charges both limit dimensions. A
    /// transaction already tracked is accepted idempotently.
    ///
    /// On rejection no state is mutated (any partial reservation is rolled
    /// back), so the caller may safely drop the transaction.
    pub fn try_admit(&mut self, admission: Admission) -> Result<(), LimitRejection> {
        if self.records.contains_key(&admission.hash) {
            return Ok(());
        }

        let sender_cap =
            if admission.sender_locked { u32::MAX } else { self.limits.default_sender };
        if !self.sender_counts.try_increment(admission.sender, sender_cap) {
            return Err(LimitRejection::SenderLimit);
        }

        let payer_ok = if admission.payer_trusted {
            let book = self
                .payer_books
                .entry(admission.payer)
                .or_insert_with(|| PayerBook::new(admission.payer_balance));
            book.try_reserve(admission.hash, admission.max_cost, admission.priority)
        } else {
            self.payer_counts.try_increment(admission.payer, self.limits.default_payer)
        };

        if !payer_ok {
            // Roll back the sender reservation so a payer rejection leaves no
            // trace.
            self.sender_counts.decrement(admission.sender);
            // A freshly created, now-empty payer book is pruned to avoid leaking
            // an entry for a payer that never successfully reserved.
            if admission.payer_trusted
                && self.payer_books.get(&admission.payer).is_some_and(PayerBook::is_empty)
            {
                self.payer_books.remove(&admission.payer);
            }
            return Err(if admission.payer_trusted {
                LimitRejection::PayerBalance
            } else {
                LimitRejection::PayerLimit
            });
        }

        self.records.insert(
            admission.hash,
            AdmissionRecord {
                sender: admission.sender,
                payer: admission.payer,
                payer_trusted: admission.payer_trusted,
                max_cost: admission.max_cost,
            },
        );
        self.index.insert(admission.hash, admission.watch_set);
        Ok(())
    }

    /// Releases all bookkeeping for `hash` (limits and index). Returns `true` if
    /// the transaction was tracked. Used both for normal removal (mined,
    /// replaced) and as the internal step of invalidation.
    pub fn release(&mut self, hash: &TxHash) -> bool {
        let Some(record) = self.records.remove(hash) else {
            return false;
        };
        self.sender_counts.decrement(record.sender);
        if record.payer_trusted {
            if let Some(book) = self.payer_books.get_mut(&record.payer) {
                book.remove(hash);
                if book.is_empty() {
                    self.payer_books.remove(&record.payer);
                }
            }
        } else {
            self.payer_counts.decrement(record.payer);
        }
        self.index.remove(hash);
        true
    }

    /// Invalidates every transaction that watches one of the changed
    /// **exact-match** keys (e.g. actor-config or account-state slots). Returns
    /// the dropped hashes.
    pub fn invalidate_exact<I>(&mut self, changed: I) -> Vec<TxHash>
    where
        I: IntoIterator<Item = InvalidationKey>,
    {
        let affected = self.index.affected_exact(changed);
        let mut dropped = Vec::with_capacity(affected.len());
        for hash in affected {
            if self.release(&hash) {
                dropped.push(hash);
            }
        }
        dropped
    }

    /// Re-evaluates a payer's balance against its sponsored transactions.
    ///
    /// * Balance-bounded (trusted) payers evict from the low-priority end until
    ///   `reserved ≤ balance` (an aggregate set constraint).
    /// * Count-limited payers (and self-paying senders) drop any individual
    ///   transaction whose max cost now exceeds the balance (per-transaction
    ///   threshold), matching standard EIP-1559 semantics.
    ///
    /// Returns the dropped hashes.
    pub fn on_balance_changed(&mut self, account: Address, new_balance: U256) -> Vec<TxHash> {
        let mut dropped = Vec::new();

        if let Some(book) = self.payer_books.get_mut(&account) {
            dropped.extend(book.set_balance(new_balance));
        }

        // Per-transaction threshold for count-limited payers/self-pay. Collect
        // first to avoid borrowing the index while releasing.
        if let Some(watchers) = self.index.watchers(&InvalidationKey::Balance(account)) {
            let unaffordable: Vec<TxHash> = watchers
                .iter()
                .filter(|hash| {
                    self.records.get(*hash).is_some_and(|record| {
                        !record.payer_trusted && record.max_cost > new_balance
                    })
                })
                .copied()
                .collect();
            dropped.extend(unaffordable);
        }

        dropped.sort_unstable();
        dropped.dedup();
        for hash in &dropped {
            self.release(hash);
        }
        dropped
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

    fn slot_key(slot: u8) -> InvalidationKey {
        InvalidationKey::Slot { address: addr(0xFF), slot: alloy_primitives::B256::repeat_byte(slot) }
    }

    /// Builds a default self-paying admission with a single actor-config slot
    /// watch, for tests that focus on limits.
    fn self_pay(hash_byte: u8, account: Address, max_cost: u64) -> Admission {
        Admission {
            hash: hash(hash_byte),
            sender: account,
            payer: account,
            sender_locked: false,
            payer_trusted: false,
            payer_balance: U256::from(1_000_000u64),
            max_cost: U256::from(max_cost),
            priority: 1,
            watch_set: WatchSet::new().watch(InvalidationKey::Balance(account)),
        }
    }

    #[test]
    fn default_sender_limit_is_enforced() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let sender = addr(1);

        for i in 0..DEFAULT_SENDER_LIMIT as u8 {
            // Distinct payers so the payer dimension never binds first.
            let mut adm = self_pay(i, sender, 10);
            adm.payer = addr(100 + i);
            assert!(guard.try_admit(adm).is_ok());
        }
        let mut over = self_pay(200, sender, 10);
        over.payer = addr(250);
        assert_eq!(guard.try_admit(over), Err(LimitRejection::SenderLimit));
        assert_eq!(guard.len(), DEFAULT_SENDER_LIMIT as usize);
    }

    #[test]
    fn locked_sender_has_unlimited_sender_dimension() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let sender = addr(1);

        for i in 0..50u8 {
            let adm = Admission {
                sender_locked: true,
                payer: addr(100 + i), // distinct payers to isolate the sender dim
                ..self_pay(i, sender, 10)
            };
            assert!(guard.try_admit(adm).is_ok());
        }
        assert_eq!(guard.len(), 50);
    }

    #[test]
    fn default_payer_limit_is_enforced_across_senders() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);

        for i in 0..DEFAULT_PAYER_LIMIT as u8 {
            let adm = Admission { payer, ..self_pay(i, addr(i + 1), 10) };
            assert!(guard.try_admit(adm).is_ok());
        }
        let over = Admission { payer, ..self_pay(200, addr(201), 10) };
        assert_eq!(guard.try_admit(over), Err(LimitRejection::PayerLimit));
    }

    #[test]
    fn trusted_payer_is_bounded_by_balance() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);

        let make = |h: u8, cost: u64| Admission {
            payer,
            payer_trusted: true,
            payer_balance: U256::from(100u64),
            max_cost: U256::from(cost),
            ..self_pay(h, addr(h + 1), cost)
        };

        // 6 sponsored txs of cost 15 = 90 ≤ 100: all admitted, beating the
        // count limit of 4 because the payer is balance-bounded.
        for i in 0..6u8 {
            assert!(guard.try_admit(make(i, 15)).is_ok());
        }
        // The 7th (total 105) exceeds the balance.
        assert_eq!(guard.try_admit(make(6, 15)), Err(LimitRejection::PayerBalance));
        assert_eq!(guard.len(), 6);
    }

    #[test]
    fn rejected_payer_rolls_back_sender_reservation() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);

        // Saturate the payer count limit using distinct senders.
        for i in 0..DEFAULT_PAYER_LIMIT as u8 {
            assert!(guard.try_admit(Admission { payer, ..self_pay(i, addr(i + 1), 10) }).is_ok());
        }
        // A new sender whose payer is over the limit must be fully rejected,
        // leaving the sender dimension free for a later (different payer) tx.
        let sender = addr(200);
        assert_eq!(
            guard.try_admit(Admission { payer, ..self_pay(100, sender, 10) }),
            Err(LimitRejection::PayerLimit)
        );
        // Same sender, a fresh payer: must succeed, proving the rollback freed
        // the sender slot.
        assert!(
            guard.try_admit(Admission { payer: addr(250), ..self_pay(101, sender, 10) }).is_ok()
        );
    }

    #[test]
    fn release_frees_both_dimensions() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let sender = addr(1);

        for i in 0..DEFAULT_SENDER_LIMIT as u8 {
            let adm = Admission { payer: addr(100 + i), ..self_pay(i, sender, 10) };
            assert!(guard.try_admit(adm).is_ok());
        }
        assert!(guard.release(&hash(0)));
        // A slot opened up.
        let adm = Admission { payer: addr(250), ..self_pay(200, sender, 10) };
        assert!(guard.try_admit(adm).is_ok());
        assert!(!guard.release(&hash(123)), "untracked release is a no-op");
    }

    #[test]
    fn invalidate_exact_drops_slot_watchers() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let account = addr(1);
        let watched = slot_key(7);
        let other = slot_key(8);

        let adm = Admission {
            watch_set: WatchSet::new().watch(InvalidationKey::Balance(account)).watch(watched),
            ..self_pay(1, account, 10)
        };
        assert!(guard.try_admit(adm).is_ok());

        // An unrelated slot change drops nothing.
        assert!(guard.invalidate_exact([other]).is_empty());
        assert_eq!(guard.len(), 1);

        // The watched slot change drops the tx and frees its limits.
        let dropped = guard.invalidate_exact([watched]);
        assert_eq!(dropped, vec![hash(1)]);
        assert!(guard.is_empty());
    }

    #[test]
    fn balance_drop_evicts_unaffordable_count_limited_tx() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let account = addr(1);

        // Self-pay tx costing 500 against a (default) count-limited payer.
        let adm = self_pay(1, account, 500);
        assert!(guard.try_admit(adm).is_ok());

        // Balance still covers it: kept.
        assert!(guard.on_balance_changed(account, U256::from(500u64)).is_empty());
        // Balance drops below cost: dropped.
        let dropped = guard.on_balance_changed(account, U256::from(499u64));
        assert_eq!(dropped, vec![hash(1)]);
        assert!(guard.is_empty());
    }

    #[test]
    fn balance_drop_evicts_lowest_priority_for_trusted_payer() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);

        let make = |h: u8, cost: u64, priority: u128| Admission {
            payer,
            payer_trusted: true,
            payer_balance: U256::from(1_000u64),
            max_cost: U256::from(cost),
            priority,
            watch_set: WatchSet::new().watch(InvalidationKey::Balance(payer)),
            ..self_pay(h, addr(h + 1), cost)
        };

        assert!(guard.try_admit(make(1, 300, 5)).is_ok()); // lowest priority
        assert!(guard.try_admit(make(2, 300, 20)).is_ok()); // highest priority
        assert!(guard.try_admit(make(3, 300, 10)).is_ok()); // middle

        // Balance drops to 500: must shed until reserved ≤ 500, evicting the two
        // lowest-priority txs (prio 5 then 10).
        let mut dropped = guard.on_balance_changed(payer, U256::from(500u64));
        dropped.sort();
        assert_eq!(dropped, vec![hash(1), hash(3)]);
        assert!(guard.contains(&hash(2)));
        assert_eq!(guard.len(), 1);
    }
}
