//! Mempool admission and invalidation bookkeeping.
//!
//! [`MempoolGuard`] ties together the mechanisms built in
//! [`crate::invalidation`] and [`crate::limits`]:
//!
//! * the account **signature / payment** admission checks, and
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

/// Default cap on inflight signatures per unlocked account.
pub const DEFAULT_SIGNATURE_LIMIT: u32 = 4;
/// Default cap on inflight payments per count-limited payer account.
pub const DEFAULT_PAYMENT_LIMIT: u32 = 4;
/// Default cap on inflight config-change transactions per account.
///
/// Config changes mutate the account's own authorization surface (actor set,
/// policy, local epoch/sequence), so a landed one can invalidate every other
/// inflight transaction that reads that surface. At most one may be pending per
/// account, and — unlike the signature dimension — this cap is **not** lifted by
/// a locked or trusted classification: the stability that earns the unlimited
/// signature exemption is precisely what a config change is about to disturb.
pub const DEFAULT_ACCOUNT_CHANGE_LIMIT: u32 = 1;

/// Configurable per-account admission caps.
#[derive(Debug, Clone, Copy)]
pub struct GuardLimits {
    /// Cap on inflight transactions signed by an unlocked account, whether it
    /// appears as sender or sponsor. Locked accounts are exempt.
    pub signature_limit: u32,
    /// Cap on inflight payments for a count-limited payer. Balance-bounded
    /// trusted payers use aggregate reservations instead.
    pub payment_limit: u32,
    /// Cap on inflight config-change transactions per changed account. Applies
    /// regardless of lock/trusted status (see [`DEFAULT_ACCOUNT_CHANGE_LIMIT`]).
    pub account_change_limit: u32,
}

impl Default for GuardLimits {
    fn default() -> Self {
        Self {
            signature_limit: DEFAULT_SIGNATURE_LIMIT,
            payment_limit: DEFAULT_PAYMENT_LIMIT,
            account_change_limit: DEFAULT_ACCOUNT_CHANGE_LIMIT,
        }
    }
}

/// Why an admission was rejected by the limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LimitRejection {
    /// The sender's signature limit is reached.
    #[error("sender signature limit reached")]
    SenderLimit,
    /// The sponsored payer's signature limit is reached.
    #[error("payer signature limit reached")]
    PayerLimit,
    /// A count-limited payer's payment limit is reached.
    #[error("payer payment limit reached")]
    PaymentLimit,
    /// A balance-bounded payer cannot afford another reservation.
    #[error("payer balance is insufficient for reservation")]
    PayerBalance,
    /// The changed account already has an inflight config-change transaction.
    #[error("account already has an inflight config change")]
    AccountChangeLimit,
}

/// Per-transaction admission classification resolved during validation and
/// carried on the pooled transaction. The pool combines this with the
/// transaction's [`WatchSet`] to build an [`Admission`] without re-reading chain
/// state.
///
/// All fields default to the strictest interpretation when classification can't
/// be established (not locked, not trusted), so a failed or skipped read can
/// never grant elevated mempool access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitClass {
    /// Resolved sender account (the account whose sender dimension is charged).
    pub sender: Address,
    /// Resolved payer account (equals `sender` for self-paying transactions).
    pub payer: Address,
    /// Cache generation against which the lock/trusted classification was
    /// derived. Once integrated with the pool, admission will reject a class if
    /// state-diff invalidation advanced the generation while validation was in
    /// flight.
    pub classification_generation: u64,
    /// Whether the sender's owner config is locked (stable auth surface).
    pub sender_locked: bool,
    /// Whether the payer's owner config is locked (stable auth surface).
    pub payer_locked: bool,
    /// Whether the payer is balance-bounded (locked + trusted bytecode).
    pub payer_trusted: bool,
    /// The payer's state balance, used to seed a balance-bounded payer book.
    pub payer_balance: U256,
    /// The maximum cost this transaction can charge the payer.
    pub max_cost: U256,
    /// The account whose authorization surface this transaction mutates via a
    /// config change, or `None` if it carries none. Charged against the
    /// account-change dimension regardless of lock/trusted status.
    pub account_change: Option<Address>,
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
    /// Whether the payer's owner config is locked (stable auth surface ⇒ no
    /// sponsored-payer signature charge).
    pub payer_locked: bool,
    /// Whether the payer is balance-bounded (locked + trusted bytecode ⇒ the
    /// payer dimension is limited by balance rather than a count).
    pub payer_trusted: bool,
    /// The payer's current balance, used to seed a balance-bounded payer book.
    pub payer_balance: U256,
    /// The maximum cost this transaction can charge the payer.
    pub max_cost: U256,
    /// Effective-tip priority used for balance-bounded eviction ordering.
    pub priority: u128,
    /// The account whose authorization surface this transaction mutates via a
    /// config change, or `None` if it carries none. Charged against the
    /// account-change dimension regardless of lock/trusted status.
    pub account_change: Option<Address>,
    /// The invalidation surfaces this transaction depends on.
    pub watch_set: WatchSet,
}

