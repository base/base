//! Nonce allocation and tracking for load-test senders.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_provider::Provider;
use thiserror::Error;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::{debug, trace, warn};

/// Errors returned by nonce reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum NonceError {
    /// The chain nonce read did not complete within the RPC timeout.
    #[error("nonce fetch timed out")]
    FetchTimeout,

    /// The chain nonce read failed.
    #[error("nonce fetch failed")]
    FetchFailed,

    /// Nonce reservation failed due to repeated cache contention.
    #[error("nonce acquisition failed")]
    AcquisitionFailed,

    /// Nonce arithmetic overflowed `u64::MAX`.
    #[error("nonce overflow")]
    Overflow,
}

/// Internal state tracked by [`NonceManager`].
#[derive(Debug, Default)]
pub struct NonceState {
    /// The cached nonce value, or `None` if uninitialized / reset.
    pub nonce: Option<u64>,
    /// Counter bumped on every [`NonceManager::reset`], letting an in-flight
    /// chain fetch detect that a reset invalidated it.
    ///
    /// Uses [`u64::wrapping_add`] so the counter cannot panic. A
    /// generation collision (wrapping all the way around to the same
    /// snapshot value) would require 2^64 calls to `reset()`, which
    /// is infeasible in practice.
    pub generation: u64,
    /// Nonces returned explicitly before publication. These are recycled on
    /// subsequent nonce requests, filling gaps left by failed send tasks.
    ///
    /// Not cleared by [`NonceManager::reset`] — the nonces were never
    /// published and must persist for reuse.
    pub returned_nonces: BTreeSet<u64>,
}

/// Manages nonce allocation and tracking.
///
/// Wraps a [`tokio::sync::Mutex`] around internal nonce state, lazily
/// fetching the initial nonce from chain state via
/// [`Provider::get_transaction_count`] on first use. Subsequent calls
/// increment locally without making RPC calls.
///
/// The mutex is held for the duration of transaction signing via the
/// returned [`NonceGuard`], ensuring sequential nonce assignment even
/// under concurrent access.
#[derive(Debug, Clone)]
pub struct NonceManager<P> {
    inner: Arc<Mutex<NonceState>>,
    provider: P,
    address: Address,
    rpc_timeout: Duration,
    /// When `true`, the initial nonce fetch uses the `pending` block tag
    /// (counting both confirmed and mempool transactions). When `false`
    /// (the default), `latest` is used (confirmed transactions only).
    use_pending_tag: bool,
}

impl<P: Provider> NonceManager<P> {
    /// Maximum number of retry attempts when `reset()` races with
    /// `next_nonce()`, clearing the cache between the RPC fetch and
    /// lock re-acquisition.
    pub const MAX_RETRY_ATTEMPTS: u8 = 5;

