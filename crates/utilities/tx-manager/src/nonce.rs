//! Nonce allocation and tracking.

use std::sync::Arc;

use alloy_primitives::Address;
use alloy_provider::{Provider, RootProvider};
#[cfg(test)]
use tokio::sync::Notify;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::{debug, info, warn};

use crate::TxManagerError;

/// Internal state tracked by [`NonceManager`].
///
/// Pairs the optional cached nonce with a generation counter that
/// distinguishes "never initialized" (`generation == 0`) from
/// "cleared by [`NonceManager::reset`]" (`generation > 0`).
#[derive(Debug)]
pub struct NonceState {
    /// The cached nonce value, or `None` if uninitialized / reset.
    nonce: Option<u64>,
    /// Monotonically increasing counter bumped on every
    /// [`NonceManager::reset`].
    ///
    /// Uses [`u64::wrapping_add`] so the counter cannot panic. A
    /// generation collision (wrapping all the way around to the same
    /// snapshot value) would require 2^64 calls to `reset()`, which
    /// is infeasible in practice.
    generation: u64,
}

impl NonceState {
    /// Creates a new [`NonceState`] with no cached nonce and generation
    /// zero.
    pub const fn new() -> Self {
        Self { nonce: None, generation: 0 }
    }

    /// Returns the cached nonce value, or `None` if uninitialized /
    /// reset.
    pub const fn nonce(&self) -> Option<u64> {
        self.nonce
    }

    /// Returns the current generation counter.
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

impl Default for NonceState {
    fn default() -> Self {
        Self::new()
    }
}

/// Synchronization barrier for deterministic race-condition testing.
///
/// Inserted between Phase 2 (RPC fetch) and Phase 3 (lock
/// re-acquisition) in [`NonceManager::next_nonce`] to give tests a
/// window to call [`NonceManager::reset`] and verify the
/// generation-mismatch retry logic.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct TestBarrier {
    /// Signalled by `next_nonce` after the RPC fetch completes, before
    /// re-acquiring the lock.
    pub(crate) fetch_completed: Notify,
    /// Awaited by `next_nonce` after signalling `fetch_completed`.
    /// The test signals this once it has finished mutating state.
    pub(crate) may_proceed: Notify,
}

#[cfg(test)]
impl TestBarrier {
    /// Creates a new [`TestBarrier`].
    pub(crate) fn new() -> Self {
        Self { fetch_completed: Notify::new(), may_proceed: Notify::new() }
    }
}

/// Manages nonce allocation and tracking.
///
/// Wraps a [`tokio::sync::Mutex`] around a [`NonceState`], lazily
/// fetching the initial nonce from chain state via
/// [`Provider::get_transaction_count`] on first use. Subsequent calls
/// increment locally without making RPC calls.
///
/// The mutex is held for the duration of transaction signing via the
/// returned [`NonceGuard`], ensuring sequential nonce assignment even
/// under concurrent access.
#[derive(Debug, Clone)]
pub struct NonceManager {
    inner: Arc<Mutex<NonceState>>,
    provider: RootProvider,
    address: Address,
    #[cfg(test)]
    test_barrier: Option<Arc<TestBarrier>>,
}

impl NonceManager {
    /// Creates a new [`NonceManager`] with no cached nonce.
    ///
    /// The first call to [`next_nonce`](Self::next_nonce) will fetch the
    /// current transaction count from the provider.
    pub fn new(provider: RootProvider, address: Address) -> Self {
        Self {
            inner: Arc::new(Mutex::new(NonceState::new())),
            provider,
            address,
            #[cfg(test)]
            test_barrier: None,
        }
    }

    /// Maximum number of retry attempts when `reset()` races with
    /// `next_nonce()`, clearing the cache between the RPC fetch and
    /// lock re-acquisition.
    const MAX_RETRY_ATTEMPTS: u8 = 5;

