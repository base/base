//! Cached `BaseTime` timestamp extraction for RPC responses.

use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex, PoisonError},
};

use alloy_primitives::BlockHash;
use base_common_consensus::BaseTransaction;
use base_protocol::BaseTimeUpdateTx;
use lru::LruCache;
use reth_node_api::BlockBody;
use reth_primitives_traits::Block as _;
use reth_storage_api::{BlockReader, BlockSource, errors::ProviderError};

/// Cache of validated `BaseTime` timestamps keyed by block hash.
#[derive(Clone, Debug)]
pub struct BaseTimeCache {
    timestamps: Arc<Mutex<LruCache<BlockHash, Option<u64>>>>,
}

impl Default for BaseTimeCache {
    fn default() -> Self {
        Self {
            timestamps: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(256).unwrap_or(NonZeroUsize::MIN),
            ))),
        }
    }
}

impl BaseTimeCache {
    /// Returns the validated millisecond timestamp for a block, loading its body on a cache miss.
    pub fn get<T, Provider>(
        &self,
        provider: &Provider,
        block_hash: BlockHash,
        block_number: u64,
        block_timestamp: u64,
    ) -> Result<Option<u64>, ProviderError>
    where
        T: BaseTransaction,
        Provider: BlockReader<Transaction = T>,
    {
        if let Some(timestamp_ms) =
            self.timestamps.lock().unwrap_or_else(PoisonError::into_inner).get(&block_hash).copied()
        {
            return Ok(timestamp_ms);
        }

        let block = match provider.find_block_by_hash(block_hash, BlockSource::Any) {
            Ok(Some(block)) => block,
            Ok(None) | Err(ProviderError::BlockExpired { .. }) => return Ok(None),
            Err(error) => return Err(error),
        };

        Ok(self.insert_from_transactions(
            block_hash,
            block_number,
            block_timestamp,
            block.body().transactions(),
        ))
    }

    /// Validates, caches, and returns a block's millisecond timestamp from its transactions.
    pub fn insert_from_transactions<T: BaseTransaction>(
        &self,
        block_hash: BlockHash,
        block_number: u64,
        block_timestamp: u64,
        transactions: &[T],
    ) -> Option<u64> {
        let timestamp_ms =
            BaseTimeUpdateTx::extract_timestamp_ms(transactions, block_number, block_timestamp)
                .ok();
        self.insert(block_hash, timestamp_ms);
        timestamp_ms
    }

    /// Caches a timestamp already derived from a loaded block.
    pub fn insert(&self, block_hash: BlockHash, timestamp_ms: Option<u64>) {
        self.timestamps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .put(block_hash, timestamp_ms);
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{BlockBody, Header, Sealable};
    use alloy_primitives::B256;
    use base_common_consensus::{BaseBlock, BasePrimitives, BaseTxEnvelope, TxDeposit};
    use base_protocol::BaseTimeUpdateTx;
    use reth_provider::test_utils::MockEthProvider;

    use super::BaseTimeCache;

    #[test]
    fn extracts_and_caches_validated_block_timestamp() {
        let block_number = 9;
        let block_hash = B256::repeat_byte(1);
        let transactions = vec![
            TxDeposit::default().seal_slow().into(),
            BaseTimeUpdateTx::new(600).unwrap().into_deposit_tx(block_number).into(),
        ];
        let block = BaseBlock {
            header: Header { number: block_number, timestamp: 42, ..Default::default() },
            body: BlockBody { transactions, ..Default::default() },
        };
        let provider = MockEthProvider::<BasePrimitives>::new();
        provider.add_block(block_hash, block.clone());
        let cache = BaseTimeCache::default();

        assert_eq!(
            cache.get::<BaseTxEnvelope, _>(&provider, block_hash, block_number, 42).unwrap(),
            Some(42_600)
        );

        provider.blocks.lock().clear();
        assert_eq!(
            cache.get::<BaseTxEnvelope, _>(&provider, block_hash, block_number, 42).unwrap(),
            Some(42_600)
        );
        assert_eq!(
            cache
                .get::<BaseTxEnvelope, _>(&provider, B256::repeat_byte(2), block_number, 42)
                .unwrap(),
            None
        );

        let previously_missing_hash = B256::repeat_byte(2);
        provider.add_block(previously_missing_hash, block);
        assert_eq!(
            cache
                .get::<BaseTxEnvelope, _>(&provider, previously_missing_hash, block_number, 42)
                .unwrap(),
            Some(42_600)
        );
    }
}