/// How a transaction was charged against the limits, retained so removal can
/// release exactly the dimensions it consumed.
#[derive(Debug, Clone, Copy)]
pub struct AdmissionRecord {
    /// Account charged on the sender-signature dimension.
    pub sender: Address,
    /// Account charged on the payment and optional sponsor-signature dimensions.
    pub payer: Address,
    /// Whether admission charged the sender-signature dimension.
    pub sender_signature_charged: bool,
    /// Whether admission charged the sponsor-signature dimension.
    pub payer_signature_charged: bool,
    /// Whether admission actually reserved aggregate balance instead of a
    /// payment count. This records the accounting path taken, not merely the
    /// incoming classification: forced insertion may conservatively fall back
    /// to count accounting if an aggregate reservation does not fit.
    pub payer_trusted: bool,
    /// Maximum payer cost reserved by this transaction.
    pub max_cost: U256,
    /// The account charged on the account-change dimension, if any.
    pub account_change: Option<Address>,
}

/// In-memory admission and invalidation ledger for the pool.
#[derive(Debug)]
pub struct MempoolGuard {
    index: InvalidationIndex,
    signature_counts: InflightCounters,
    payment_counts: InflightCounters,
    change_counts: InflightCounters,
    payer_books: HashMap<Address, PayerBook>,
    records: HashMap<TxHash, AdmissionRecord>,
    limits: GuardLimits,
}

impl MempoolGuard {
    /// Creates a guard that never rejects on count limits (caps set to
    /// [`u32::MAX`]). Used while admission-limit enforcement is staged: the
    /// index and balance/exact invalidation are fully active, but no transaction
    /// is rejected for exceeding a per-account count.
    #[must_use]
    pub fn unlimited() -> Self {
        Self::new(GuardLimits {
            signature_limit: u32::MAX,
            payment_limit: u32::MAX,
            account_change_limit: u32::MAX,
        })
    }

