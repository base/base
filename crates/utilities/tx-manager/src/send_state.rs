//! Transaction send state tracking.
//!
//! [`SendState`] tracks publication progress and critical errors for the send
//! loop. All mutable fields use a [`std::sync::Mutex`] since critical sections
//! are CPU-bound with no `.await` points.

use std::{sync::Mutex, time::Duration};

use alloy_primitives::B256;

use crate::{TxManagerError, TxManagerResult};

/// Tracks the publication state of a single logical transaction through its
/// lifecycle: mined hashes, nonce errors, fee bumps, and mempool deadlines.
#[derive(Debug)]
pub struct SendState {
    /// Mutable interior state protected by a std Mutex.
    inner: Mutex<SendStateInner>,
    /// Number of nonce-too-low errors before the send loop aborts.
    /// Immutable after construction.
    safe_abort_nonce_too_low_count: u64,
}

#[derive(Debug)]
struct SendStateInner {
    mined_txs: Vec<B256>,
    has_published: bool,
    nonce_too_low_count: u64,
    bump_count: u64,
    mempool_deadline: Option<Duration>,
    assigned_nonce: Option<u64>,
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
    pub const fn new(safe_abort_nonce_too_low_count: u64) -> TxManagerResult<Self> {
        if safe_abort_nonce_too_low_count == 0 {
            return Err(TxManagerError::InvalidSafeAbortNonceTooLowCount);
        }
        Ok(Self {
            inner: Mutex::new(SendStateInner {
                mined_txs: Vec::new(),
                has_published: false,
                nonce_too_low_count: 0,
                bump_count: 0,
                mempool_deadline: None,
                assigned_nonce: None,
            }),
            safe_abort_nonce_too_low_count,
        })
    }

    /// Processes a send error, updating internal state accordingly.
    ///
    /// - [`TxManagerError::NonceTooLow`] increments the nonce-too-low counter.
    /// - Other errors leave state unchanged and are handled by the send loop.
    pub fn process_send_error(&self, err: &TxManagerError) {
        if matches!(err, TxManagerError::NonceTooLow) {
            let mut inner = self.inner.lock().expect("SendState mutex poisoned");
            inner.nonce_too_low_count += 1;
        }
    }

    /// Records that a transaction with the given hash has been observed
    /// onchain.
    pub fn tx_mined(&self, tx_hash: B256) {
        let mut inner = self.inner.lock().expect("SendState mutex poisoned");
        if !inner.mined_txs.contains(&tx_hash) {
            inner.mined_txs.push(tx_hash);
        }
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
        if let Some(idx) = inner.mined_txs.iter().position(|h| *h == tx_hash) {
            inner.mined_txs.swap_remove(idx);
            if inner.mined_txs.is_empty() {
                inner.nonce_too_low_count = 0;
            }
        }
    }

    /// Returns the critical error at the given runtime-relative timestamp, or
    /// `None` if sending should continue.
    ///
    /// Conditions are checked in priority order:
    /// 1. If any transaction is mined, returns `None` (wait for confirmation).
    /// 2. If no transaction has been published and a nonce-too-low error was
    ///    seen, returns [`TxManagerError::NonceTooLow`] (immediate abort).
    /// 3. If nonce-too-low errors have reached the threshold, returns
    ///    [`TxManagerError::NonceTooLow`].
    /// 4. If the mempool deadline has expired *and* no transaction has been
    ///    published, returns [`TxManagerError::MempoolDeadlineExpired`]. Once
    ///    a transaction is published it may be in the mempool and the
    ///    deadline's purpose is served. Further waiting is governed by
    ///    `tx_send_timeout` and the bump cycle, not by aborting (which would
    ///    leak a pre-reserved nonce and stall publication on lower nonces
    ///    still live on L1).
    /// 5. Otherwise, returns `None`.
    ///
    /// A transaction counts as published once [`record_publish`](Self::record_publish)
    /// was called for it.
    #[must_use]
    pub fn critical_error_at(&self, now: Duration) -> Option<TxManagerError> {
        let inner = self.inner.lock().expect("SendState mutex poisoned");

        if !inner.mined_txs.is_empty() {
            return None;
        }
        if !inner.has_published && inner.nonce_too_low_count > 0 {
            return Some(TxManagerError::NonceTooLow);
        }
        if inner.nonce_too_low_count >= self.safe_abort_nonce_too_low_count {
            return Some(TxManagerError::NonceTooLow);
        }
        if !inner.has_published
            && let Some(deadline) = inner.mempool_deadline
            && now >= deadline
        {
            return Some(TxManagerError::MempoolDeadlineExpired);
        }

        None
    }