    /// Reserves the next nonce for transaction signing.
    ///
    /// On the first call (or after [`reset`](Self::reset)), fetches the
    /// current transaction count from the provider. Subsequent calls
    /// increment the cached value locally without making RPC calls.
    ///
    /// The RPC fetch (when needed) is performed without holding the
    /// lock, so concurrent callers are not blocked by the network
    /// round-trip. Once a nonce is reserved, the returned
    /// [`NonceGuard`] holds the lock for its entire lifetime,
    /// serializing all nonce assignment until the guard is dropped or
    /// rolled back.
    ///
    /// Returns a [`NonceGuard`] that holds the mutex lock for the duration
    /// of transaction signing. Drop the guard on success, or call
    /// [`NonceGuard::rollback`] on failure to restore the nonce.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::Rpc`] if the provider call fails,
    /// [`TxManagerError::NonceAcquisitionFailed`] if the nonce cache is
    /// repeatedly cleared by concurrent [`reset`](Self::reset) calls during
    /// acquisition, or [`TxManagerError::NonceOverflow`] if the nonce
    /// exceeds `u64::MAX`.
    pub async fn next_nonce(&self) -> Result<NonceGuard, TxManagerError> {
        for attempt in 0..Self::MAX_RETRY_ATTEMPTS {
            // Acquire the owned lock once upfront.
            let guard = Arc::clone(&self.inner).lock_owned().await;

            // Fast path: cache is populated — read-and-increment in a
            // single lock round-trip.
            if let Some(n) = guard.nonce {
                return Self::reserve_nonce(guard, n);
            }

            // Cache miss: snapshot the generation, then drop the lock so
            // the RPC fetch doesn't block concurrent callers.
            let generation = guard.generation;
            drop(guard);

            // Phase 2: fetch from chain *without* holding the lock so
            // concurrent callers are not blocked by the RPC round-trip.
            // Multiple concurrent callers may fetch redundantly; only
            // the first writer's value is used.
            let fetched = self.provider.get_transaction_count(self.address).await.map_err(|e| {
                warn!(
                    error = %e, address = %self.address,
                    "failed to fetch nonce from chain",
                );
                TxManagerError::Rpc(e.to_string())
            })?;

            // Test hook: pause between Phase 2 and Phase 3 so tests
            // can deterministically call reset() during this window.
            #[cfg(test)]
            if let Some(barrier) = &self.test_barrier {
                barrier.fetch_completed.notify_one();
                barrier.may_proceed.notified().await;
            }

            // Phase 3: re-acquire the lock and populate only if still
            // unset AND the generation has not changed. If reset()
            // was called mid-flight the generation will have been
            // bumped — discard the stale fetch and retry.
            let mut guard = Arc::clone(&self.inner).lock_owned().await;

            if guard.generation != generation {
                drop(guard);
                debug!(attempt, "nonce cache reset during RPC fetch, retrying");
                continue;
            }

            let nonce = *guard.nonce.get_or_insert_with(|| {
                debug!(nonce = fetched, "nonce fetched from chain");
                fetched
            });
            return Self::reserve_nonce(guard, nonce);
        }

        warn!(attempts = Self::MAX_RETRY_ATTEMPTS, "nonce acquisition failed after max retries",);
        Err(TxManagerError::NonceAcquisitionFailed)
    }

    /// Advances the cached nonce by one and returns a [`NonceGuard`]
    /// holding the lock and the reserved value.
    ///
    /// Both the fast path (cache hit) and Phase 3 (after RPC fetch) use
    /// this single call-site for the `checked_add` overflow check,
    /// ensuring consistent behavior.
    fn reserve_nonce(
        mut guard: OwnedMutexGuard<NonceState>,
        nonce: u64,
    ) -> Result<NonceGuard, TxManagerError> {
        let next = nonce.checked_add(1).ok_or(TxManagerError::NonceOverflow)?;
        guard.nonce = Some(next);
        debug!(nonce, "nonce reserved");
        Ok(NonceGuard { guard: Some(guard), nonce })
    }

    /// Clears the cached nonce, forcing a fresh chain fetch on the next
    /// call to [`next_nonce`](Self::next_nonce).
    ///
    /// # Caution
    ///
    /// The fresh fetch uses the `latest` block tag, so pending-but-unconfirmed
    /// transactions are not counted. Callers should avoid calling `reset()`
    /// while transactions are still in-flight, as the freshly fetched nonce
    /// may conflict with pending transactions in the mempool.
    pub async fn reset(&self) {
        let mut guard = self.inner.lock().await;
        guard.nonce = None;
        // Wrapping add is intentional: a full u64 wrap-around (2^64 resets)
        // is infeasible, and wrapping avoids a panic on overflow.
        guard.generation = guard.generation.wrapping_add(1);
        info!(address = %self.address, "nonce cache reset");
    }
}

/// RAII guard holding a reserved nonce and the nonce mutex lock.
///
/// The lock is held for the duration of transaction signing to prevent
/// concurrent nonce conflicts. Drop the guard after successful signing
/// to release the lock, or call [`rollback`](Self::rollback) on failure
/// to restore the nonce for reuse.
#[derive(Debug)]
pub struct NonceGuard {
    guard: Option<OwnedMutexGuard<NonceState>>,
    nonce: u64,
}

impl NonceGuard {
    /// Returns the reserved nonce value.
    pub const fn nonce(&self) -> u64 {
        self.nonce
    }