    /// Creates a guard with the given limits.
    #[must_use]
    pub fn new(limits: GuardLimits) -> Self {
        Self {
            index: InvalidationIndex::new(),
            signature_counts: InflightCounters::new(),
            payment_counts: InflightCounters::new(),
            change_counts: InflightCounters::new(),
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

    /// Returns the hashes of every currently tracked transaction. Used by the
    /// pool's per-block reconcile to find and release records whose transaction
    /// has left both pools through a path the guard did not observe.
    #[must_use]
    pub fn tracked_hashes(&self) -> Vec<TxHash> {
        self.records.keys().copied().collect()
    }

    /// Applies signature and payment admission checks and, on success, registers
    /// the transaction's watch set and charges every applicable dimension. A
    /// transaction already tracked is accepted idempotently.
    ///
    /// On rejection no state is mutated (any partial reservation is rolled
    /// back), so the caller may safely drop the transaction.
    pub fn try_admit(&mut self, admission: Admission) -> Result<(), LimitRejection> {
        if let Some(record) = self.records.get(&admission.hash) {
            debug_assert_eq!(record.sender, admission.sender, "re-admission changed sender");
            debug_assert_eq!(record.payer, admission.payer, "re-admission changed payer");
            debug_assert_eq!(record.max_cost, admission.max_cost, "re-admission changed max cost");
            debug_assert_eq!(
                record.sender_signature_charged, !admission.sender_locked,
                "re-admission changed sender lock classification"
            );
            debug_assert_eq!(
                record.payer_signature_charged,
                admission.payer != admission.sender && !admission.payer_locked,
                "re-admission changed payer lock classification"
            );
            // A forced replacement may conservatively fall back from trusted
            // aggregate accounting to count accounting, but never vice versa.
            debug_assert!(!record.payer_trusted || admission.payer_trusted);
            debug_assert_eq!(
                record.account_change, admission.account_change,
                "re-admission changed account-change classification"
            );
            debug_assert_eq!(
                self.index.watch_set(&admission.hash),
                Some(&admission.watch_set),
                "re-admission changed invalidation surfaces"
            );
            return Ok(());
        }

        // The account-change dimension is charged first: it is independent of the
        // signature/payment dimensions and is never lifted by lock/trusted status,
        // so an early rejection here needs no rollback.
        if let Some(account) = admission.account_change
            && !self.change_counts.try_increment(account, self.limits.account_change_limit)
        {
            return Err(LimitRejection::AccountChangeLimit);
        }

        let sender_signature_charged = !admission.sender_locked;
        if sender_signature_charged
            && !self.signature_counts.try_increment(admission.sender, self.limits.signature_limit)
        {
            self.release_change(admission.account_change);
            return Err(LimitRejection::SenderLimit);
        }

        let payer_signature_charged =
            admission.payer != admission.sender && !admission.payer_locked;
        if payer_signature_charged
            && !self.signature_counts.try_increment(admission.payer, self.limits.signature_limit)
        {
            if sender_signature_charged {
                self.signature_counts.decrement(admission.sender);
            }
            self.release_change(admission.account_change);
            return Err(LimitRejection::PayerLimit);
        }

        let payment_ok = if admission.payer_trusted {
            // Only the first admission seeds the book. Once it exists, canonical
            // balance updates own this value through `on_balance_changed`; a
            // later validation snapshot must not overwrite a newer diff-fed value.
            let book = self
                .payer_books
                .entry(admission.payer)
                .or_insert_with(|| PayerBook::new(admission.payer_balance));
            book.try_reserve(admission.hash, admission.max_cost, admission.priority)
        } else {
            self.payment_counts.try_increment(admission.payer, self.limits.payment_limit)
        };

        if !payment_ok {
            // Roll back signature reservations so a payment rejection leaves
            // no trace.
            if sender_signature_charged {
                self.signature_counts.decrement(admission.sender);
            }
            if payer_signature_charged {
                self.signature_counts.decrement(admission.payer);
            }
            self.release_change(admission.account_change);
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
                LimitRejection::PaymentLimit
            });
        }

        self.records.insert(
            admission.hash,
            AdmissionRecord {
                sender: admission.sender,
                payer: admission.payer,
                sender_signature_charged,
                payer_signature_charged,
                payer_trusted: admission.payer_trusted,
                max_cost: admission.max_cost,
                account_change: admission.account_change,
            },
        );
        self.index.insert(admission.hash, admission.watch_set);
        Ok(())
    }