    /// Returns `true` when there are mined transactions awaiting confirmation.
    #[must_use]
    pub fn is_waiting_for_confirmation(&self) -> bool {
        let inner = self.inner.lock().expect("SendState mutex poisoned");
        !inner.mined_txs.is_empty()
    }

    /// Records that a transaction may have reached the node: the node accepted
    /// it, or its publish got no answer (a timeout or a transport failure).
    pub fn record_publish(&self) {
        let mut inner = self.inner.lock().expect("SendState mutex poisoned");
        inner.has_published = true;
    }

    /// Records that a fee bump was performed, incrementing the bump counter
    /// and returning the new count.
    pub fn record_fee_bump(&self) -> u64 {
        let mut inner = self.inner.lock().expect("SendState mutex poisoned");
        inner.bump_count += 1;
        inner.bump_count
    }

    /// Sets the runtime-relative mempool inclusion deadline.
    ///
    /// `absolute_deadline` is an absolute timestamp from the runtime clock,
    /// not a relative timeout duration. Callers should pass `runtime.now() +
    /// timeout`.
    pub fn set_runtime_mempool_deadline(&self, absolute_deadline: Duration) {
        let mut inner = self.inner.lock().expect("SendState mutex poisoned");
        inner.mempool_deadline = Some(absolute_deadline);
    }

    /// Returns `true` if at least one transaction was published, see
    /// [`record_publish`](Self::record_publish).
    #[must_use]
    pub fn has_published(&self) -> bool {
        let inner = self.inner.lock().expect("SendState mutex poisoned");
        inner.has_published
    }

    /// Records the nonce assigned to this send after signing.
    pub fn record_assigned_nonce(&self, nonce: u64) {
        let mut inner = self.inner.lock().expect("SendState mutex poisoned");
        inner.assigned_nonce = Some(nonce);
    }

    /// Returns the nonce assigned after signing, if preparation succeeded.
    #[must_use]
    pub fn assigned_nonce(&self) -> Option<u64> {
        let inner = self.inner.lock().expect("SendState mutex poisoned");
        inner.assigned_nonce
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
        assert!(state.critical_error_at(Duration::ZERO).is_none());
    }

    // ── Fresh state ─────────────────────────────────────────────────────

    #[test]
    fn fresh_state_has_no_critical_error() {
        let state = SendState::new(3).unwrap();
        assert!(state.critical_error_at(Duration::ZERO).is_none());
    }

    #[test]
    fn fresh_state_is_not_waiting_for_confirmation() {
        let state = SendState::new(3).unwrap();
        assert!(!state.is_waiting_for_confirmation());
    }

    #[test]
    fn fresh_state_has_not_published() {
        let state = SendState::new(3).unwrap();
        assert!(!state.has_published());
    }

    // ── Nonce-too-low threshold ─────────────────────────────────────────

    #[test]
    fn nonce_too_low_below_threshold_no_abort() {
        let state = SendState::new(3).unwrap();
        state.record_publish();
        state.process_send_error(&TxManagerError::NonceTooLow);
        state.process_send_error(&TxManagerError::NonceTooLow);
        assert!(state.critical_error_at(Duration::ZERO).is_none());
    }

    #[test]
    fn nonce_too_low_at_threshold_aborts() {
        let state = SendState::new(3).unwrap();
        state.record_publish();
        for _ in 0..3 {
            state.process_send_error(&TxManagerError::NonceTooLow);
        }
        assert_eq!(state.critical_error_at(Duration::ZERO), Some(TxManagerError::NonceTooLow));
    }

