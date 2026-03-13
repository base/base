//! Transaction send state tracking.
//!
//! [`SendState`] is the state machine the send loop uses to decide whether to
//! continue retrying, bump fees, or abort. All mutable fields are behind a
//! [`std::sync::Mutex`] (not `tokio`) since critical sections are CPU-bound
//! with no `.await` points.

use std::{collections::HashSet, sync::Mutex, time::Instant};

use alloy_primitives::B256;

use crate::{TxManagerError, TxManagerResult};

/// Tracks the publication state of a single logical transaction through its
/// lifecycle.
///
/// `SendState` is the state machine the send loop uses to decide whether to
/// continue retrying, bump fees, or abort. It tracks mined transaction hashes,
/// nonce-too-low error counts, mempool deadline expiry, successful publish
/// counts, fee bump state, and the already-reserved flag.
///
/// All mutable fields are wrapped in a [`std::sync::Mutex`] (not `tokio`)
/// since all critical sections are CPU-bound with no `.await` points.
#[derive(Debug)]
pub struct SendState {
    /// Mutable interior state protected by a std Mutex.
    inner: Mutex<SendStateInner>,
    /// Number of nonce-too-low errors before the send loop aborts.
    /// Immutable after construction.
    safe_abort_nonce_too_low_count: u64,
}

/// Interior mutable state for [`SendState`], protected by a [`Mutex`].
///
/// This type is intentionally private — it is an implementation detail behind
/// the Mutex with no standalone meaning.
#[derive(Debug)]
struct SendStateInner {
    /// Hashes of transactions that have been observed onchain.
    mined_txs: HashSet<B256>,
    /// Number of times the transaction was successfully published.
    successful_publish_count: u64,
    /// Number of nonce-too-low errors encountered.
    nonce_too_low_count: u64,
    /// Whether the nonce slot was already reserved by another sender.
    already_reserved: bool,
    /// Whether the next send attempt should bump fees.
    bump_fees: bool,
    /// Number of fee bumps performed.
    bump_count: u64,
    /// Optional deadline for mempool inclusion.
    mempool_deadline: Option<Instant>,
}

impl SendState {
    /// Creates a new `SendState` with the given nonce-too-low abort threshold.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::InvalidSafeAbortNonceTooLowCount`] if
    /// `safe_abort_nonce_too_low_count` is 0. A zero threshold would cause the
    /// send loop to abort on the very first nonce-too-low error after a
    /// successful publish, making fee bumps impossible.
    pub fn new(safe_abort_nonce_too_low_count: u64) -> TxManagerResult<Self> {
        if safe_abort_nonce_too_low_count == 0 {
            return Err(TxManagerError::InvalidSafeAbortNonceTooLowCount);
        }
        Ok(Self {
            inner: Mutex::new(SendStateInner {
                mined_txs: HashSet::new(),
                successful_publish_count: 0,
                nonce_too_low_count: 0,
                already_reserved: false,
                bump_fees: false,
                bump_count: 0,
                mempool_deadline: None,
            }),
            safe_abort_nonce_too_low_count,
        })
    }

    /// Processes a send error, updating internal state accordingly.
    ///
    /// - [`TxManagerError::NonceTooLow`] increments the nonce-too-low counter.
    /// - [`TxManagerError::AlreadyReserved`] sets the already-reserved flag.
    /// - Any [retryable](TxManagerError::is_retryable) error sets the
    ///   bump-fees flag for the next send attempt.
    /// - Other critical errors are no-ops (handled at a higher level).
    ///
    /// Note: `NonceTooLow` is not retryable, so the nonce-too-low branch and
    /// the bump-fees branch are mutually exclusive.
    pub fn process_send_error(&self, err: &TxManagerError) {
        let mut inner = self.inner.lock().expect("SendState mutex poisoned");
        match err {
            TxManagerError::NonceTooLow => {
                inner.nonce_too_low_count += 1;
            }
            TxManagerError::AlreadyReserved => {
                inner.already_reserved = true;
            }
            e if e.is_retryable() => {
                inner.bump_fees = true;
            }
            _ => {}
        }
    }

