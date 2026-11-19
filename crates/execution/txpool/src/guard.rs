//! Mempool admission and invalidation bookkeeping.
//!
//! [`MempoolGuard`] ties together the mechanisms built in
//! [`crate::invalidation`] and [`crate::limits`]:
//!
//! * the per-payer **payment** admission checks, and
//! * state-keyed **invalidation** (exact-match slot drops and balance
//!   threshold/aggregate re-evaluation).
//!
//! It is a pure in-memory ledger: it never reads chain state. Callers supply the
//! per-transaction classification ([`Admission`]) computed during validation,
//! and feed it changed storage slots and account balances from the block diff
//! stream. The guard returns the hashes it has dropped so the pool can evict the
//! corresponding transactions.

use alloy_primitives::{
    Address, TxHash, U256,
    map::{AddressMap, B256Map},
};

use crate::{
    InflightCounters, InvalidationIndex, InvalidationKey, PayerBook, UnixMillis, WatchSet,
};

/// Default cap on inflight payments per count-limited payer account.
pub const DEFAULT_PAYMENT_LIMIT: u32 = 16;
/// Default cap on inflight payments per operator-allowlisted payer account.
pub const DEFAULT_ALLOWLISTED_PAYMENT_LIMIT: u32 = 64;

/// Configurable per-account admission caps.
#[derive(Debug, Clone, Copy)]
pub struct GuardLimits {
    /// Cap on inflight payments per payer account.
    pub payment_limit: u32,
    /// Cap on inflight payments for an operator-allowlisted payer, which is
    /// also balance-bounded through its payer book.
    pub allowlisted_payment_limit: u32,
}

impl Default for GuardLimits {
    fn default() -> Self {
        Self {
            payment_limit: DEFAULT_PAYMENT_LIMIT,
            allowlisted_payment_limit: DEFAULT_ALLOWLISTED_PAYMENT_LIMIT,
        }
    }
}

/// Why an admission was rejected by the limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LimitRejection {
    /// The payer's payment limit is reached.
    #[error("payer payment limit reached")]
    PaymentLimit,
    /// A balance-bounded (allowlisted) payer cannot afford another reservation.
    #[error("payer balance is insufficient for reservation")]
    PayerBalance,
}