    #[test]
    fn nonce_too_low_above_threshold_aborts() {
        let state = SendState::new(3).unwrap();
        state.record_publish();
        for _ in 0..5 {
            state.process_send_error(&TxManagerError::NonceTooLow);
        }
        assert_eq!(state.critical_error_at(Duration::ZERO), Some(TxManagerError::NonceTooLow));
    }

    // ── Mined tx suppression ────────────────────────────────────────────

    #[test]
    fn mined_tx_suppresses_nonce_too_low_abort() {
        let state = SendState::new(3).unwrap();
        state.record_publish();
        // Accumulate errors past threshold.
        for _ in 0..5 {
            state.process_send_error(&TxManagerError::NonceTooLow);
        }
        assert_eq!(state.critical_error_at(Duration::ZERO), Some(TxManagerError::NonceTooLow));

        state.tx_mined(B256::with_last_byte(1));
        assert!(state.critical_error_at(Duration::ZERO).is_none());
    }

    #[test]
    fn mined_tx_suppresses_mempool_deadline_abort() {
        let state = SendState::new(3).unwrap();
        state.set_runtime_mempool_deadline(Duration::ZERO);
        assert_eq!(
            state.critical_error_at(Duration::ZERO),
            Some(TxManagerError::MempoolDeadlineExpired)
        );

        state.tx_mined(B256::with_last_byte(1));
        assert!(state.critical_error_at(Duration::ZERO).is_none());
    }

    // ── tx_not_mined reset ──────────────────────────────────────────────

    #[test]
    fn tx_not_mined_resets_nonce_count_when_empty() {
        let state = SendState::new(3).unwrap();
        state.record_publish();

        let tx = B256::with_last_byte(1);
        state.tx_mined(tx);

        // Accumulate errors while mined.
        for _ in 0..5 {
            state.process_send_error(&TxManagerError::NonceTooLow);
        }
        // Still suppressed by mined tx.
        assert!(state.critical_error_at(Duration::ZERO).is_none());

        // Un-mine: counter should reset.
        state.tx_not_mined(tx);
        assert!(state.critical_error_at(Duration::ZERO).is_none());

        // Need fresh errors to re-trigger.
        for _ in 0..3 {
            state.process_send_error(&TxManagerError::NonceTooLow);
        }
        assert_eq!(state.critical_error_at(Duration::ZERO), Some(TxManagerError::NonceTooLow));
    }

    // ── Multiple mined txs ──────────────────────────────────────────────

    #[test]
    fn removing_one_of_multiple_mined_txs_does_not_reset() {
        let state = SendState::new(3).unwrap();
        state.record_publish();

        let tx1 = B256::with_last_byte(1);
        let tx2 = B256::with_last_byte(2);
        state.tx_mined(tx1);
        state.tx_mined(tx2);

        for _ in 0..5 {
            state.process_send_error(&TxManagerError::NonceTooLow);
        }

        // Remove one — still has another mined tx, no reset.
        state.tx_not_mined(tx1);
        assert!(state.critical_error_at(Duration::ZERO).is_none());
        assert!(state.is_waiting_for_confirmation());
    }

    #[test]
    fn tx_not_mined_with_never_mined_hash_does_not_reset_counter() {
        let state = SendState::new(3).unwrap();
        state.record_publish();

        for _ in 0..5 {
            state.process_send_error(&TxManagerError::NonceTooLow);
        }
        assert_eq!(state.critical_error_at(Duration::ZERO), Some(TxManagerError::NonceTooLow));

        // Removing a hash that was never mined is a no-op — the counter
        // is preserved and the abort remains valid.
        state.tx_not_mined(B256::with_last_byte(99));
        assert_eq!(state.critical_error_at(Duration::ZERO), Some(TxManagerError::NonceTooLow));
    }

    #[test]
    fn removing_last_mined_tx_resets_counter() {
        let state = SendState::new(3).unwrap();
        state.record_publish();

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
        assert!(state.critical_error_at(Duration::ZERO).is_none());
    }

    // ── Mempool deadline ────────────────────────────────────────────────

    #[test]
    fn expired_mempool_deadline_triggers_abort() {
        let state = SendState::new(3).unwrap();
        state.set_runtime_mempool_deadline(Duration::ZERO);
        assert_eq!(
            state.critical_error_at(Duration::ZERO),
            Some(TxManagerError::MempoolDeadlineExpired)
        );
    }