    /// Records that a transaction with the given hash has been observed
    /// onchain.
    pub fn tx_mined(&self, tx_hash: B256) {
        let mut inner = self.inner.lock().expect("SendState mutex poisoned");
        inner.mined_txs.insert(tx_hash);
    }

    /// Records that a previously-mined transaction is no longer onchain
    /// (e.g., after a reorg).
    ///
    /// If the hash was actually present and `mined_txs` becomes empty after
    /// removal, the nonce-too-low counter is reset to 0. This prevents false
    /// aborts after a reorg removes all confirmations while ignoring no-op
    /// removals of hashes that were never tracked.
    pub fn tx_not_mined(&self, tx_hash: B256) {
        let mut inner = self.inner.lock().expect("SendState mutex poisoned");
        let was_present = inner.mined_txs.remove(&tx_hash);
        if was_present && inner.mined_txs.is_empty() {
            inner.nonce_too_low_count = 0;
        }
    }

    /// Returns the critical error that should cause the send loop to abort, or
    /// `None` if sending should continue.
    ///
    /// Conditions are checked in priority order:
    /// 1. If any transaction is mined, returns `None` (wait for confirmation).
    /// 2. If the nonce slot was already reserved, returns
    ///    [`TxManagerError::AlreadyReserved`].
    /// 3. If no successful publish has occurred and a nonce-too-low error was
    ///    seen, returns [`TxManagerError::NonceTooLow`] (immediate abort).
    /// 4. If nonce-too-low errors have reached the threshold, returns
    ///    [`TxManagerError::NonceTooLow`].
    /// 5. If the mempool deadline has expired, returns
    ///    [`TxManagerError::MempoolDeadlineExpired`].
    /// 6. Otherwise, returns `None`.
    #[must_use]
    pub fn critical_error(&self) -> Option<TxManagerError> {
        let inner = self.inner.lock().expect("SendState mutex poisoned");

        // 1. Mined tx suppression: a tx is onchain, wait for confirmation.
        if !inner.mined_txs.is_empty() {
            return None;
        }

        // 2. Nonce slot already reserved by another sender.
        if inner.already_reserved {
            return Some(TxManagerError::AlreadyReserved);
        }

        // 3. Pre-publish immediate abort: nonce consumed before any successful
        //    publish.
        if inner.successful_publish_count == 0 && inner.nonce_too_low_count > 0 {
            return Some(TxManagerError::NonceTooLow);
        }

        // 4. Nonce-too-low threshold reached after successful publishes.
        if inner.nonce_too_low_count >= self.safe_abort_nonce_too_low_count {
            return Some(TxManagerError::NonceTooLow);
        }

        // 5. Mempool deadline expired.
        if let Some(deadline) = inner.mempool_deadline
            && Instant::now() >= deadline
        {
            return Some(TxManagerError::MempoolDeadlineExpired);
        }

        // 6. No critical error — continue sending.
        None
    }

    /// Returns `true` when there are mined transactions awaiting confirmation.
    #[must_use]
    pub fn is_waiting_for_confirmation(&self) -> bool {
        let inner = self.inner.lock().expect("SendState mutex poisoned");
        !inner.mined_txs.is_empty()
    }

    /// Records a successful transaction publication.
    pub fn record_successful_publish(&self) {
        let mut inner = self.inner.lock().expect("SendState mutex poisoned");
        inner.successful_publish_count += 1;
    }

    /// Records that a fee bump was performed, incrementing the bump counter
    /// and clearing the bump-fees flag.
    pub fn record_fee_bump(&self) {
        let mut inner = self.inner.lock().expect("SendState mutex poisoned");
        inner.bump_count += 1;
        inner.bump_fees = false;
    }

    /// Returns `true` if the next send attempt should bump fees.
    #[must_use]
    pub fn should_bump_fees(&self) -> bool {
        let inner = self.inner.lock().expect("SendState mutex poisoned");
        inner.bump_fees
    }

