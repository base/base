//! Nonce allocation and tracking.

use std::sync::Arc;

use alloy_primitives::Address;
use alloy_provider::{Provider, RootProvider};
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
    pub nonce: Option<u64>,
    /// Monotonically increasing counter bumped on every
    /// [`NonceManager::reset`].
    pub generation: u64,
}

impl NonceState {
    /// Creates a new [`NonceState`] with no cached nonce and generation
    /// zero.
    pub const fn new() -> Self {
        Self { nonce: None, generation: 0 }
    }
}

impl Default for NonceState {
    fn default() -> Self {
        Self::new()
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
}

impl NonceManager {
    /// Creates a new [`NonceManager`] with no cached nonce.
    ///
    /// The first call to [`next_nonce`](Self::next_nonce) will fetch the
    /// current transaction count from the provider.
    pub fn new(provider: RootProvider, address: Address) -> Self {
        Self { inner: Arc::new(Mutex::new(NonceState::new())), provider, address }
    }

    /// Maximum number of retry attempts when `reset()` races with
    /// `next_nonce()`, clearing the cache between the peek and lock
    /// acquisition.
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
            // Phase 1: peek under the lock to check whether init is needed
            // and snapshot the current generation.
            let (needs_init, generation) = {
                let state = self.inner.lock().await;
                (state.nonce.is_none(), state.generation)
            };

            // Phase 2: if uninitialized, fetch from chain *without* holding
            // the lock so concurrent callers are not blocked by the RPC
            // round-trip. Multiple concurrent callers may fetch redundantly;
            // only the first writer's value is used.
            if needs_init {
                let fetched =
                    self.provider.get_transaction_count(self.address).await.map_err(|e| {
                        warn!(
                            error = %e, address = %self.address,
                            "failed to fetch nonce from chain",
                        );
                        TxManagerError::Rpc(e.to_string())
                    })?;

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
                let next = nonce.checked_add(1).ok_or(TxManagerError::NonceOverflow)?;
                guard.nonce = Some(next);
                debug!(nonce, "nonce reserved");
                return Ok(NonceGuard { guard: Some(guard), nonce });
            }

            // Steady-state fast path: cache is populated. Acquire the
            // owned lock for the read-and-increment.
            let mut guard = Arc::clone(&self.inner).lock_owned().await;
            if let Some(n) = guard.nonce {
                let next = n.checked_add(1).ok_or(TxManagerError::NonceOverflow)?;
                guard.nonce = Some(next);
                debug!(nonce = n, "nonce reserved");
                return Ok(NonceGuard { guard: Some(guard), nonce: n });
            }

            // Rare: reset() cleared the cache between our peek and lock
            // acquisition. Drop the lock and retry.
            drop(guard);
            debug!(attempt, "nonce cache cleared during acquisition, retrying");
        }

        warn!(attempts = Self::MAX_RETRY_ATTEMPTS, "nonce acquisition failed after max retries",);
        Err(TxManagerError::NonceAcquisitionFailed)
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
            }
        }
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
}