    #[test]
    fn future_mempool_deadline_does_not_abort() {
        let state = SendState::new(3).unwrap();
        state.set_runtime_mempool_deadline(Duration::from_secs(60));
        assert!(state.critical_error_at(Duration::ZERO).is_none());
    }

    #[test]
    fn no_mempool_deadline_does_not_abort() {
        let state = SendState::new(3).unwrap();
        assert!(state.critical_error_at(Duration::ZERO).is_none());
    }

    #[test]
    fn expired_mempool_deadline_after_publish_does_not_abort() {
        // Once a transaction has been successfully published it is in the
        // mempool, so the deadline that guards "tx never reached mempool"
        // must no longer fire. Otherwise the send loop aborts while the
        // pre-reserved nonce stays live on L1, stalling subsequent sends.
        let state = SendState::new(3).unwrap();
        state.set_runtime_mempool_deadline(Duration::ZERO);
        state.record_publish();
        assert!(state.critical_error_at(Duration::ZERO).is_none());
    }

    #[test]
    fn runtime_mempool_deadline_expires_at_exact_timestamp() {
        let state = SendState::new(3).unwrap();
        state.set_runtime_mempool_deadline(Duration::from_secs(1));

        assert!(state.critical_error_at(Duration::ZERO).is_none());
        assert_eq!(
            state.critical_error_at(Duration::from_secs(1)),
            Some(TxManagerError::MempoolDeadlineExpired),
        );
    }

    // ── Pre-publish immediate abort ─────────────────────────────────────

    #[test]
    fn pre_publish_nonce_too_low_triggers_immediate_abort() {
        let state = SendState::new(3).unwrap();
        // No successful publish recorded.
        state.process_send_error(&TxManagerError::NonceTooLow);
        assert_eq!(state.critical_error_at(Duration::ZERO), Some(TxManagerError::NonceTooLow));
    }

    // ── Post-publish gradual abort ──────────────────────────────────────

    #[test]
    fn post_publish_nonce_too_low_requires_threshold() {
        let state = SendState::new(3).unwrap();
        state.record_publish();

        // Single nonce-too-low should NOT abort after a successful publish.
        state.process_send_error(&TxManagerError::NonceTooLow);
        assert!(state.critical_error_at(Duration::ZERO).is_none());

        // Need to reach the threshold.
        state.process_send_error(&TxManagerError::NonceTooLow);
        assert!(state.critical_error_at(Duration::ZERO).is_none());

        state.process_send_error(&TxManagerError::NonceTooLow);
        assert_eq!(state.critical_error_at(Duration::ZERO), Some(TxManagerError::NonceTooLow));
    }

    // ── NonceTooHigh ───────────────────────────────────────────────────

    #[test]
    fn nonce_too_high_is_transient() {
        let state = SendState::new(3).unwrap();
        state.process_send_error(&TxManagerError::NonceTooHigh);
        assert!(state.critical_error_at(Duration::ZERO).is_none());
    }

    // ── Priority ordering ────────────────────────────────────────────────

    #[test]
    fn pre_publish_nonce_too_low_takes_priority_over_expired_deadline() {
        let state = SendState::new(3).unwrap();
        // No successful publish.
        state.process_send_error(&TxManagerError::NonceTooLow);
        state.set_runtime_mempool_deadline(Duration::ZERO);

        // Pre-publish NonceTooLow (priority 2) wins over
        // MempoolDeadlineExpired (priority 4).
        assert_eq!(state.critical_error_at(Duration::ZERO), Some(TxManagerError::NonceTooLow));
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

    #[test]
    fn record_fee_bump_increments_and_returns_count() {
        let state = SendState::new(3).unwrap();
        assert_eq!(state.record_fee_bump(), 1);
        assert_eq!(state.record_fee_bump(), 2);
    }

    // ── has_published ───────────────────────────────────────────────────

    #[test]
    fn has_published_tracks_publication() {
        let state = SendState::new(3).unwrap();
        assert!(!state.has_published());

        state.record_publish();
        assert!(state.has_published());

        // Idempotent.
        state.record_publish();
        assert!(state.has_published());
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
