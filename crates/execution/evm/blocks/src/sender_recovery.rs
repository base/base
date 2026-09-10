use alloc::sync::Arc;

use alloy_primitives::{Address, B256, map::FbBuildHasher};
#[cfg(test)]
use base_common_types_chain::EthereumTxEnvelope;
#[cfg(test)]
use base_common_types_chain::EthereumTypedTransaction;
#[cfg(test)]
use base_common_types_chain::TxEip4844;
use base_common_types_chain::{RecoveryError, SignedTransaction};

/// Number of entries retained in the default sender recovery cache.
const SENDER_RECOVERY_CACHE_CAPACITY: usize = 1 << 17;

/// Shared cache of recovered transaction senders.
///
/// Sender recovery is performed when a transaction enters the pool and again when the same
/// transaction is received in an execution payload. Sharing this bounded, lock-free cache lets
/// payload prewarming reuse the result produced by transaction-pool ingress.
#[derive(Clone, Debug)]
pub struct SenderRecoveryCache {
    cache: Arc<fixed_cache::Cache<B256, Address, FbBuildHasher<32>, SenderRecoveryCacheConfig>>,
}

impl SenderRecoveryCache {
    /// Creates a sender recovery cache with the given capacity.
    ///
    /// The capacity must satisfy [`fixed_cache::Cache`] capacity requirements.
    pub fn new(capacity: usize) -> Self {
        Self { cache: Arc::new(fixed_cache::Cache::new(capacity, FbBuildHasher::<32>::default())) }
    }

    /// Returns a cached sender for the transaction hash.
    #[inline]
    pub fn get(&self, tx_hash: &B256) -> Option<Address> {
        self.cache.get(tx_hash)
    }

    /// Returns the cached sender or recovers and caches it on a miss.
    ///
    /// Failed recoveries are not cached.
    #[inline]
    pub fn recover<T: SignedTransaction>(&self, transaction: &T) -> Result<Address, RecoveryError> {
        self.cache.get_or_try_insert_with_ref(
            transaction.tx_hash(),
            |_| transaction.try_recover(),
            |hash| *hash,
        )
    }
}

impl Default for SenderRecoveryCache {
    fn default() -> Self {
        Self::new(SENDER_RECOVERY_CACHE_CAPACITY)
    }
}

struct SenderRecoveryCacheConfig;

impl fixed_cache::CacheConfig for SenderRecoveryCacheConfig {
    const STATS: bool = false;
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Signature, U256};
    use base_common_types_chain::TxLegacy;

    use super::*;

    #[test]
    fn recover_populates_cache() {
        let transaction = EthereumTxEnvelope::<TxEip4844>::new_unhashed(
            EthereumTypedTransaction::<TxEip4844>::Legacy(TxLegacy::default()),
            Signature::test_signature(),
        );
        let cache = SenderRecoveryCache::new(4);
        let shared_cache = cache.clone();

        let sender = cache.recover(&transaction).unwrap();

        assert_eq!(shared_cache.get(transaction.tx_hash()), Some(sender));
        assert_eq!(shared_cache.recover(&transaction).unwrap(), sender);
    }

    #[test]
    fn failed_recovery_is_not_cached() {
        let transaction = EthereumTxEnvelope::<TxEip4844>::new_unhashed(
            EthereumTypedTransaction::<TxEip4844>::Legacy(TxLegacy::default()),
            Signature::new(U256::ZERO, U256::ZERO, false),
        );
        let cache = SenderRecoveryCache::new(4);

        assert!(cache.recover(&transaction).is_err());
        assert_eq!(cache.get(transaction.tx_hash()), None);
    }
}
