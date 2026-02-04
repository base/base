//! Transaction data store.
//!
//! Provides a concurrent cache for transaction metadata including resource metering
//! information and backrun bundles. Uses LRU eviction to bound memory usage.

use std::{
    fmt::Debug,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use alloy_consensus::Transaction;
use alloy_primitives::TxHash;
use base_bundles::{AcceptedBundle, MeterBundleResponse};
use concurrent_queue::ConcurrentQueue;
use reth_optimism_txpool::OpPooledTransaction;
use reth_transaction_pool::PoolTransaction;
use tracing::{debug, info};

use super::{StoreData, StoredBackrunBundle, TxData};
use crate::BuilderMetrics;

#[derive(Clone)]
pub struct TxDataStore {
    data: Arc<StoreData>,
    metrics: BuilderMetrics,
}

impl Debug for TxDataStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxDataStore")
            .field("entries", &self.data.by_tx_hash.len())
            .field("metering_enabled", &self.data.metering_enabled.load(Ordering::Relaxed))
            .finish()
    }
}

impl TxDataStore {
    pub fn new(enable_resource_metering: bool, buffer_size: usize) -> Self {
        Self {
            data: Arc::new(StoreData {
                by_tx_hash: dashmap::DashMap::new(),
                lru: ConcurrentQueue::bounded(buffer_size),
                metering_enabled: AtomicBool::new(enable_resource_metering),
            }),
            metrics: BuilderMetrics::default(),
        }
    }

    fn evict_if_needed(&self) {
        if self.data.lru.is_full()
            && let Ok(evicted_hash) = self.data.lru.pop()
        {
            self.data.by_tx_hash.remove(&evicted_hash);
            debug!(
                target: "tx_data_store",
                evicted_tx = ?evicted_hash,
                "Evicted old transaction data"
            );
        }
    }

    pub fn get(&self, tx_hash: &TxHash) -> TxData {
        let metering_enabled = self.data.metering_enabled.load(Ordering::Relaxed);

        let Some(entry) = self.data.by_tx_hash.get(tx_hash) else {
            if metering_enabled {
                self.metrics.metering_unknown_transaction.increment(1);
            }
            return TxData { metering: None, backrun_bundles: vec![] };
        };

        let data = entry.clone();

        if metering_enabled {
            if data.metering.is_some() {
                self.metrics.metering_known_transaction.increment(1);
            } else {
                self.metrics.metering_unknown_transaction.increment(1);
            }
        }

        data
    }

    pub fn insert_backrun_bundle(&self, bundle: AcceptedBundle) -> Result<(), String> {
        if bundle.txs.len() < 2 {
            return Err("Bundle must have at least 2 transactions (target + backrun)".to_string());
        }

        let target_tx_hash = bundle.txs[0].tx_hash();

        // Convert OpTxEnvelope transactions to OpPooledTransaction
        let backrun_txs: Vec<OpPooledTransaction> = bundle.txs[1..]
            .iter()
            .filter_map(|tx| {
                let (envelope, signer) = tx.clone().into_parts();
                let pooled_envelope: op_alloy_consensus::OpPooledTransaction =
                    envelope.try_into().ok()?;
                let recovered_pooled =
                    alloy_consensus::transaction::Recovered::new_unchecked(pooled_envelope, signer);
                Some(OpPooledTransaction::from_pooled(recovered_pooled))
            })
            .collect();

        if backrun_txs.is_empty() {
            return Err("No valid poolable transactions in backrun bundle".to_string());
        }

        let backrun_sender = backrun_txs[0].sender();

        self.evict_if_needed();
        let _ = self.data.lru.push(target_tx_hash);

        let total_priority_fee: u128 =
            backrun_txs.iter().map(|tx| tx.max_priority_fee_per_gas().unwrap_or(0)).sum();

        let stored_bundle = StoredBackrunBundle {
            bundle_id: *bundle.uuid(),
            sender: backrun_sender,
            backrun_txs,
            total_priority_fee,
        };

        let replaced = {
            let mut entry = self.data.by_tx_hash.entry(target_tx_hash).or_default();
            let replaced = if let Some(pos) =
                entry.backrun_bundles.iter().position(|b| b.sender == backrun_sender)
            {
                entry.backrun_bundles[pos] = stored_bundle;
                true
            } else {
                entry.backrun_bundles.push(stored_bundle);
                false
            };
            entry.backrun_bundles.sort_by(|a, b| b.total_priority_fee.cmp(&a.total_priority_fee));
            replaced
        };

        if replaced {
            info!(
                target: "tx_data_store",
                target_tx = ?target_tx_hash,
                sender = ?backrun_sender,
                bundle_id = ?bundle.uuid(),
                "Replaced existing backrun bundle from same sender"
            );
        }

        info!(
            target: "tx_data_store",
            target_tx = ?target_tx_hash,
            sender = ?backrun_sender,
            bundle_id = ?bundle.uuid(),
            "Stored backrun bundle"
        );

        self.metrics.backrun_bundles_in_store.set(self.data.by_tx_hash.len() as f64);

        Ok(())
    }

