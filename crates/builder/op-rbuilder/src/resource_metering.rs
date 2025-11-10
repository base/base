use crate::metrics::OpRBuilderMetrics;
use alloy_primitives::TxHash;
use concurrent_queue::{ConcurrentQueue, PopError};
use dashmap::try_result::TryResult;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};
use std::{
    fmt::Debug,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tips_core::MeterBundleResponse;

struct Data {
    enabled: AtomicBool,
    by_tx_hash: dashmap::DashMap<TxHash, MeterBundleResponse>,
    lru: ConcurrentQueue<TxHash>,
}

#[derive(Clone)]
pub struct ResourceMetering {
    data: Arc<Data>,
    metrics: OpRBuilderMetrics,
}

impl Debug for ResourceMetering {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceMetering")
            .field("enabled", &self.data.enabled)
            .field("by_tx_hash", &self.data.by_tx_hash.len())
            .finish()
    }
}

impl ResourceMetering {
    pub(crate) fn insert(&self, tx: TxHash, metering_info: MeterBundleResponse) {
        let to_remove = if self.data.lru.is_full() {
            match self.data.lru.pop() {
                Ok(tx_hash) => Some(tx_hash),
                Err(PopError::Empty) => None,
                Err(PopError::Closed) => None,
            }
        } else {
            None
        };

        if let Some(tx_hash) = to_remove {
            self.data.by_tx_hash.remove(&tx_hash);
        }

        self.data.by_tx_hash.insert(tx, metering_info);
    }

    pub(crate) fn clear(&self) {
        self.data.by_tx_hash.clear();
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.data.enabled.store(enabled, Ordering::Relaxed);
    }

    pub(crate) fn get(&self, tx: &TxHash) -> Option<MeterBundleResponse> {
        if !self.data.enabled.load(Ordering::Relaxed) {
            return None;
        }

        match self.data.by_tx_hash.try_get(tx) {
            TryResult::Present(result) => {
                self.metrics.metering_known_transaction.increment(1);
                Some(result.clone())
            }
            TryResult::Absent => {
                self.metrics.metering_unknown_transaction.increment(1);
                None
            }
            TryResult::Locked => {
                self.metrics.metering_locked_transaction.increment(1);
                None
            }
        }
    }
}

impl Default for ResourceMetering {
    fn default() -> Self {
        Self::new(false, 10_000)
    }
}

impl ResourceMetering {
    pub fn new(enabled: bool, buffer_size: usize) -> Self {
        Self {
            data: Arc::new(Data {
                by_tx_hash: dashmap::DashMap::new(),
                enabled: AtomicBool::new(enabled),
                lru: ConcurrentQueue::bounded(buffer_size),
            }),
            metrics: OpRBuilderMetrics::default(),
        }
    }
}

// Namespace overrides for ingesting resource metering
#[cfg_attr(not(test), rpc(server, namespace = "base"))]
#[cfg_attr(test, rpc(server, client, namespace = "base"))]
pub trait BaseApiExt {
    #[method(name = "setMeteringInformation")]
    async fn set_metering_information(
        &self,
        tx_hash: TxHash,
        meter: MeterBundleResponse,
    ) -> RpcResult<()>;

    #[method(name = "setMeteringEnabled")]
    async fn set_metering_enabled(&self, enabled: bool) -> RpcResult<()>;

    #[method(name = "clearMeteringInformation")]
    async fn clear_metering_information(&self) -> RpcResult<()>;
}

pub(crate) struct ResourceMeteringExt {
    metering_info: ResourceMetering,
}

impl ResourceMeteringExt {
    pub(crate) fn new(metering_info: ResourceMetering) -> Self {
        Self { metering_info }
    }
}

#[async_trait]
impl BaseApiExtServer for ResourceMeteringExt {
    async fn set_metering_information(
        &self,
        tx_hash: TxHash,
        metering: MeterBundleResponse,
    ) -> RpcResult<()> {
        self.metering_info.insert(tx_hash, metering);
        Ok(())
    }

    async fn set_metering_enabled(&self, enabled: bool) -> RpcResult<()> {
        self.metering_info.set_enabled(enabled);
        Ok(())
    }

    async fn clear_metering_information(&self) -> RpcResult<()> {
        self.metering_info.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, TxHash, U256};
    use tips_core::MeterBundleResponse;

    fn create_test_metering(gas_used: u64) -> MeterBundleResponse {
        MeterBundleResponse {
            bundle_hash: B256::random(),
            bundle_gas_price: U256::from(123),
            coinbase_diff: U256::from(123),
            eth_sent_to_coinbase: U256::from(123),
            gas_fees: U256::from(123),
            results: vec![],
            state_block_number: 4,
            state_flashblock_index: None,
            total_gas_used: gas_used,
            total_execution_time_us: 533,
        }
    }

    #[test]
    fn test_basic_insert_get_and_enable_disable() {
        let metering = ResourceMetering::default();
        let tx_hash = TxHash::random();
        let meter_data = create_test_metering(21000);

        metering.insert(tx_hash, meter_data);
        assert!(metering.get(&tx_hash).is_none());

        metering.set_enabled(true);
        assert_eq!(metering.get(&tx_hash).unwrap().total_gas_used, 21000);

        metering.insert(tx_hash, create_test_metering(50000));
        assert_eq!(metering.get(&tx_hash).unwrap().total_gas_used, 50000);

        metering.set_enabled(false);
        assert!(metering.get(&tx_hash).is_none());

        metering.set_enabled(true);
        assert!(metering.get(&TxHash::random()).is_none());
    }

    #[test]
    fn test_clear() {
        let metering = ResourceMetering::new(true, 100);

        let tx1 = TxHash::random();
        let tx2 = TxHash::random();

        metering.insert(tx1, create_test_metering(1000));
        metering.insert(tx2, create_test_metering(2000));

        assert!(metering.get(&tx1).is_some());
        assert!(metering.get(&tx2).is_some());

        metering.clear();

        assert!(metering.get(&tx1).is_none());
        assert!(metering.get(&tx2).is_none());
    }
}