    /// Registers a transaction unconditionally, bypassing the count caps. Used
    /// for replacements: the replaced transaction is released first, so the swap
    /// is net-neutral on the count dimensions and must never be rejected (you can
    /// always fee-bump your own pooled transaction).
    ///
    /// A balance-bounded (trusted) payer still goes through the book; if the
    /// fee-bumped reservation no longer fits the cached balance (even after the
    /// replaced transaction's release), the record falls back to the
    /// count-based, per-transaction-threshold path instead of `payer_trusted`
    /// — an unreserved trusted-payer record would be invisible both to
    /// `PayerBook::set_balance` (never entered the book) and to the per-tx
    /// threshold check in `on_balance_changed` (which skips `payer_trusted`
    /// records), so it would only ever be cleaned up by `reconcile_guard` or
    /// expiry. The fallback keeps the "never reject" guarantee while ensuring
    /// the record stays reachable by the next balance-driven eviction.
    pub fn insert_forced(&mut self, admission: Admission) {
        if let Some(record) = self.records.get(&admission.hash) {
            debug_assert_eq!(record.sender, admission.sender, "forced re-admission changed sender");
            debug_assert_eq!(record.payer, admission.payer, "forced re-admission changed payer");
            debug_assert_eq!(
                record.max_cost, admission.max_cost,
                "forced re-admission changed max cost"
            );
            debug_assert_eq!(
                record.sender_signature_charged, !admission.sender_locked,
                "forced re-admission changed sender lock classification"
            );
            debug_assert_eq!(
                record.payer_signature_charged,
                admission.payer != admission.sender && !admission.payer_locked,
                "forced re-admission changed payer lock classification"
            );
            // A previous forced insertion may have fallen back from trusted
            // aggregate accounting to count accounting, but never vice versa.
            debug_assert!(!record.payer_trusted || admission.payer_trusted);
            debug_assert_eq!(
                record.account_change, admission.account_change,
                "forced re-admission changed account-change classification"
            );
            debug_assert_eq!(
                self.index.watch_set(&admission.hash),
                Some(&admission.watch_set),
                "forced re-admission changed invalidation surfaces"
            );
            return;
        }
        if let Some(account) = admission.account_change {
            let change_ok = self.change_counts.try_increment(account, u32::MAX);
            debug_assert!(change_ok, "uncapped increment must always succeed");
        }
        let sender_signature_charged = !admission.sender_locked;
        if sender_signature_charged {
            let sender_ok = self.signature_counts.try_increment(admission.sender, u32::MAX);
            debug_assert!(sender_ok, "uncapped increment must always succeed");
        }
        let payer_signature_charged =
            admission.payer != admission.sender && !admission.payer_locked;
        if payer_signature_charged {
            let payer_ok = self.signature_counts.try_increment(admission.payer, u32::MAX);
            debug_assert!(payer_ok, "uncapped increment must always succeed");
        }

        let payer_trusted = admission.payer_trusted && {
            // As in `try_admit`, only creation uses the validation snapshot;
            // canonical balance updates own an existing book's value.
            self.payer_books
                .entry(admission.payer)
                .or_insert_with(|| PayerBook::new(admission.payer_balance))
                .try_reserve(admission.hash, admission.max_cost, admission.priority)
        };

        if !payer_trusted {
            let payer_ok = self.payment_counts.try_increment(admission.payer, u32::MAX);
            debug_assert!(payer_ok, "uncapped increment must always succeed");
            // A trusted payer whose reservation didn't fit leaves a freshly
            // created, still-empty book behind — prune it so a payer that
            // never successfully reserved doesn't leak an entry.
            if admission.payer_trusted
                && self.payer_books.get(&admission.payer).is_some_and(PayerBook::is_empty)
            {
                self.payer_books.remove(&admission.payer);
            }
        }

        self.records.insert(
            admission.hash,
            AdmissionRecord {
                sender: admission.sender,
                payer: admission.payer,
                sender_signature_charged,
                payer_signature_charged,
                payer_trusted,
                max_cost: admission.max_cost,
                account_change: admission.account_change,
            },
        );
        self.index.insert(admission.hash, admission.watch_set);
    }

    /// Releases all bookkeeping for `hash` (limits and index). Returns `true` if
    /// the transaction was tracked. Used both for normal removal (mined,
    /// replaced) and as the internal step of invalidation.
    pub fn release(&mut self, hash: &TxHash) -> bool {
        let Some(record) = self.records.remove(hash) else {
            return false;
        };
        if record.sender_signature_charged {
            self.signature_counts.decrement(record.sender);
        }
        if record.payer_signature_charged {
            self.signature_counts.decrement(record.payer);
        }
        if record.payer_trusted {
            if let Some(book) = self.payer_books.get_mut(&record.payer) {
                book.remove(hash);
                if book.is_empty() {
                    self.payer_books.remove(&record.payer);
                }
            }
        } else {
            self.payment_counts.decrement(record.payer);
        }
        self.release_change(record.account_change);
        self.index.remove(hash);
        true
    }

    /// Releases the account-change reservation for `account_change`, if any.
    fn release_change(&mut self, account_change: Option<Address>) {
        if let Some(account) = account_change {
            self.change_counts.decrement(account);
        }
    }