    pub fn remove_backrun_bundles(&self, target_tx_hash: &TxHash) {
        if let Some(mut entry) = self.data.by_tx_hash.get_mut(target_tx_hash) {
            let bundle_count = entry.backrun_bundles.len();
            entry.backrun_bundles.clear();

            if bundle_count > 0 {
                info!(
                    target: "tx_data_store",
                    target_tx = ?target_tx_hash,
                    bundle_count,
                    "Removed backrun bundles"
                );
            }

            if entry.metering.is_none() && entry.backrun_bundles.is_empty() {
                drop(entry);
                self.data.by_tx_hash.remove(target_tx_hash);
            }
        }

        self.metrics.backrun_bundles_in_store.set(self.data.by_tx_hash.len() as f64);
    }

    pub fn insert_metering(&self, tx_hash: TxHash, metering_info: MeterBundleResponse) {
        self.evict_if_needed();
        let _ = self.data.lru.push(tx_hash);

        let mut entry = self.data.by_tx_hash.entry(tx_hash).or_default();
        entry.metering = Some(metering_info);
    }

    pub fn clear_metering(&self) {
        for mut entry in self.data.by_tx_hash.iter_mut() {
            entry.metering = None;
        }
        self.data.by_tx_hash.retain(|_, v| v.metering.is_some() || !v.backrun_bundles.is_empty());
    }

    pub fn set_metering_enabled(&self, enabled: bool) {
        self.data.metering_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn len(&self) -> usize {
        self.data.by_tx_hash.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.by_tx_hash.is_empty()
    }
}

impl Default for TxDataStore {
    fn default() -> Self {
        Self::new(false, 10_000)
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{SignableTransaction, transaction::Recovered};
    use alloy_primitives::{Address, B256, TxHash, U256};
    use alloy_provider::network::TxSignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use op_alloy_consensus::OpTxEnvelope;
    use op_alloy_rpc_types::OpTransactionRequest;
    use uuid::Uuid;

    use super::*;

    fn create_recovered_tx(
        from: &PrivateKeySigner,
        nonce: u64,
        to: Address,
    ) -> Recovered<OpTxEnvelope> {
        let mut txn = OpTransactionRequest::default()
            .value(U256::from(10_000))
            .gas_limit(21_000)
            .max_fee_per_gas(200)
            .max_priority_fee_per_gas(100)
            .from(from.address())
            .to(to)
            .nonce(nonce)
            .build_typed_tx()
            .unwrap();

        let sig = from.sign_transaction_sync(&mut txn).unwrap();
        let envelope = OpTxEnvelope::Eip1559(txn.eip1559().cloned().unwrap().into_signed(sig));
        Recovered::new_unchecked(envelope, from.address())
    }

    fn create_test_accepted_bundle(txs: Vec<Recovered<OpTxEnvelope>>) -> AcceptedBundle {
        AcceptedBundle {
            uuid: Uuid::new_v4(),
            txs,
            block_number: 1,
            flashblock_number_min: None,
            flashblock_number_max: None,
            min_timestamp: None,
            max_timestamp: None,
            reverting_tx_hashes: vec![],
            replacement_uuid: None,
            dropping_tx_hashes: vec![],
            meter_bundle_response: MeterBundleResponse {
                bundle_gas_price: U256::ZERO,
                bundle_hash: TxHash::ZERO,
                coinbase_diff: U256::ZERO,
                eth_sent_to_coinbase: U256::ZERO,
                gas_fees: U256::ZERO,
                results: vec![],
                state_block_number: 0,
                state_flashblock_index: None,
                total_gas_used: 0,
                total_execution_time_us: 0,
                state_root_time_us: 0,
            },
        }
    }

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
            state_root_time_us: 0,
        }
    }

    #[test]
    fn test_unified_get() {
        let store = TxDataStore::new(true, 100);
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let target_tx = create_recovered_tx(&alice, 0, bob.address());
        let backrun_tx = create_recovered_tx(&alice, 1, bob.address());
        let target_tx_hash = target_tx.tx_hash();

        store.insert_metering(target_tx_hash, create_test_metering(21000));

        let bundle = create_test_accepted_bundle(vec![target_tx, backrun_tx]);
        store.insert_backrun_bundle(bundle).unwrap();

        let result = store.get(&target_tx_hash);
        assert!(result.metering.is_some());
        assert_eq!(result.metering.as_ref().unwrap().total_gas_used, 21000);
        assert_eq!(result.backrun_bundles.len(), 1);
    }

