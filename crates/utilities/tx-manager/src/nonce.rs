//! Nonce allocation and tracking.

use std::sync::Arc;

use alloy_primitives::Address;
use alloy_provider::{Provider, RootProvider};
use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::{debug, info, warn};

use crate::TxManagerError;

/// Manages nonce allocation and tracking.
///
/// Wraps a [`tokio::sync::Mutex`] around an optional cached nonce value,
/// lazily fetching the initial nonce from chain state via
/// [`Provider::get_transaction_count`] on first use. Subsequent calls
/// increment locally without making RPC calls.
///
/// The mutex is held for the duration of transaction signing via the
/// returned [`NonceGuard`], ensuring sequential nonce assignment even
/// under concurrent access.
#[derive(Debug, Clone)]
pub struct NonceManager {
    inner: Arc<Mutex<Option<u64>>>,
    provider: RootProvider,
    address: Address,
}

impl NonceManager {
    /// Creates a new [`NonceManager`] with no cached nonce.
    ///
    /// The first call to [`next_nonce`](Self::next_nonce) will fetch the
    /// current transaction count from the provider.
    pub fn new(provider: RootProvider, address: Address) -> Self {
        Self { inner: Arc::new(Mutex::new(None)), provider, address }
    }

    /// Reserves the next nonce for transaction signing.
    ///
    /// On the first call (or after [`reset`](Self::reset)), fetches the
    /// current transaction count from the provider. Subsequent calls
    /// increment the cached value locally without making RPC calls.
    ///
    /// Returns a [`NonceGuard`] that holds the mutex lock for the duration
    /// of transaction signing. Drop the guard on success, or call
    /// [`NonceGuard::rollback`] on failure to restore the nonce.
    ///
    /// # Errors
    ///
    /// Returns [`TxManagerError::Rpc`] if the provider call fails.
    pub async fn next_nonce(&self) -> Result<NonceGuard, TxManagerError> {
        let mut guard = Arc::clone(&self.inner).lock_owned().await;

        let nonce = match *guard {
            Some(n) => {
                debug!(nonce = n, "nonce reserved");
                n
            }
            None => {
                let n = self.provider.get_transaction_count(self.address).await.map_err(|e| {
                    warn!(error = %e, address = %self.address, "failed to fetch nonce from chain");
                    TxManagerError::Rpc(e.to_string())
                })?;
                debug!(nonce = n, "nonce fetched from chain");
                n
            }
        };

        *guard = Some(nonce + 1);

        Ok(NonceGuard { guard: Some(guard), nonce })
    }

    /// Clears the cached nonce, forcing a fresh chain fetch on the next
    /// call to [`next_nonce`](Self::next_nonce).
    pub async fn reset(&self) {
        let mut guard = self.inner.lock().await;
        *guard = None;
        info!(address = %self.address, "nonce cache reset");
    }
}

/// RAII guard holding a reserved nonce and the nonce mutex lock.
///
/// The lock is held for the duration of transaction signing to prevent
/// concurrent nonce conflicts. Drop the guard after successful signing
/// to release the lock and advance the nonce. Call
/// [`rollback`](Self::rollback) on signing failure to restore the nonce
/// for reuse.
#[derive(Debug)]
pub struct NonceGuard {
    guard: Option<OwnedMutexGuard<Option<u64>>>,
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
            *guard = Some(self.nonce);
            debug!(nonce = self.nonce, "nonce rolled back");
        }
    }
}