    /// Releases and returns every transaction tracked by the guard.
    ///
    /// This is the fail-safe path for a gap in the state-diff feed: without all
    /// intervening changed keys, no existing admission can be proven current.
    pub fn invalidate_all(&mut self) -> Vec<TxHash> {
        let hashes = self.tracked_hashes();
        for hash in &hashes {
            let released = self.release(hash);
            debug_assert!(released, "tracked hash must be releasable");
        }
        hashes
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

    /// Invalidates all transactions in occupied expiry buckets at or below
    /// `horizon`, returning the dropped hashes and number of buckets fired.
    pub fn invalidate_expiry_buckets_through(&mut self, horizon: u64) -> (Vec<TxHash>, usize) {
        let due = self.index.due_expiry_buckets(horizon);
        let bucket_count = due.len();
        (self.invalidate_exact(due), bucket_count)
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
                        debug_assert_eq!(
                            record.payer, account,
                            "balance watcher must match the transaction payer"
                        );
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
            // The two eviction paths are mutually exclusive for each record:
            // aggregate books contain trusted-payer records, while the
            // threshold filter selects only non-trusted records.
            // `PayerBook::set_balance` already removed aggregate-limit
            // evictions from the book. `release` intentionally touches that
            // book again (as a no-op) while releasing the other dimensions.
            let released = self.release(hash);
            debug_assert!(released, "evicted hash must be releasable");
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
        InvalidationKey::Slot {
            address: addr(0xFF),
            slot: alloy_primitives::B256::repeat_byte(slot),
        }
    }

    /// Builds a default self-paying admission with a single actor-config slot
    /// watch, for tests that focus on limits.
    fn self_pay(hash_byte: u8, account: Address, max_cost: u64) -> Admission {
        Admission {
            hash: hash(hash_byte),
            sender: account,
            payer: account,
            sender_locked: false,
            payer_locked: false,
            payer_trusted: false,
            payer_balance: U256::from(1_000_000u64),
            max_cost: U256::from(max_cost),
            priority: 1,
            account_change: None,
            watch_set: WatchSet::new(),
        }
    }

    #[test]
    fn default_signature_limit_is_enforced() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let sender = addr(1);

        for i in 0..DEFAULT_SIGNATURE_LIMIT as u8 {
            // Distinct payers so the payer dimension never binds first.
            let mut adm = self_pay(i, sender, 10);
            adm.payer = addr(100 + i);
            assert!(guard.try_admit(adm).is_ok());
        }
        let mut over = self_pay(200, sender, 10);
        over.payer = addr(250);
        assert_eq!(guard.try_admit(over), Err(LimitRejection::SenderLimit));
        assert_eq!(guard.len(), DEFAULT_SIGNATURE_LIMIT as usize);
    }

    #[test]
    #[should_panic(expected = "re-admission changed payer")]
    fn idempotent_admission_rejects_changed_parameters() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let sender = addr(1);
        guard.try_admit(self_pay(1, sender, 10)).unwrap();

        let changed = Admission { payer: addr(2), ..self_pay(1, sender, 10) };
        let _ = guard.try_admit(changed);
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
    fn account_change_limit_is_one_regardless_of_lock_status() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let account = addr(1);

        // A locked sender is exempt from the signature dimension but NOT from the
        // config-change dimension: the first config change is admitted, a second
        // for the same account is rejected even though the sender is "unlimited".
        let first = Admission {
            sender_locked: true,
            account_change: Some(account),
            payer: addr(100),
            ..self_pay(1, account, 10)
        };
        assert!(guard.try_admit(first).is_ok());

        let second = Admission {
            sender_locked: true,
            account_change: Some(account),
            payer: addr(101),
            ..self_pay(2, account, 10)
        };
        assert_eq!(guard.try_admit(second), Err(LimitRejection::AccountChangeLimit));
        assert_eq!(guard.len(), 1);