    #[test]
    fn test_backrun_bundle_store() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let target_tx = create_recovered_tx(&alice, 0, bob.address());
        let backrun_tx1 = create_recovered_tx(&alice, 1, bob.address());
        let backrun_tx2 = create_recovered_tx(&alice, 2, bob.address());

        let target_tx_hash = target_tx.tx_hash();

        let store = TxDataStore::new(false, 100);

        let single_tx_bundle = create_test_accepted_bundle(vec![target_tx.clone()]);
        assert!(store.insert_backrun_bundle(single_tx_bundle).is_err());
        assert_eq!(store.len(), 0);

        let valid_bundle =
            create_test_accepted_bundle(vec![target_tx.clone(), backrun_tx1.clone()]);
        assert!(store.insert_backrun_bundle(valid_bundle).is_ok());
        assert_eq!(store.len(), 1);

        let data = store.get(&target_tx_hash);
        assert_eq!(data.backrun_bundles.len(), 1);
        assert_eq!(data.backrun_bundles[0].backrun_txs.len(), 1);
        assert_eq!(*data.backrun_bundles[0].backrun_txs[0].hash(), backrun_tx1.tx_hash());

        let replacement_bundle = create_test_accepted_bundle(vec![target_tx, backrun_tx2.clone()]);
        assert!(store.insert_backrun_bundle(replacement_bundle).is_ok());
        assert_eq!(store.len(), 1);

        let data = store.get(&target_tx_hash);
        assert_eq!(data.backrun_bundles.len(), 1);
        assert_eq!(*data.backrun_bundles[0].backrun_txs[0].hash(), backrun_tx2.tx_hash());

        store.remove_backrun_bundles(&target_tx_hash);
        assert!(store.get(&target_tx_hash).backrun_bundles.is_empty());
    }

    #[test]
    fn test_backrun_bundle_multiple_senders() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();
        let charlie = PrivateKeySigner::random();

        let target_tx = create_recovered_tx(&alice, 0, bob.address());
        let alice_backrun = create_recovered_tx(&alice, 1, bob.address());
        let charlie_backrun = create_recovered_tx(&charlie, 0, bob.address());

        let target_tx_hash = target_tx.tx_hash();
        let store = TxDataStore::new(false, 100);

        let alice_bundle = create_test_accepted_bundle(vec![target_tx.clone(), alice_backrun]);
        store.insert_backrun_bundle(alice_bundle).unwrap();

        let charlie_bundle = create_test_accepted_bundle(vec![target_tx, charlie_backrun]);
        store.insert_backrun_bundle(charlie_bundle).unwrap();

        let data = store.get(&target_tx_hash);
        assert_eq!(data.backrun_bundles.len(), 2);
    }

    #[test]
    fn test_lru_eviction() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let store = TxDataStore::new(false, 2);

        for nonce in 0..3u64 {
            let target = create_recovered_tx(&alice, nonce * 2, bob.address());
            let backrun = create_recovered_tx(&alice, nonce * 2 + 1, bob.address());
            let bundle = create_test_accepted_bundle(vec![target, backrun]);
            let _ = store.insert_backrun_bundle(bundle);
        }

        assert_eq!(store.len(), 2);
    }

    #[test]
    fn test_metering_insert_and_get() {
        let store = TxDataStore::new(true, 100);
        let tx_hash = TxHash::random();
        let meter_data = create_test_metering(21000);

        store.insert_metering(tx_hash, meter_data);
        let data = store.get(&tx_hash);
        assert_eq!(data.metering.as_ref().unwrap().total_gas_used, 21000);

        store.insert_metering(tx_hash, create_test_metering(50000));
        let data = store.get(&tx_hash);
        assert_eq!(data.metering.as_ref().unwrap().total_gas_used, 50000);
    }

    #[test]
    fn test_clear_metering() {
        let store = TxDataStore::new(true, 100);

        let tx1 = TxHash::random();
        let tx2 = TxHash::random();

        store.insert_metering(tx1, create_test_metering(1000));
        store.insert_metering(tx2, create_test_metering(2000));

        assert!(store.get(&tx1).metering.is_some());
        assert!(store.get(&tx2).metering.is_some());

        store.clear_metering();

        assert!(store.get(&tx1).metering.is_none());
        assert!(store.get(&tx2).metering.is_none());
    }
}
