//! In-process mempool metering: spawn a worker, await meterBundle, stash the result.

use std::{collections::HashSet, sync::Arc, time::Duration};

use alloy_consensus::Header;
use alloy_primitives::{Bytes, TxHash};
use base_bundles::{Bundle, InlineMetering, MeterBundleResponse};
use base_common_consensus::BaseBlock;
use base_execution_chainspec::BaseChainSpec;
use base_flashblocks::FlashblocksAPI;
use moka::{policy::EvictionPolicy, sync::Cache};
use parking_lot::Mutex;
use reth_provider::{
    BlockReader, BlockReaderIdExt, ChainSpecProvider, HeaderProvider, StateProviderFactory,
};
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use crate::{MeteringApiImpl, MeteringApiServer};

/// Default max concurrent inline meterBundle workers.
pub const DEFAULT_INLINE_METERING_MAX_CONCURRENT: usize = 32;
/// Default cache capacity for inline metering results.
pub const DEFAULT_INLINE_METERING_CACHE_CAPACITY: u64 = 10_000;
/// Default TTL for inline metering cache entries.
pub const DEFAULT_INLINE_METERING_CACHE_TTL: Duration = Duration::from_secs(30);

/// Spawns meterBundle workers and caches successful responses by tx hash.
pub struct InlineMeteringService<Provider, FB> {
    api: Arc<MeteringApiImpl<Provider, FB>>,
    cache: Cache<TxHash, MeterBundleResponse>,
    in_flight: Arc<Mutex<HashSet<TxHash>>>,
    semaphore: Arc<Semaphore>,
}

impl<Provider, FB> std::fmt::Debug for InlineMeteringService<Provider, FB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineMeteringService")
            .field("cache_entries", &self.cache.entry_count())
            .field("in_flight", &self.in_flight.lock().len())
            .field("available_permits", &self.semaphore.available_permits())
            .finish_non_exhaustive()
    }
}

impl<Provider, FB> InlineMeteringService<Provider, FB>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = BaseChainSpec>
        + BlockReaderIdExt<Header = Header>
        + BlockReader<Block = BaseBlock>
        + HeaderProvider<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
    FB: FlashblocksAPI + Send + Sync + 'static,
{
    /// Creates a new service wrapping the given metering API implementation.
    pub fn new(api: Arc<MeteringApiImpl<Provider, FB>>, max_concurrent: usize) -> Self {
        let max_concurrent = max_concurrent.max(1);
        let cache = Cache::builder()
            .max_capacity(DEFAULT_INLINE_METERING_CACHE_CAPACITY)
            .eviction_policy(EvictionPolicy::lru())
            .time_to_live(DEFAULT_INLINE_METERING_CACHE_TTL)
            .build();
        Self {
            api,
            cache,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
        }
    }

    fn spawn_meter(&self, tx_hash: TxHash, raw: Bytes) {
        if self.cache.contains_key(&tx_hash) {
            return;
        }
        {
            let mut in_flight = self.in_flight.lock();
            if !in_flight.insert(tx_hash) {
                return;
            }
        }

        let api = Arc::clone(&self.api);
        let cache = self.cache.clone();
        let in_flight = Arc::clone(&self.in_flight);
        let semaphore = Arc::clone(&self.semaphore);

        tokio::spawn(async move {
            let Ok(_permit) = semaphore.acquire_owned().await else {
                in_flight.lock().remove(&tx_hash);
                return;
            };

            let bundle = Bundle { txs: vec![raw], ..Default::default() };
            match api.meter_bundle(bundle).await {
                Ok(response) => {
                    debug!(tx_hash = %tx_hash, "inline meterBundle completed");
                    cache.insert(tx_hash, response);
                }
                Err(error) => {
                    warn!(tx_hash = %tx_hash, error = %error, "inline meterBundle failed");
                }
            }
            in_flight.lock().remove(&tx_hash);
        });
    }
}

impl<Provider, FB> InlineMetering for InlineMeteringService<Provider, FB>
where
    Provider: StateProviderFactory
        + ChainSpecProvider<ChainSpec = BaseChainSpec>
        + BlockReaderIdExt<Header = Header>
        + BlockReader<Block = BaseBlock>
        + HeaderProvider<Header = Header>
        + Clone
        + Send
        + Sync
        + 'static,
    FB: FlashblocksAPI + Send + Sync + 'static,
{
    fn get(&self, tx_hash: &TxHash) -> Option<MeterBundleResponse> {
        self.cache.get(tx_hash)
    }

    fn submit(&self, tx_hash: TxHash, raw: Bytes) {
        self.spawn_meter(tx_hash, raw);
    }
}