/// Per-transaction admission classification resolved during validation and
/// carried on the pooled transaction. The pool combines this with the
/// transaction's [`WatchSet`] to build an [`Admission`] without re-reading chain
/// state.
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitClass {
    /// Resolved sender account.
    pub sender: Address,
    /// Resolved payer account (equals `sender` for self-paying transactions).
    pub payer: Address,
    /// Classification generation at which the classification was derived.
    /// Admission rejects a class if state-diff invalidation advanced the
    /// generation while validation was in flight.
    pub classification_generation: u64,
    /// Whether the payer is operator-allowlisted (count-limited at its own cap
    /// and balance-bounded through a payer book).
    pub payer_allowlisted: bool,
    /// The payer's state balance, used to seed a balance-bounded payer book.
    pub payer_balance: U256,
    /// The maximum cost this transaction can charge the payer.
    pub max_cost: U256,
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
    /// Whether the payer is operator-allowlisted (⇒ counted at
    /// [`GuardLimits::allowlisted_payment_limit`] and also balance-bounded).
    pub payer_allowlisted: bool,
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
pub struct AdmissionRecord {
    /// Resolved sender account.
    pub sender: Address,
    /// Account charged on the payment dimension.
    pub payer: Address,
    /// Whether admission actually reserved aggregate balance in the payer's
    /// book. This records the accounting path taken, not merely the incoming
    /// classification: forced insertion may conservatively fall back to the
    /// per-transaction threshold if an aggregate reservation does not fit.
    pub payer_booked: bool,
    /// Maximum payer cost reserved by this transaction.
    pub max_cost: U256,
}

/// In-memory admission and invalidation ledger for the pool.
#[derive(Debug)]
pub struct MempoolGuard {
    index: InvalidationIndex,
    payment_counts: InflightCounters,
    payer_books: AddressMap<PayerBook>,
    records: B256Map<(AdmissionRecord, WatchSet)>,
    limits: GuardLimits,
}

impl MempoolGuard {
    /// Creates a guard that never rejects on count limits (caps set to
    /// [`u32::MAX`]). Used while admission-limit enforcement is staged: the
    /// index and balance/exact invalidation are fully active, but no transaction
    /// is rejected for exceeding a per-account count.
    #[must_use]
    pub fn unlimited() -> Self {
        Self::new(GuardLimits { payment_limit: u32::MAX, allowlisted_payment_limit: u32::MAX })
    }

    /// Creates a guard with the given limits.
    #[must_use]
    pub fn new(limits: GuardLimits) -> Self {
        Self {
            index: InvalidationIndex::new(),
            payment_counts: InflightCounters::new(),
            payer_books: AddressMap::default(),
            records: B256Map::default(),
            limits,
        }
    }

    /// The payer's payment count cap: allowlisted payers have their own cap
    /// and are also balance-bounded through a book.
    const fn payment_limit(&self, admission: &Admission) -> u32 {
        if admission.payer_allowlisted {
            self.limits.allowlisted_payment_limit
        } else {
            self.limits.payment_limit
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

    /// Returns the tracked transactions watching `key`.
    #[must_use]
    pub fn watchers(&self, key: &InvalidationKey) -> Vec<TxHash> {
        self.index.watchers(key).copied().collect()
    }

    /// Returns the hashes of every currently tracked transaction. Used by the
    /// pool's per-block reconcile to find and release records whose transaction
    /// has left both pools through a path the guard did not observe.
    #[must_use]
    pub fn tracked_hashes(&self) -> Vec<TxHash> {
        self.records.keys().copied().collect()
    }

    /// Applies the payment admission checks and, on success, registers the
    /// transaction's watch set and charges the payer. A transaction already
    /// tracked is accepted idempotently.
    ///
    /// On rejection no state is mutated (any partial reservation is rolled
    /// back), so the caller may safely drop the transaction.
    pub fn try_admit(&mut self, admission: Admission) -> Result<(), LimitRejection> {
        if let Some((record, watch_set)) = self.records.get(&admission.hash) {
            debug_assert_eq!(record.sender, admission.sender, "re-admission changed sender");
            debug_assert_eq!(record.payer, admission.payer, "re-admission changed payer");
            debug_assert_eq!(record.max_cost, admission.max_cost, "re-admission changed max cost");
            // A forced replacement may conservatively fall back from aggregate
            // accounting to the per-transaction threshold, but never vice versa.
            debug_assert!(!record.payer_booked || admission.payer_allowlisted);
            debug_assert_eq!(
                watch_set, &admission.watch_set,
                "re-admission changed invalidation surfaces"
            );
            return Ok(());
        }

        if !self.payment_counts.try_increment(admission.payer, self.payment_limit(&admission)) {
            return Err(LimitRejection::PaymentLimit);
        }
        let payer_booked = admission.payer_allowlisted;
        if payer_booked && {
            // Only the first admission seeds the book. Once it exists, canonical
            // balance updates own this value through `on_balance_changed`; a
            // later validation snapshot must not overwrite a newer diff-fed value.
            let book = self
                .payer_books
                .entry(admission.payer)
                .or_insert_with(|| PayerBook::new(admission.payer_balance));
            !book.try_reserve(admission.hash, admission.max_cost, admission.priority)
        } {
            self.payment_counts.decrement(admission.payer);
            // A freshly created, now-empty payer book is pruned to avoid leaking
            // an entry for a payer that never successfully reserved.
            if self.payer_books.get(&admission.payer).is_some_and(PayerBook::is_empty) {
                self.payer_books.remove(&admission.payer);
            }
            return Err(LimitRejection::PayerBalance);
        }

        self.index.insert(admission.hash, &admission.watch_set);
        self.records.insert(
            admission.hash,
            (
                AdmissionRecord {
                    sender: admission.sender,
                    payer: admission.payer,
                    payer_booked,
                    max_cost: admission.max_cost,
                },
                admission.watch_set,
            ),
        );
        Ok(())
    }

    /// Registers a transaction unconditionally, bypassing the count caps. Used
    /// for replacements that passed [`Self::check_replacement`]: the replaced
    /// transaction is released first, so a swap that keeps the payer is
    /// net-neutral on the count dimension and must never be rejected (you can
    /// always fee-bump your own pooled transaction).
    ///
    /// An allowlisted payer still goes through the book; if the fee-bumped
    /// reservation no longer fits the cached balance (even after the replaced
    /// transaction's release), the record falls back to the
    /// per-transaction-threshold path instead of `payer_booked` — an unreserved
    /// booked record would be invisible both to `PayerBook::set_balance` (never
    /// entered the book) and to the per-tx threshold check in
    /// `on_balance_changed` (which skips `payer_booked` records), so it would
    /// only ever be cleaned up by `reconcile_guard` or expiry. The fallback
    /// keeps the "never reject" guarantee while ensuring the record stays
    /// reachable by the next balance-driven eviction.
    pub fn insert_forced(&mut self, admission: Admission) {
        if let Some((record, watch_set)) = self.records.get(&admission.hash) {
            debug_assert_eq!(record.sender, admission.sender, "forced re-admission changed sender");
            debug_assert_eq!(record.payer, admission.payer, "forced re-admission changed payer");
            debug_assert_eq!(
                record.max_cost, admission.max_cost,
                "forced re-admission changed max cost"
            );
            // A previous forced insertion may have fallen back from aggregate
            // accounting to the per-transaction threshold, but never vice versa.
            debug_assert!(!record.payer_booked || admission.payer_allowlisted);
            debug_assert_eq!(
                watch_set, &admission.watch_set,
                "forced re-admission changed invalidation surfaces"
            );
            return;
        }
        let payer_ok = self.payment_counts.try_increment(admission.payer, u32::MAX);
        debug_assert!(payer_ok, "uncapped increment must always succeed");

        let payer_booked = admission.payer_allowlisted && {
            // As in `try_admit`, only creation uses the validation snapshot;
            // canonical balance updates own an existing book's value.
            self.payer_books
                .entry(admission.payer)
                .or_insert_with(|| PayerBook::new(admission.payer_balance))
                .try_reserve(admission.hash, admission.max_cost, admission.priority)
        };
        // An allowlisted payer whose reservation didn't fit leaves a freshly
        // created, still-empty book behind — prune it so a payer that never
        // successfully reserved doesn't leak an entry.
        if admission.payer_allowlisted
            && !payer_booked
            && self.payer_books.get(&admission.payer).is_some_and(PayerBook::is_empty)
        {
            self.payer_books.remove(&admission.payer);
        }

        self.index.insert(admission.hash, &admission.watch_set);
        self.records.insert(
            admission.hash,
            (
                AdmissionRecord {
                    sender: admission.sender,
                    payer: admission.payer,
                    payer_booked,
                    max_cost: admission.max_cost,
                },
                admission.watch_set,
            ),
        );
    }

    /// Checks, without mutating, whether replacing the tracked `replaced` with
    /// `admission` fits the caps. Keeping the payer is net-neutral and always
    /// fits. Naming a different payer charges that payer as a fresh admission
    /// would, so the new payer must have room; otherwise a fee bump could
    /// rotate a sponsor past its caps through [`Self::insert_forced`].
    pub fn check_replacement(
        &self,
        replaced: &TxHash,
        admission: &Admission,
    ) -> Result<(), LimitRejection> {
        if self.records.get(replaced).is_none_or(|(record, _)| record.payer == admission.payer) {
            return Ok(());
        }
        if self.payment_counts.get(&admission.payer) >= self.payment_limit(admission) {
            return Err(LimitRejection::PaymentLimit);
        }
        let available = self
            .payer_books
            .get(&admission.payer)
            .map_or(admission.payer_balance, PayerBook::available);
        if admission.payer_allowlisted && admission.max_cost > available {
            return Err(LimitRejection::PayerBalance);
        }
        Ok(())
    }

    /// Releases all bookkeeping for `hash` (limits and index). Returns `true` if
    /// the transaction was tracked. Used both for normal removal (mined,
    /// replaced) and as the internal step of invalidation.
    pub fn release(&mut self, hash: &TxHash) -> bool {
        let Some((record, watch_set)) = self.records.remove(hash) else {
            return false;
        };
        if record.payer_booked
            && let Some(book) = self.payer_books.get_mut(&record.payer)
        {
            book.remove(hash);
            if book.is_empty() {
                self.payer_books.remove(&record.payer);
            }
        }
        self.payment_counts.decrement(record.payer);
        self.index.remove(hash, &watch_set);
        true
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
    /// **exact-match** keys (e.g. nonce slots or code hashes). Returns
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

    /// Invalidates every transaction whose expiry deadline has passed at the
    /// block time `now`, returning the dropped hashes and the
    /// number of distinct deadlines fired.
    pub fn invalidate_expired(&mut self, now: UnixMillis) -> (Vec<TxHash>, usize) {
        let due = self.index.take_expired(now);
        let deadline_count = due.chunk_by(|a, b| a.0 == b.0).count();
        // `release` is false for a hash already dropped via an earlier deadline.
        let dropped =
            due.into_iter().filter_map(|(_, hash)| self.release(&hash).then_some(hash)).collect();
        (dropped, deadline_count)
    }

    /// Re-evaluates a payer's balance against its sponsored transactions.
    ///
    /// * Balance-bounded (allowlisted) payers evict from the low-priority end until
    ///   `reserved ≤ balance` (an aggregate set constraint).
    /// * Other payers (and self-paying senders) drop any individual
    ///   transaction whose max cost now exceeds the balance (per-transaction
    ///   threshold), matching standard EIP-1559 semantics.
    ///
    /// Returns the dropped hashes.
    pub fn on_balance_changed(&mut self, account: Address, new_balance: U256) -> Vec<TxHash> {
        let mut dropped = Vec::new();

        if let Some(book) = self.payer_books.get_mut(&account) {
            dropped.extend(book.set_balance(new_balance));
        }

        // Per-transaction threshold for unbooked payers/self-pay. Collect
        // first to avoid borrowing the index while releasing.
        let unaffordable: Vec<TxHash> = self
            .index
            .watchers(&InvalidationKey::Balance(account))
            .filter(|hash| {
                self.records.get(*hash).is_some_and(|(record, _)| {
                    debug_assert_eq!(
                        record.payer, account,
                        "balance watcher must match the transaction payer"
                    );
                    !record.payer_booked && record.max_cost > new_balance
                })
            })
            .copied()
            .collect();
        dropped.extend(unaffordable);

        dropped.sort_unstable();
        dropped.dedup();
        for hash in &dropped {
            // The two eviction paths are mutually exclusive for each record:
            // aggregate books contain booked records, while the threshold
            // filter selects only unbooked records.
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

    /// Builds a default self-paying admission with no watches, for tests that
    /// focus on limits.
    fn self_pay(hash_byte: u8, account: Address, max_cost: u64) -> Admission {
        Admission {
            hash: hash(hash_byte),
            sender: account,
            payer: account,
            payer_allowlisted: false,
            payer_balance: U256::from(1_000_000u64),
            max_cost: U256::from(max_cost),
            priority: 1,
            watch_set: WatchSet::new(),
        }
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
    fn default_payment_limit_is_enforced_across_senders() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);

        for i in 0..DEFAULT_PAYMENT_LIMIT as u8 {
            let adm = Admission { payer, ..self_pay(i, addr(i + 1), 10) };
            assert!(guard.try_admit(adm).is_ok());
        }
        let over = Admission { payer, ..self_pay(200, addr(201), 10) };
        assert_eq!(guard.try_admit(over), Err(LimitRejection::PaymentLimit));
    }

    #[test]
    fn default_payment_limit_is_sixteen() {
        assert_eq!(GuardLimits::default().payment_limit, 16);
    }

    /// An allowlisted payer gets its own payment cap and is also bounded by its
    /// balance through a payer book; releasing frees both.
    #[test]
    fn allowlisted_payer_is_bounded_by_count_and_balance() {
        let payer = addr(9);
        let limits = GuardLimits { allowlisted_payment_limit: 20, ..GuardLimits::default() };
        let mut guard = MempoolGuard::new(limits);
        let make = |h: u8, cost: u64| Admission {
            payer,
            payer_allowlisted: true,
            payer_balance: U256::from(1_000u64),
            max_cost: U256::from(cost),
            ..self_pay(h, addr(h + 1), cost)
        };

        // Past the default payment limit, up to the allowlisted cap.
        for i in 0..20u8 {
            guard.try_admit(make(i, 10)).unwrap();
        }
        assert_eq!(guard.try_admit(make(100, 10)), Err(LimitRejection::PaymentLimit));

        // The book bounds the aggregate cost: 20 * 10 is reserved of 1_000.
        assert!(guard.release(&hash(0)));
        assert_eq!(guard.try_admit(make(101, 900)), Err(LimitRejection::PayerBalance));
        guard.try_admit(make(102, 810)).unwrap();
        assert_eq!(guard.len(), 20);

        // A balance drop evicts from the book.
        assert!(!guard.on_balance_changed(payer, U256::from(500u64)).is_empty());
        for hash in guard.tracked_hashes() {
            assert!(guard.release(&hash));
        }
        assert!(guard.is_empty());
        assert!(guard.payer_books.is_empty(), "releasing every payment drops the book");
        guard.try_admit(make(103, 10)).unwrap();
    }

    #[test]
    fn release_frees_the_payment_slot() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);

        for i in 0..DEFAULT_PAYMENT_LIMIT as u8 {
            let adm = Admission { payer, ..self_pay(i, addr(i + 1), 10) };
            assert!(guard.try_admit(adm).is_ok());
        }
        assert!(guard.release(&hash(0)));
        // A slot opened up.
        let adm = Admission { payer, ..self_pay(200, addr(200), 10) };
        assert!(guard.try_admit(adm).is_ok());
        assert!(!guard.release(&hash(123)), "untracked release is a no-op");
    }

    #[test]
    fn insert_forced_bypasses_caps_for_replacement() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);

        // Fill the payer's payment dimension to its default cap.
        for i in 0..DEFAULT_PAYMENT_LIMIT as u8 {
            let adm = Admission { payer, ..self_pay(i, addr(i + 1), 10) };
            assert!(guard.try_admit(adm).is_ok());
        }
        // A genuinely new tx is rejected at the cap.
        let new = Admission { payer, ..self_pay(50, addr(200), 10) };
        assert_eq!(guard.try_admit(new), Err(LimitRejection::PaymentLimit));

        // A replacement that keeps its payer (pool releases the old hash first,
        // then force-inserts the new one) stays at the cap and is never rejected.
        let replacement = Admission { payer, ..self_pay(60, addr(1), 10) };
        assert_eq!(guard.check_replacement(&hash(0), &replacement), Ok(()));
        assert!(guard.release(&hash(0)));
        guard.insert_forced(replacement);
        assert_eq!(guard.len(), DEFAULT_PAYMENT_LIMIT as usize);
        assert!(guard.contains(&hash(60)));
        assert!(!guard.contains(&hash(0)));
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
    fn expiry_invalidation_fires_only_passed_deadlines() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let make = |id, deadline| Admission {
            watch_set: WatchSet::new()
                .watch(InvalidationKey::Expiry(UnixMillis::from_millis(deadline))),
            ..self_pay(id, addr(id), 10)
        };

        assert!(guard.try_admit(make(1, 1)).is_ok());
        assert!(guard.try_admit(make(2, 1_700_000_000_200)).is_ok());
        assert!(guard.try_admit(make(3, 1_700_000_000_400)).is_ok());
        // Two passed deadlines still drop the transaction once.
        let mut twice = make(4, 5);
        twice.watch_set.push(InvalidationKey::Expiry(UnixMillis::from_millis(1_700_000_000_100)));
        assert!(guard.try_admit(twice).is_ok());

        let (mut dropped, deadlines) =
            guard.invalidate_expired(UnixMillis::from_millis(1_700_000_000_400));
        dropped.sort_unstable();
        assert_eq!(dropped, vec![hash(1), hash(2), hash(4)]);
        assert_eq!(deadlines, 4);
        assert!(guard.contains(&hash(3)), "valid through its deadline millisecond");
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
    fn replacement_naming_a_new_payer_is_held_to_its_payment_cap() {
        let limits = GuardLimits { payment_limit: 2, ..GuardLimits::default() };
        let mut guard = MempoolGuard::new(limits);
        let sponsored =
            |h: u8, sender: Address, payer: Address| Admission { payer, ..self_pay(h, sender, 10) };
        let (full, open) = (addr(50), addr(51));
        for i in 0..2 {
            assert!(guard.try_admit(sponsored(i, addr(10 + i), full)).is_ok());
        }
        assert!(guard.try_admit(sponsored(5, addr(20), addr(52))).is_ok());

        // Rotating the fee bump to a payer at its cap is rejected; to one with
        // room it is accepted.
        assert_eq!(
            guard.check_replacement(&hash(5), &sponsored(6, addr(20), full)),
            Err(LimitRejection::PaymentLimit)
        );
        assert_eq!(guard.check_replacement(&hash(5), &sponsored(6, addr(20), open)), Ok(()));
        // Switching to self-pay charges the sender's own payment count.
        assert_eq!(guard.check_replacement(&hash(5), &self_pay(6, addr(20), 10)), Ok(()));
        // An untracked replaced hash is admitted through `try_admit` instead.
        assert_eq!(guard.check_replacement(&hash(9), &sponsored(6, addr(20), full)), Ok(()));
    }

    #[test]
    fn replacement_naming_a_new_booked_payer_must_fit_its_balance() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(70);
        let booked = |h: u8, sender: Address, cost: u64| Admission {
            payer,
            payer_allowlisted: true,
            payer_balance: U256::from(100u64),
            max_cost: U256::from(cost),
            ..self_pay(h, sender, cost)
        };
        assert!(guard.try_admit(booked(1, addr(10), 80)).is_ok());
        assert!(guard.try_admit(self_pay(2, addr(11), 10)).is_ok());

        assert_eq!(
            guard.check_replacement(&hash(2), &booked(3, addr(11), 30)),
            Err(LimitRejection::PayerBalance)
        );
        assert_eq!(guard.check_replacement(&hash(2), &booked(3, addr(11), 20)), Ok(()));
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
    fn insert_forced_allowlisted_replacement_tracks_in_book() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);
        let make = |h: u8, cost: u64| Admission {
            payer,
            payer_allowlisted: true,
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

    /// An allowlisted payer's forced replacement that overshoots its book falls
    /// back to the per-transaction threshold path, still charged to its count,
    /// and stays evictable by a balance drop.
    #[test]
    fn insert_forced_allowlisted_replacement_overshooting_balance_falls_back_to_count_path() {
        let payer = addr(9);
        let limits = GuardLimits { allowlisted_payment_limit: 2, ..GuardLimits::default() };
        let mut guard = MempoolGuard::new(limits);
        let make = |h: u8, cost: u64| Admission {
            payer,
            payer_allowlisted: true,
            payer_balance: U256::from(100u64),
            max_cost: U256::from(cost),
            watch_set: WatchSet::new().watch(InvalidationKey::Balance(payer)),
            ..self_pay(h, addr(h + 1), cost)
        };

        guard.try_admit(make(1, 60)).unwrap();
        assert!(guard.release(&hash(1)));
        guard.insert_forced(make(2, 150));
        assert!(guard.contains(&hash(2)));
        assert!(guard.payer_books.is_empty(), "the overshooting reservation never entered a book");

        // Still counted: one more fits under the cap of two, a third does not.
        guard.try_admit(make(3, 10)).unwrap();
        assert_eq!(guard.try_admit(make(4, 10)), Err(LimitRejection::PaymentLimit));

        // The fallback record is evictable by the per-transaction threshold.
        let dropped = guard.on_balance_changed(payer, U256::from(100u64));
        assert_eq!(dropped, vec![hash(2)]);
        guard.try_admit(make(5, 10)).unwrap();
    }

    /// A sender that pays for itself is bounded by its own payment limit.
    #[test]
    fn self_paying_sender_is_bounded_by_payment_limit() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let sender = addr(1);
        let exempt = |h: u8| Admission { ..self_pay(h, sender, 10) };
        for i in 0..DEFAULT_PAYMENT_LIMIT as u8 {
            guard.try_admit(exempt(i)).unwrap();
        }
        assert_eq!(guard.try_admit(exempt(200)), Err(LimitRejection::PaymentLimit));
    }

    #[test]
    fn balance_drop_evicts_lowest_priority_for_allowlisted_payer() {
        let mut guard = MempoolGuard::new(GuardLimits::default());
        let payer = addr(9);

        let make = |h: u8, cost: u64, priority: u128| Admission {
            payer,
            payer_allowlisted: true,
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
        let allowlisted_payer = addr(2);

        guard.try_admit(self_pay(1, sender, 10)).unwrap();
        guard
            .try_admit(Admission {
                payer: allowlisted_payer,
                payer_allowlisted: true,
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
                payer: allowlisted_payer,
                payer_allowlisted: true,
                payer_balance: U256::from(100u64),
                ..self_pay(4, sender, 20)
            })
            .unwrap();
    }
}