    /// Creates a new [`NonceManager`] with no cached nonce.
    ///
    /// The `rpc_timeout` bounds the `get_transaction_count` RPC call
    /// performed during lazy nonce initialization.
    ///
    /// The first call to [`next_nonce`](Self::next_nonce) will fetch the
    /// current transaction count from the provider.
    pub fn new(provider: P, address: Address, rpc_timeout: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(NonceState::default())),
            provider,
            address,
            rpc_timeout,
            use_pending_tag: false,
        }
    }

    /// Configures the nonce manager to use the `pending` block tag when
    /// fetching the initial nonce from chain.
    pub const fn with_pending_tag(mut self) -> Self {
        self.use_pending_tag = true;
        self
    }

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
    /// Returns [`NonceError::FetchTimeout`] or [`NonceError::FetchFailed`]
    /// if the provider call fails, [`NonceError::AcquisitionFailed`] if the
    /// nonce cache is repeatedly cleared by concurrent
    /// [`reset`](Self::reset) calls during acquisition, or
    /// [`NonceError::Overflow`] if the nonce exceeds `u64::MAX`.
    pub async fn next_nonce(&self) -> Result<NonceGuard, NonceError> {
        for attempt in 0..Self::MAX_RETRY_ATTEMPTS {
            // Acquire the owned lock once upfront.
            let guard = Arc::clone(&self.inner).lock_owned().await;

            // Fast path: cache is populated — read-and-increment in a
            // single lock round-trip.
            if let Some(n) = guard.nonce {
                return Self::advance_nonce(guard, n);
            }

            // Cache miss: snapshot the generation, then drop the lock so
            // the RPC fetch doesn't block concurrent callers.
            let generation = guard.generation;
            drop(guard);

            // Phase 2: fetch from chain *without* holding the lock so
            // concurrent callers are not blocked by the RPC round-trip.
            // Multiple concurrent callers may fetch redundantly; only
            // the first writer's value is used.
            let count_fut = if self.use_pending_tag {
                self.provider.get_transaction_count(self.address).pending()
            } else {
                self.provider.get_transaction_count(self.address)
            };
            let fetched = tokio::time::timeout(self.rpc_timeout, count_fut)
                .await
                .map_err(|_| {
                    warn!(
                        address = %self.address,
                        timeout = ?self.rpc_timeout,
                        "nonce fetch timed out",
                    );
                    NonceError::FetchTimeout
                })?
                .map_err(|_| {
                    // The raw transport error may embed credential-bearing
                    // RPC URLs, so neither log nor propagate it.
                    warn!(address = %self.address, "failed to fetch nonce from chain");
                    NonceError::FetchFailed
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

            // Prune returned nonces that the chain has already confirmed.
            // `split_off(fetched)` moves elements >= fetched into `kept`;
            // the original set retains elements < fetched (stale).
            let kept = guard.returned_nonces.split_off(&fetched);
            if !guard.returned_nonces.is_empty() {
                debug!(
                    pruned = guard.returned_nonces.len(),
                    chain_nonce = fetched,
                    "pruned stale returned nonces"
                );
            }
            guard.returned_nonces = kept;

            return Self::advance_nonce(guard, nonce);
        }

        warn!(attempts = Self::MAX_RETRY_ATTEMPTS, "nonce acquisition failed after max retries",);
        Err(NonceError::AcquisitionFailed)
    }

    /// Advances the cached nonce by one and returns a [`NonceGuard`]
    /// holding the lock and the reserved value.
    ///
    /// Both the fast path (cache hit) and Phase 3 (after RPC fetch) use
    /// this single call-site for the `checked_add` overflow check,
    /// ensuring consistent behavior.
    pub fn advance_nonce(
        mut guard: OwnedMutexGuard<NonceState>,
        nonce: u64,
    ) -> Result<NonceGuard, NonceError> {
        // Priority: reissue a returned nonce before allocating a fresh one.
        // BTreeSet::pop_first gives the smallest gap, filling holes
        // left-to-right so the chain can make forward progress.
        if let Some(returned) = guard.returned_nonces.pop_first() {
            // Recycle does not use the cached counter, so bump it past the
            // hole we just filled. Otherwise the next fresh allocation can
            // rewind below a nonce we already handed out.
            guard.nonce = Some(nonce.max(returned.saturating_add(1)));
            debug!(nonce = returned, "reissuing returned nonce");
            return Ok(NonceGuard { guard: Some(guard), nonce: returned, recycled: true });
        }

        let next = nonce.checked_add(1).ok_or(NonceError::Overflow)?;
        guard.nonce = Some(next);
        debug!(nonce, "nonce reserved");
        Ok(NonceGuard { guard: Some(guard), nonce, recycled: false })
    }

    /// Clears the cached nonce, forcing a fresh chain fetch on the next
    /// call to [`next_nonce`](Self::next_nonce).
    ///
    /// # Caution
    ///
    /// Unless [`with_pending_tag`](Self::with_pending_tag) was set, the
    /// fresh fetch uses the `latest` block tag, so pending-but-unconfirmed
    /// transactions are not counted. Callers should avoid calling `reset()`
    /// while transactions are still in-flight, as the freshly fetched nonce
    /// may conflict with pending transactions in the mempool.
    pub async fn reset(&self) {
        let mut guard = self.inner.lock().await;
        guard.nonce = None;
        // Wrapping add is intentional: a full u64 wrap-around (2^64 resets)
        // is infeasible, and wrapping avoids a panic on overflow.
        guard.generation = guard.generation.wrapping_add(1);
        // `returned_nonces` is intentionally NOT cleared — these nonces
        // were never published and must persist for reuse.
        trace!(address = %self.address, "nonce cache reset");
    }

    /// Returns a previously reserved nonce for reuse.
    ///
    /// Called when a reserved task fails before publishing, making it
    /// available for reissue by the next
    /// [`next_nonce`](Self::next_nonce) call.
    ///
    /// The returned nonce is stored in the internal returned-nonce set and
    /// is not cleared by [`reset`](Self::reset), since it was never
    /// published to the network.
    pub async fn return_reserved_nonce(&self, nonce: u64) {
        let mut guard = self.inner.lock().await;
        guard.returned_nonces.insert(nonce);
        debug!(nonce, "reserved nonce returned for reuse");
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
    /// Whether this guard's nonce was recycled from
    /// the internal returned-nonce set. Affects rollback behavior:
    /// recycled nonces are returned to the set rather than restoring the
    /// cache.
    recycled: bool,
}

impl NonceGuard {
    /// Returns the reserved nonce value.
    pub const fn nonce(&self) -> u64 {
        self.nonce
    }

    /// Rolls back the nonce reservation, restoring the cached nonce to
    /// the value that was reserved.
    ///
    /// For recycled nonces, the
    /// nonce is re-inserted into the returned set rather than restoring
    /// the cache, since the cache was never advanced for these nonces.
    ///
    /// This allows the next call to [`NonceManager::next_nonce`] to reuse
    /// the same nonce value. Consumes the guard, releasing the lock.
    pub fn rollback(mut self) {
        if let Some(mut guard) = self.guard.take() {
            if self.recycled {
                guard.returned_nonces.insert(self.nonce);
                debug!(nonce = self.nonce, "recycled nonce returned to reuse set");
            } else {
                guard.nonce = Some(self.nonce);
                debug!(nonce = self.nonce, "nonce rolled back");
            }
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
    use std::{
        future::{Future, poll_fn},
        task::Poll,
    };

    use alloy_provider::RootProvider;

    use super::*;

    impl NonceManager<RootProvider> {
        /// Creates a [`NonceManager`] with the cache pre-seeded to `nonce`.
        ///
        /// Test-only: allows exercising edge-case nonce values (e.g.
        /// `u64::MAX`) without a real chain fetch.
        fn new_with_nonce(provider: RootProvider, address: Address, nonce: u64) -> Self {
            Self {
                inner: Arc::new(Mutex::new(NonceState {
                    nonce: Some(nonce),
                    ..Default::default()
                })),
                provider,
                address,
                rpc_timeout: Duration::from_secs(10),
                use_pending_tag: false,
            }
        }
    }

    #[test]
    fn nonce_state_default_is_empty() {
        let state = NonceState::default();
        assert_eq!(state.nonce, None);
        assert_eq!(state.generation, 0);
        assert!(state.returned_nonces.is_empty());
    }

    #[tokio::test]
    async fn reissuing_returned_nonce_does_not_rewind_cache() {
        let provider =
            RootProvider::new_http("http://127.0.0.1:1".parse().expect("valid test URL"));

        let manager = NonceManager::new_with_nonce(provider, Address::ZERO, 2);
        manager.return_reserved_nonce(3).await;
        manager.return_reserved_nonce(4).await;

        let guard = manager.next_nonce().await.expect("reissue 3");
        assert_eq!(guard.nonce(), 3);
        drop(guard);

        let guard = manager.next_nonce().await.expect("reissue 4");
        assert_eq!(guard.nonce(), 4);
        drop(guard);

        let guard = manager.next_nonce().await.expect("fresh after holes");
        assert_eq!(guard.nonce(), 5);
    }

    #[tokio::test]
    async fn next_nonce_returns_overflow_at_u64_max() {
        let provider =
            RootProvider::new_http("http://127.0.0.1:1".parse().expect("valid test URL"));

        let manager = NonceManager::new_with_nonce(provider, Address::ZERO, u64::MAX);

        let err = manager.next_nonce().await.expect_err("should overflow");
        assert_eq!(err, NonceError::Overflow);
    }

    #[tokio::test]
    async fn reset_stays_pending_while_nonce_guard_holds_mutex() {
        let provider =
            RootProvider::new_http("http://127.0.0.1:1".parse().expect("valid test URL"));

        let manager = NonceManager::new_with_nonce(provider, Address::ZERO, 0);

        let guard = manager.next_nonce().await.expect("should reserve nonce");
        assert_eq!(guard.nonce(), 0);

        let mut reset = std::pin::pin!(manager.reset());
        let reset_is_pending =
            poll_fn(|cx| Poll::Ready(matches!(reset.as_mut().poll(cx), Poll::Pending))).await;
        assert!(reset_is_pending, "reset() must stay pending while a NonceGuard holds the mutex",);

        drop(guard);
        reset.await;
        assert_eq!(manager.inner.lock().await.nonce, None);
    }
}