    /// Clears the bump-fees flag without incrementing the bump counter.
    ///
    /// Called before attempting a fee bump so that a failed attempt
    /// (e.g., RPC timeout in [`suggest_gas_price_caps`]) does not
    /// immediately re-trigger the bump on the next loop iteration. If
    /// the bump succeeds, [`record_fee_bump`] will also clear the flag
    /// (a harmless no-op) and increment the counter. If a new retryable
    /// error occurs later, [`process_send_error`] will re-set the flag.
    ///
    /// [`suggest_gas_price_caps`]: crate::SimpleTxManager::suggest_gas_price_caps
    /// [`record_fee_bump`]: Self::record_fee_bump
    /// [`process_send_error`]: Self::process_send_error
    pub fn clear_bump_fees(&self) {
        let mut inner = self.inner.lock().expect("SendState mutex poisoned");
        inner.bump_fees = false;
    }

    /// Sets the mempool inclusion deadline.
    pub fn set_mempool_deadline(&self, deadline: Instant) {
        let mut inner = self.inner.lock().expect("SendState mutex poisoned");
        inner.mempool_deadline = Some(deadline);
    }

    /// Returns the number of fee bumps performed so far.
    #[must_use]
    pub fn bump_count(&self) -> u64 {
        let inner = self.inner.lock().expect("SendState mutex poisoned");
        inner.bump_count
    }