        // A config change for a *different* account is unaffected.
        let other = addr(2);
        let other_change = Admission {
            sender_locked: true,
            account_change: Some(other),
            payer: addr(102),
            ..self_pay(3, other, 10)
        };
        assert!(guard.try_admit(other_change).is_ok());
    }

    #[test]
    fn account_change_slot_frees_on_release() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let account = addr(1);

        let first = Admission {
            account_change: Some(account),
            payer: addr(100),
            ..self_pay(1, account, 10)
        };
        assert!(guard.try_admit(first).is_ok());
        let blocked = Admission {
            account_change: Some(account),
            payer: addr(101),
            ..self_pay(2, account, 10)
        };
        assert_eq!(guard.try_admit(blocked), Err(LimitRejection::AccountChangeLimit));

        assert!(guard.release(&hash(1)));
        let after = Admission {
            account_change: Some(account),
            payer: addr(102),
            ..self_pay(3, account, 10)
        };
        assert!(guard.try_admit(after).is_ok());
    }

    #[test]
    fn account_change_charge_rolls_back_when_a_later_dimension_rejects() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);

        // Saturate the payer's payment count with distinct senders. A locked
        // payer is charged only on the payment dimension, so the config change
        // below binds there rather than on the payer signature dimension.
        for i in 0..DEFAULT_PAYMENT_LIMIT as u8 {
            let fill = Admission { payer, payer_locked: true, ..self_pay(i, addr(i + 1), 10) };
            assert!(guard.try_admit(fill).is_ok());
        }

        let account = addr(200);
        // A config change whose payer is over the payment limit is rejected on the
        // payment dimension — the account-change charge must be rolled back.
        let rejected = Admission {
            payer,
            payer_locked: true,
            account_change: Some(account),
            ..self_pay(100, account, 10)
        };
        assert_eq!(guard.try_admit(rejected), Err(LimitRejection::PaymentLimit));

        // The rollback freed the account-change slot: the same account admits a
        // config change through a fresh payer.
        let accepted = Admission {
            payer: addr(250),
            payer_locked: true,
            account_change: Some(account),
            ..self_pay(101, account, 10)
        };
        assert!(guard.try_admit(accepted).is_ok());
    }

    #[test]
    fn insert_forced_bypasses_account_change_cap_for_replacement() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let account = addr(1);

        let first = Admission {
            account_change: Some(account),
            payer: addr(100),
            ..self_pay(1, account, 10)
        };
        assert!(guard.try_admit(first).is_ok());
        let new = Admission {
            account_change: Some(account),
            payer: addr(101),
            ..self_pay(2, account, 10)
        };
        assert_eq!(guard.try_admit(new), Err(LimitRejection::AccountChangeLimit));

        // A replacement releases the old hash first, then force-inserts the
        // fee-bumped one; the swap stays at the cap and is never rejected.
        assert!(guard.release(&hash(1)));
        let replacement = Admission {
            account_change: Some(account),
            payer: addr(102),
            ..self_pay(3, account, 10)
        };
        guard.insert_forced(replacement);
        assert!(guard.contains(&hash(3)));
        assert!(!guard.contains(&hash(1)));
        assert_eq!(guard.len(), 1);
    }

    #[test]
    fn default_payment_limit_is_enforced_across_senders() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);

        for i in 0..DEFAULT_PAYMENT_LIMIT as u8 {
            let adm = Admission { payer, payer_locked: true, ..self_pay(i, addr(i + 1), 10) };
            assert!(guard.try_admit(adm).is_ok());
        }
        let over = Admission { payer, payer_locked: true, ..self_pay(200, addr(201), 10) };
        assert_eq!(guard.try_admit(over), Err(LimitRejection::PaymentLimit));
    }

    #[test]
    fn signature_limit_is_shared_across_sender_and_payer_roles() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let account = addr(9);

        for i in 0..DEFAULT_SIGNATURE_LIMIT as u8 {
            let admission =
                Admission { payer: addr(100 + i), payer_locked: true, ..self_pay(i, account, 10) };
            guard.try_admit(admission).unwrap();
        }

        let sponsored = Admission { payer: account, ..self_pay(100, addr(10), 10) };
        assert_eq!(guard.try_admit(sponsored), Err(LimitRejection::PayerLimit));
    }

    #[test]
    fn locked_untrusted_payer_is_bounded_by_payment_count() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);

        for i in 0..DEFAULT_PAYMENT_LIMIT as u8 {
            let admission = Admission { payer, payer_locked: true, ..self_pay(i, addr(i + 1), 10) };
            guard.try_admit(admission).unwrap();
        }

        let over = Admission { payer, payer_locked: true, ..self_pay(100, addr(100), 10) };
        assert_eq!(guard.try_admit(over), Err(LimitRejection::PaymentLimit));
    }

    #[test]
    fn trusted_payer_is_bounded_by_balance() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);

        let make = |h: u8, cost: u64| Admission {
            payer,
            payer_locked: true,
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
        for i in 0..DEFAULT_PAYMENT_LIMIT as u8 {
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

        for i in 0..DEFAULT_SIGNATURE_LIMIT as u8 {
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
    fn expiry_invalidation_fires_only_occupied_due_buckets() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let make = |id, bucket| Admission {
            watch_set: WatchSet::new().watch(InvalidationKey::ExpiryBucket(bucket)),
            ..self_pay(id, addr(id), 10)
        };

        assert!(guard.try_admit(make(1, 1)).is_ok());
        assert!(guard.try_admit(make(2, 850_000_000)).is_ok());
        assert!(guard.try_admit(make(3, 900_000_000)).is_ok());

        let (mut dropped, buckets) = guard.invalidate_expiry_buckets_through(850_000_000);
        dropped.sort_unstable();
        assert_eq!(dropped, vec![hash(1), hash(2)]);
        assert_eq!(buckets, 2);
        assert!(guard.contains(&hash(3)));
    }

    #[test]
    fn balance_drop_evicts_unaffordable_count_limited_tx() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let account = addr(1);

        // Self-pay tx costing 500 against a (default) count-limited payer.
        let adm = Admission {
            watch_set: WatchSet::new().watch(InvalidationKey::Balance(account)),
            ..self_pay(1, account, 500)
        };
        assert!(guard.try_admit(adm).is_ok());

        // Balance still covers it: kept.
        assert!(guard.on_balance_changed(account, U256::from(500u64)).is_empty());
        // Balance drops below cost: dropped.
        let dropped = guard.on_balance_changed(account, U256::from(499u64));
        assert_eq!(dropped, vec![hash(1)]);
        assert!(guard.is_empty());
    }

    #[test]
    #[should_panic(expected = "balance watcher must match the transaction payer")]
    fn balance_change_rejects_mismatched_payer_watch() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(1);
        let watched = addr(2);
        let admission = Admission {
            watch_set: WatchSet::new().watch(InvalidationKey::Balance(watched)),
            ..self_pay(1, payer, 500)
        };

        assert!(guard.try_admit(admission).is_ok());
        guard.on_balance_changed(watched, U256::from(499u64));
    }

    #[test]
    fn insert_forced_bypasses_caps_for_replacement() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let sender = addr(1);

        // Fill the sender dimension to its default cap with distinct payers.
        for i in 0..DEFAULT_SIGNATURE_LIMIT as u8 {
            let adm = Admission { payer: addr(100 + i), ..self_pay(i, sender, 10) };
            assert!(guard.try_admit(adm).is_ok());
        }
        // A genuinely new tx is rejected at the cap.
        let new = Admission { payer: addr(200), ..self_pay(50, sender, 10) };
        assert_eq!(guard.try_admit(new), Err(LimitRejection::SenderLimit));

        // A replacement (pool releases the old hash first, then force-inserts the
        // new one) stays at the cap and is never rejected.
        assert!(guard.release(&hash(0)));
        let replacement = Admission { payer: addr(250), ..self_pay(60, sender, 10) };
        guard.insert_forced(replacement);
        assert_eq!(guard.len(), DEFAULT_SIGNATURE_LIMIT as usize);
        assert!(guard.contains(&hash(60)));
        assert!(!guard.contains(&hash(0)));
    }

    #[test]
    fn insert_forced_is_idempotent_and_releasable() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let account = addr(1);

        let adm = self_pay(1, account, 10);
        guard.insert_forced(adm.clone());
        // Re-forcing the same hash is a no-op (no double counting).
        guard.insert_forced(adm);
        assert_eq!(guard.len(), 1);

        // A forced insert is released through the normal path and frees the slot.
        assert!(guard.release(&hash(1)));
        assert!(guard.is_empty());
    }

    #[test]
    #[should_panic(expected = "forced re-admission changed payer")]
    fn forced_idempotent_admission_rejects_changed_parameters() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let sender = addr(1);
        guard.insert_forced(self_pay(1, sender, 10));

        let changed = Admission { payer: addr(2), ..self_pay(1, sender, 10) };
        guard.insert_forced(changed);
    }

    #[test]
    fn insert_forced_trusted_replacement_tracks_in_book() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);
        let make = |h: u8, cost: u64| Admission {
            payer,
            payer_locked: true,
            payer_trusted: true,
            payer_balance: U256::from(100u64),
            max_cost: U256::from(cost),
            watch_set: WatchSet::new().watch(InvalidationKey::Balance(payer)),
            ..self_pay(h, addr(h + 1), cost)
        };

        assert!(guard.try_admit(make(1, 60)).is_ok());
        // Swap: release the old reservation, then force the fee-bumped one. The
        // new reservation fits the freed balance and is tracked for eviction.
        assert!(guard.release(&hash(1)));
        guard.insert_forced(make(2, 70));
        assert!(guard.contains(&hash(2)));
        // A balance drop below the reservation still evicts it (book-tracked).
        let dropped = guard.on_balance_changed(payer, U256::from(50u64));
        assert_eq!(dropped, vec![hash(2)]);
        assert!(guard.is_empty());
    }

    #[test]
    fn insert_forced_trusted_replacement_overshooting_balance_falls_back_to_count_path() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);
        let make = |h: u8, cost: u64| Admission {
            payer,
            payer_locked: true,
            payer_trusted: true,
            payer_balance: U256::from(100u64),
            max_cost: U256::from(cost),
            watch_set: WatchSet::new().watch(InvalidationKey::Balance(payer)),
            ..self_pay(h, addr(h + 1), cost)
        };

        assert!(guard.try_admit(make(1, 60)).is_ok());
        // Swap: release the old reservation, then force a fee-bumped one whose
        // cost exceeds even the freed balance. `try_reserve` fails, so the
        // record must fall back to the count-based path rather than being
        // silently unreserved-yet-`payer_trusted` (invisible to eviction).
        assert!(guard.release(&hash(1)));
        guard.insert_forced(make(2, 150));
        assert!(guard.contains(&hash(2)));

        // Before this fix, this record would have kept `payer_trusted: true`
        // without ever entering the book — invisible to `set_balance` (never
        // reserved) *and* to the per-tx threshold check (which skips
        // `payer_trusted` records). The fallback makes it evictable again via
        // the per-transaction threshold path once the balance can't cover it.
        let dropped = guard.on_balance_changed(payer, U256::from(100u64));
        assert_eq!(dropped, vec![hash(2)]);
        assert!(guard.is_empty());
    }

    #[test]
    fn balance_drop_evicts_lowest_priority_for_trusted_payer() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);

        let make = |h: u8, cost: u64, priority: u128| Admission {
            payer,
            payer_locked: true,
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

    #[test]
    fn invalidate_all_releases_every_dimension() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let sender = addr(1);
        let trusted_payer = addr(2);

        guard.try_admit(self_pay(1, sender, 10)).unwrap();
        guard
            .try_admit(Admission {
                payer: trusted_payer,
                payer_locked: true,
                payer_trusted: true,
                payer_balance: U256::from(100u64),
                ..self_pay(2, sender, 20)
            })
            .unwrap();

        let mut invalidated = guard.invalidate_all();
        invalidated.sort_unstable();
        assert_eq!(invalidated, vec![hash(1), hash(2)]);
        assert!(guard.is_empty());

        // All count and reservation bookkeeping must have been released, so
        // the same dimensions can immediately admit new transactions.
        guard.try_admit(self_pay(3, sender, 10)).unwrap();
        guard
            .try_admit(Admission {
                payer: trusted_payer,
                payer_locked: true,
                payer_trusted: true,
                payer_balance: U256::from(100u64),
                ..self_pay(4, sender, 20)
            })
            .unwrap();
    }
}
