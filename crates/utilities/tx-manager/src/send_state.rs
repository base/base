//! Transaction send state tracking.

use std::collections::HashSet;
use std::sync::Mutex;

use alloy_primitives::B256;

use crate::TxManagerError;

/// Maximum consecutive nonce-too-low errors before a critical error is raised.
const NONCE_TOO_LOW_THRESHOLD: u64 = 3;

/// Tracks the state of a transaction through its lifecycle.
///
/// All mutable fields live behind a [`Mutex`] so that `SendState` can be
/// shared across tasks without requiring `&mut self`. The critical sections
/// are CPU-bound (counter increments and `HashSet` operations) and never
/// hold the lock across `.await` points.
#[derive(Debug)]
pub struct SendState {
    /// Interior-mutable state protected by a standard mutex.
    inner: Mutex<SendStateInner>,
}

/// Inner mutable state for [`SendState`].
#[derive(Debug, Default)]
struct SendStateInner {
    /// Transaction hashes that have been mined.
    mined_txs: HashSet<B256>,
    /// Number of successfully published transactions.
    successful_publish_count: u64,
    /// Consecutive nonce-too-low errors observed.
    nonce_too_low_count: u64,
    /// Whether the nonce slot was already reserved by another sender.
    already_reserved: bool,
    /// Whether fees should be bumped on the next attempt.
    bump_fees: bool,
    /// Number of fee bumps performed.
    bump_count: u64,
}

impl SendState {
    /// Creates a new `SendState` with zeroed counters and an empty mined set.
    #[must_use]
    pub fn new() -> Self {
        Self { inner: Mutex::new(SendStateInner::default()) }
    }

    /// Updates internal counters based on the type of send error encountered.
    ///
    /// - [`TxManagerError::NonceTooLow`]: increments `nonce_too_low_count`.
    /// - [`TxManagerError::AlreadyReserved`]: sets `already_reserved`.
    /// - Retryable fee errors ([`Underpriced`](TxManagerError::Underpriced),
    ///   [`ReplacementUnderpriced`](TxManagerError::ReplacementUnderpriced),
    ///   [`FeeTooLow`](TxManagerError::FeeTooLow),
    ///   [`MaxFeePerGasTooLow`](TxManagerError::MaxFeePerGasTooLow)):
    ///   sets `bump_fees` to trigger a fee bump on the next attempt.
    pub fn process_send_error(&self, err: &TxManagerError) {
        let mut state = self.inner.lock().expect("send state lock poisoned");
        match err {
            TxManagerError::NonceTooLow => {
                state.nonce_too_low_count += 1;
            }
            TxManagerError::AlreadyReserved => {
                state.already_reserved = true;
            }
            TxManagerError::Underpriced
            | TxManagerError::ReplacementUnderpriced
            | TxManagerError::FeeTooLow
            | TxManagerError::MaxFeePerGasTooLow => {
                state.bump_fees = true;
            }
            _ => {}
        }
    }

    /// Records a transaction hash as mined and resets the nonce-too-low counter.
    pub fn tx_mined(&self, tx_hash: B256) {
        let mut state = self.inner.lock().expect("send state lock poisoned");
        state.mined_txs.insert(tx_hash);
        state.nonce_too_low_count = 0;
    }

    /// Removes a transaction hash from the mined set (e.g. on reorg).
    pub fn tx_not_mined(&self, tx_hash: B256) {
        let mut state = self.inner.lock().expect("send state lock poisoned");
        state.mined_txs.remove(&tx_hash);
    }

    /// Returns a critical error if internal thresholds have been breached.
    ///
    /// Returns [`TxManagerError::NonceTooLow`] when the nonce-too-low count
    /// exceeds the threshold, or [`TxManagerError::AlreadyReserved`] when
    /// the nonce slot was already reserved.
    #[must_use]
    pub fn critical_error(&self) -> Option<TxManagerError> {
        let state = self.inner.lock().expect("send state lock poisoned");
        if state.nonce_too_low_count >= NONCE_TOO_LOW_THRESHOLD {
            return Some(TxManagerError::NonceTooLow);
        }
        if state.already_reserved {
            return Some(TxManagerError::AlreadyReserved);
        }
        None
    }