    /// Returns the number of successful transaction publications.
    #[must_use]
    pub fn successful_publish_count(&self) -> u64 {
        let inner = self.inner.lock().expect("SendState mutex poisoned");
        inner.successful_publish_count
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rstest::rstest;

    use super::*;

    // ── Constructor validation ──────────────────────────────────────────

    #[test]
    fn constructor_rejects_zero_threshold() {
        assert_eq!(
            SendState::new(0).unwrap_err(),
            TxManagerError::InvalidSafeAbortNonceTooLowCount,
        );
    }

    #[rstest]
    #[case::one(1)]
    #[case::ten(10)]
    #[case::max(u64::MAX)]
    fn constructor_accepts_positive_threshold(#[case] count: u64) {
        let state = SendState::new(count).unwrap();
        assert!(state.critical_error().is_none());
    }

    // ── Fresh state ─────────────────────────────────────────────────────

    #[test]
    fn fresh_state_has_no_critical_error() {
        let state = SendState::new(3).unwrap();
        assert!(state.critical_error().is_none());
    }

    #[test]
    fn fresh_state_is_not_waiting_for_confirmation() {
        let state = SendState::new(3).unwrap();
        assert!(!state.is_waiting_for_confirmation());
    }

    #[test]
    fn fresh_state_should_not_bump_fees() {
        let state = SendState::new(3).unwrap();
        assert!(!state.should_bump_fees());
    }

    #[test]
    fn fresh_state_bump_count_is_zero() {
        let state = SendState::new(3).unwrap();
        assert_eq!(state.bump_count(), 0);
    }

    // ── Nonce-too-low threshold ─────────────────────────────────────────

    #[test]
    fn nonce_too_low_below_threshold_no_abort() {
        let state = SendState::new(3).unwrap();
        state.record_successful_publish();
        state.process_send_error(&TxManagerError::NonceTooLow);
        state.process_send_error(&TxManagerError::NonceTooLow);
        assert!(state.critical_error().is_none());
    }

    #[test]
    fn nonce_too_low_at_threshold_aborts() {
        let state = SendState::new(3).unwrap();
        state.record_successful_publish();
        for _ in 0..3 {
            state.process_send_error(&TxManagerError::NonceTooLow);
        }
        assert_eq!(state.critical_error(), Some(TxManagerError::NonceTooLow));
    }

    #[test]
    fn nonce_too_low_above_threshold_aborts() {
        let state = SendState::new(3).unwrap();
        state.record_successful_publish();
        for _ in 0..5 {
            state.process_send_error(&TxManagerError::NonceTooLow);
        }
        assert_eq!(state.critical_error(), Some(TxManagerError::NonceTooLow));
    }

    // ── Mined tx suppression ────────────────────────────────────────────

    #[test]
    fn mined_tx_suppresses_nonce_too_low_abort() {
        let state = SendState::new(3).unwrap();
        state.record_successful_publish();
        // Accumulate errors past threshold.
        for _ in 0..5 {
            state.process_send_error(&TxManagerError::NonceTooLow);
        }
        assert_eq!(state.critical_error(), Some(TxManagerError::NonceTooLow));

        // Mine a tx — critical_error should now return None.
        state.tx_mined(B256::with_last_byte(1));
        assert!(state.critical_error().is_none());
    }

    #[test]
    fn mined_tx_suppresses_already_reserved_abort() {
        let state = SendState::new(3).unwrap();
        state.process_send_error(&TxManagerError::AlreadyReserved);
        assert_eq!(state.critical_error(), Some(TxManagerError::AlreadyReserved));

        state.tx_mined(B256::with_last_byte(1));
        assert!(state.critical_error().is_none());
    }

    #[test]
    fn mined_tx_suppresses_mempool_deadline_abort() {
        let state = SendState::new(3).unwrap();
        state.set_mempool_deadline(Instant::now() - Duration::from_secs(1));
        assert_eq!(state.critical_error(), Some(TxManagerError::MempoolDeadlineExpired));

        state.tx_mined(B256::with_last_byte(1));
        assert!(state.critical_error().is_none());
    }

    // ── tx_not_mined reset ──────────────────────────────────────────────

    #[test]
    fn tx_not_mined_resets_nonce_count_when_empty() {
        let state = SendState::new(3).unwrap();
        state.record_successful_publish();

        let tx = B256::with_last_byte(1);
        state.tx_mined(tx);

        // Accumulate errors while mined.
        for _ in 0..5 {
            state.process_send_error(&TxManagerError::NonceTooLow);
        }
        // Still suppressed by mined tx.
        assert!(state.critical_error().is_none());

        // Un-mine: counter should reset.
        state.tx_not_mined(tx);
        assert!(state.critical_error().is_none());

        // Need fresh errors to re-trigger.
        for _ in 0..3 {
            state.process_send_error(&TxManagerError::NonceTooLow);
        }
        assert_eq!(state.critical_error(), Some(TxManagerError::NonceTooLow));
    }

    // ── Multiple mined txs ──────────────────────────────────────────────

    #[test]
    fn removing_one_of_multiple_mined_txs_does_not_reset() {
        let state = SendState::new(3).unwrap();
        state.record_successful_publish();

        let tx1 = B256::with_last_byte(1);
        let tx2 = B256::with_last_byte(2);
        state.tx_mined(tx1);
        state.tx_mined(tx2);

        for _ in 0..5 {
            state.process_send_error(&TxManagerError::NonceTooLow);
        }

        // Remove one — still has another mined tx, no reset.
        state.tx_not_mined(tx1);
        assert!(state.critical_error().is_none());
        assert!(state.is_waiting_for_confirmation());
    }

    #[test]
    fn tx_not_mined_with_never_mined_hash_does_not_reset_counter() {
        let state = SendState::new(3).unwrap();
        state.record_successful_publish();

        for _ in 0..5 {
            state.process_send_error(&TxManagerError::NonceTooLow);
        }
        assert_eq!(state.critical_error(), Some(TxManagerError::NonceTooLow));

        // Removing a hash that was never mined is a no-op — the counter
        // is preserved and the abort remains valid.
        state.tx_not_mined(B256::with_last_byte(99));
        assert_eq!(state.critical_error(), Some(TxManagerError::NonceTooLow));
    }

    #[test]
    fn removing_last_mined_tx_resets_counter() {
        let state = SendState::new(3).unwrap();
        state.record_successful_publish();

        let tx1 = B256::with_last_byte(1);
        let tx2 = B256::with_last_byte(2);
        state.tx_mined(tx1);
        state.tx_mined(tx2);

        for _ in 0..5 {
            state.process_send_error(&TxManagerError::NonceTooLow);
        }

        state.tx_not_mined(tx1);
        state.tx_not_mined(tx2);

        // Counter was reset — no abort yet.
        assert!(state.critical_error().is_none());
    }

    // ── Mempool deadline ────────────────────────────────────────────────

    #[test]
    fn expired_mempool_deadline_triggers_abort() {
        let state = SendState::new(3).unwrap();
        state.set_mempool_deadline(Instant::now() - Duration::from_secs(1));
        assert_eq!(state.critical_error(), Some(TxManagerError::MempoolDeadlineExpired));
    }

    #[test]
    fn future_mempool_deadline_does_not_abort() {
        let state = SendState::new(3).unwrap();
        state.set_mempool_deadline(Instant::now() + Duration::from_secs(60));
        assert!(state.critical_error().is_none());
    }

    #[test]
    fn no_mempool_deadline_does_not_abort() {
        let state = SendState::new(3).unwrap();
        assert!(state.critical_error().is_none());
    }

    // ── Pre-publish immediate abort ─────────────────────────────────────

    #[test]
    fn pre_publish_nonce_too_low_triggers_immediate_abort() {
        let state = SendState::new(3).unwrap();
        // No successful publish recorded.
        state.process_send_error(&TxManagerError::NonceTooLow);
        assert_eq!(state.critical_error(), Some(TxManagerError::NonceTooLow));
    }

    // ── Post-publish gradual abort ──────────────────────────────────────

    #[test]
    fn post_publish_nonce_too_low_requires_threshold() {
        let state = SendState::new(3).unwrap();
        state.record_successful_publish();

        // Single nonce-too-low should NOT abort after a successful publish.
        state.process_send_error(&TxManagerError::NonceTooLow);
        assert!(state.critical_error().is_none());

        // Need to reach the threshold.
        state.process_send_error(&TxManagerError::NonceTooLow);
        assert!(state.critical_error().is_none());

        state.process_send_error(&TxManagerError::NonceTooLow);
        assert_eq!(state.critical_error(), Some(TxManagerError::NonceTooLow));
    }

    // ── AlreadyReserved ─────────────────────────────────────────────────

    #[test]
    fn already_reserved_triggers_abort() {
        let state = SendState::new(3).unwrap();
        state.process_send_error(&TxManagerError::AlreadyReserved);
        assert_eq!(state.critical_error(), Some(TxManagerError::AlreadyReserved));
    }

    // ── Priority ordering ────────────────────────────────────────────────

    #[test]
    fn already_reserved_takes_priority_over_expired_deadline() {
        let state = SendState::new(3).unwrap();
        state.process_send_error(&TxManagerError::AlreadyReserved);
        state.set_mempool_deadline(Instant::now() - Duration::from_secs(1));

        // AlreadyReserved (priority 2) wins over MempoolDeadlineExpired
        // (priority 5).
        assert_eq!(state.critical_error(), Some(TxManagerError::AlreadyReserved));
    }

    #[test]
    fn pre_publish_nonce_too_low_takes_priority_over_expired_deadline() {
        let state = SendState::new(3).unwrap();
        // No successful publish.
        state.process_send_error(&TxManagerError::NonceTooLow);
        state.set_mempool_deadline(Instant::now() - Duration::from_secs(1));

        // Pre-publish NonceTooLow (priority 3) wins over
        // MempoolDeadlineExpired (priority 5).
        assert_eq!(state.critical_error(), Some(TxManagerError::NonceTooLow));
    }

    // ── tx_mined idempotency ──────────────────────────────────────────────

    #[test]
    fn tx_mined_is_idempotent() {
        let state = SendState::new(3).unwrap();
        let tx = B256::with_last_byte(1);

        // Mining the same hash twice should not double-count.
        state.tx_mined(tx);
        state.tx_mined(tx);
        assert!(state.is_waiting_for_confirmation());

        // A single tx_not_mined clears it.
        state.tx_not_mined(tx);
        assert!(!state.is_waiting_for_confirmation());
    }

    // ── Fee bump flags ──────────────────────────────────────────────────

    #[rstest]
    #[case::underpriced(TxManagerError::Underpriced)]
    #[case::replacement_underpriced(TxManagerError::ReplacementUnderpriced)]
    #[case::fee_too_low(TxManagerError::FeeTooLow)]
    #[case::max_fee_too_low(TxManagerError::MaxFeePerGasTooLow)]
    #[case::already_known(TxManagerError::AlreadyKnown)]
    #[case::rpc(TxManagerError::Rpc("any rpc error".to_string()))]
    fn retryable_error_sets_bump_fees(#[case] err: TxManagerError) {
        let state = SendState::new(3).unwrap();
        assert!(!state.should_bump_fees());
        state.process_send_error(&err);
        assert!(state.should_bump_fees());
    }

    #[test]
    fn record_fee_bump_clears_flag_and_increments_count() {
        let state = SendState::new(3).unwrap();
        state.process_send_error(&TxManagerError::Underpriced);
        assert!(state.should_bump_fees());
        assert_eq!(state.bump_count(), 0);

        state.record_fee_bump();
        assert!(!state.should_bump_fees());
        assert_eq!(state.bump_count(), 1);
    }

    #[test]
    fn nonce_too_low_does_not_set_bump_fees() {
        let state = SendState::new(3).unwrap();
        state.process_send_error(&TxManagerError::NonceTooLow);
        assert!(!state.should_bump_fees());
    }

    #[test]
    fn non_retryable_error_does_not_set_bump_fees() {
        let state = SendState::new(3).unwrap();
        state.process_send_error(&TxManagerError::InsufficientFunds);
        assert!(!state.should_bump_fees());
    }

    #[test]
    fn clear_bump_fees_clears_flag_without_incrementing_count() {
        let state = SendState::new(3).unwrap();
        state.process_send_error(&TxManagerError::Underpriced);
        assert!(state.should_bump_fees());
        assert_eq!(state.bump_count(), 0);

        // clear_bump_fees clears the flag but does not touch the counter.
        state.clear_bump_fees();
        assert!(!state.should_bump_fees());
        assert_eq!(state.bump_count(), 0, "bump_count should not increment on clear");
    }

    #[test]
    fn clear_bump_fees_allows_flag_to_be_re_set() {
        let state = SendState::new(3).unwrap();
        state.process_send_error(&TxManagerError::Underpriced);
        assert!(state.should_bump_fees());

        state.clear_bump_fees();
        assert!(!state.should_bump_fees());

        // A new retryable error re-sets the flag.
        state.process_send_error(&TxManagerError::ReplacementUnderpriced);
        assert!(state.should_bump_fees());
    }

    // ── successful_publish_count ─────────────────────────────────────────

    #[test]
    fn successful_publish_count_tracks_publications() {
        let state = SendState::new(3).unwrap();
        assert_eq!(state.successful_publish_count(), 0);

        state.record_successful_publish();
        assert_eq!(state.successful_publish_count(), 1);

        state.record_successful_publish();
        state.record_successful_publish();
        assert_eq!(state.successful_publish_count(), 3);
    }

    // ── is_waiting_for_confirmation ─────────────────────────────────────

    #[test]
    fn is_waiting_for_confirmation_reflects_mined_txs() {
        let state = SendState::new(3).unwrap();
        assert!(!state.is_waiting_for_confirmation());

        let tx = B256::with_last_byte(42);
        state.tx_mined(tx);
        assert!(state.is_waiting_for_confirmation());

        state.tx_not_mined(tx);
        assert!(!state.is_waiting_for_confirmation());
    }

    // ── Send + Sync ─────────────────────────────────────────────────────

    #[test]
    fn send_state_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SendState>();
    }
}