    /// Rolls back the nonce reservation, restoring the cached nonce to
    /// the value that was reserved.
    ///
    /// This allows the next call to [`NonceManager::next_nonce`] to reuse
    /// the same nonce value. Consumes the guard, releasing the lock.
    pub fn rollback(mut self) {
        if let Some(mut guard) = self.guard.take() {
            guard.nonce = Some(self.nonce);
            debug!(nonce = self.nonce, "nonce rolled back");
        }
    }
}

impl Drop for NonceGuard {
    fn drop(&mut self) {
        if self.guard.is_some() {
            debug!(nonce = self.nonce, "nonce consumed");
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_node_bindings::Anvil;

    use super::*;

    impl NonceManager {
        /// Creates a [`NonceManager`] with the cache pre-seeded to `nonce`.
        ///
        /// Test-only: allows exercising edge-case nonce values (e.g.
        /// `u64::MAX`) without a real chain fetch.
        fn new_with_nonce(provider: RootProvider, address: Address, nonce: u64) -> Self {
            Self {
                inner: Arc::new(Mutex::new(NonceState { nonce: Some(nonce), generation: 0 })),
                provider,
                address,
                test_barrier: None,
            }
        }

        /// Creates a [`NonceManager`] with a [`TestBarrier`] installed.
        ///
        /// The barrier pauses `next_nonce` between the RPC fetch and
        /// lock re-acquisition, giving the test precise control over
        /// when `reset()` is called.
        fn new_with_barrier(
            provider: RootProvider,
            address: Address,
            barrier: Arc<TestBarrier>,
        ) -> Self {
            Self {
                inner: Arc::new(Mutex::new(NonceState::new())),
                provider,
                address,
                test_barrier: Some(barrier),
            }
        }
    }

    #[test]
    fn nonce_state_new_is_none_and_generation_zero() {
        let state = NonceState::new();
        assert_eq!(state.nonce(), None);
        assert_eq!(state.generation(), 0);
    }

    #[test]
    fn nonce_state_default_matches_new() {
        let state = NonceState::default();
        assert_eq!(state.nonce(), None);
        assert_eq!(state.generation(), 0);
    }

    #[tokio::test]
    async fn next_nonce_returns_overflow_at_u64_max() {
        let anvil = Anvil::new().spawn();
        let url = anvil.endpoint_url();
        let provider = RootProvider::new_http(url);
        let address = anvil.addresses()[0];

        let manager = NonceManager::new_with_nonce(provider, address, u64::MAX);

        let err = manager.next_nonce().await.expect_err("should overflow");
        assert_eq!(err, TxManagerError::NonceOverflow);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn generation_mismatch_retries_after_reset_during_fetch() {
        let anvil = Anvil::new().spawn();
        let url = anvil.endpoint_url();
        let provider = RootProvider::new_http(url);
        let address = anvil.addresses()[0];

        let barrier = Arc::new(TestBarrier::new());
        let manager = NonceManager::new_with_barrier(provider, address, Arc::clone(&barrier));
        let mgr = manager.clone();

        let handle = tokio::spawn(async move { mgr.next_nonce().await });

        // Attempt 0: intercept between fetch and re-lock, then reset to
        // bump the generation so the fetched value is discarded.
        barrier.fetch_completed.notified().await;
        manager.reset().await;
        barrier.may_proceed.notify_one();

        // Attempt 1: the retry re-fetches. Let it proceed normally.
        barrier.fetch_completed.notified().await;
        barrier.may_proceed.notify_one();

        let guard = handle.await.unwrap().expect("should succeed on retry");
        // Fresh Anvil account — nonce should be 0.
        assert_eq!(guard.nonce(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retry_exhaustion_returns_nonce_acquisition_failed() {
        let anvil = Anvil::new().spawn();
        let url = anvil.endpoint_url();
        let provider = RootProvider::new_http(url);
        let address = anvil.addresses()[0];

        let barrier = Arc::new(TestBarrier::new());
        let manager = NonceManager::new_with_barrier(provider, address, Arc::clone(&barrier));
        let mgr = manager.clone();

        let handle = tokio::spawn(async move { mgr.next_nonce().await });

        // Reset during every attempt so the generation always mismatches,
        // exhausting MAX_RETRY_ATTEMPTS.
        for _ in 0..NonceManager::MAX_RETRY_ATTEMPTS {
            barrier.fetch_completed.notified().await;
            manager.reset().await;
            barrier.may_proceed.notify_one();
        }

        let err = handle.await.unwrap().expect_err("should exhaust retries");
        assert_eq!(err, TxManagerError::NonceAcquisitionFailed);
    }
}