    /// Returns `true` if at least one transaction has been mined and
    /// we are waiting for its confirmation.
    #[must_use]
    pub fn is_waiting_for_confirmation(&self) -> bool {
        let state = self.inner.lock().expect("send state lock poisoned");
        !state.mined_txs.is_empty()
    }

    /// Returns `true` if fees should be bumped on the next send attempt.
    #[must_use]
    pub fn should_bump_fees(&self) -> bool {
        let state = self.inner.lock().expect("send state lock poisoned");
        state.bump_fees
    }

    /// Returns the number of successfully published transactions.
    #[must_use]
    pub fn successful_publish_count(&self) -> u64 {
        let state = self.inner.lock().expect("send state lock poisoned");
        state.successful_publish_count
    }

    /// Returns the number of fee bumps performed.
    #[must_use]
    pub fn bump_count(&self) -> u64 {
        let state = self.inner.lock().expect("send state lock poisoned");
        state.bump_count
    }
}

impl Default for SendState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    // ── new ─────────────────────────────────────────────────────────────

    #[test]
    fn new_has_zeroed_state() {
        let state = SendState::new();
        assert!(!state.is_waiting_for_confirmation());
        assert!(state.critical_error().is_none());
    }

    // ── process_send_error ──────────────────────────────────────────────

    #[rstest]
    #[case::single_nonce_too_low(
        &[TxManagerError::NonceTooLow],
        None
    )]
    #[case::threshold_nonce_too_low(
        &[TxManagerError::NonceTooLow, TxManagerError::NonceTooLow, TxManagerError::NonceTooLow],
        Some(TxManagerError::NonceTooLow)
    )]
    #[case::already_reserved(
        &[TxManagerError::AlreadyReserved],
        Some(TxManagerError::AlreadyReserved)
    )]
    fn process_send_error_critical(
        #[case] errors: &[TxManagerError],
        #[case] expected: Option<TxManagerError>,
    ) {
        let state = SendState::new();
        for err in errors {
            state.process_send_error(err);
        }
        assert_eq!(state.critical_error(), expected);
    }

    // ── tx_mined / tx_not_mined ─────────────────────────────────────────

    #[test]
    fn tx_mined_tracks_hash() {
        let state = SendState::new();
        let hash = B256::with_last_byte(1);

        assert!(!state.is_waiting_for_confirmation());

        state.tx_mined(hash);
        assert!(state.is_waiting_for_confirmation());
    }

    #[test]
    fn tx_not_mined_removes_hash() {
        let state = SendState::new();
        let hash = B256::with_last_byte(1);

        state.tx_mined(hash);
        assert!(state.is_waiting_for_confirmation());

        state.tx_not_mined(hash);
        assert!(!state.is_waiting_for_confirmation());
    }

    #[test]
    fn tx_mined_resets_nonce_too_low_count() {
        let state = SendState::new();
        let hash = B256::with_last_byte(1);

        // Accumulate nonce-too-low errors just below threshold.
        state.process_send_error(&TxManagerError::NonceTooLow);
        state.process_send_error(&TxManagerError::NonceTooLow);
        assert!(state.critical_error().is_none());

        // Mining a tx resets the counter.
        state.tx_mined(hash);
        state.process_send_error(&TxManagerError::NonceTooLow);
        state.process_send_error(&TxManagerError::NonceTooLow);
        assert!(state.critical_error().is_none());
    }

    // ── is_waiting_for_confirmation ─────────────────────────────────────

    #[test]
    fn waiting_for_confirmation_multiple_hashes() {
        let state = SendState::new();
        let h1 = B256::with_last_byte(1);
        let h2 = B256::with_last_byte(2);

        state.tx_mined(h1);
        state.tx_mined(h2);
        assert!(state.is_waiting_for_confirmation());

        state.tx_not_mined(h1);
        assert!(state.is_waiting_for_confirmation());

        state.tx_not_mined(h2);
        assert!(!state.is_waiting_for_confirmation());
    }

    // ── Send + Sync ─────────────────────────────────────────────────────

    #[test]
    fn send_state_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SendState>();
    }
}
