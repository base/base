use alloy_consensus::{Transaction, transaction::Recovered};
use alloy_primitives::{Address, TxHash};
use concurrent_queue::ConcurrentQueue;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};
use op_alloy_consensus::OpTxEnvelope;
use std::{collections::HashMap, fmt::Debug, sync::Arc, time::Instant};
use tips_core::AcceptedBundle;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::metrics::OpRBuilderMetrics;

#[derive(Clone)]
pub struct StoredBackrunBundle {
    pub bundle_id: Uuid,
    pub sender: Address,
    pub backrun_txs: Vec<Recovered<OpTxEnvelope>>,
    pub total_priority_fee: u128,
}

struct BackrunData {
    by_target_tx: dashmap::DashMap<TxHash, HashMap<Address, StoredBackrunBundle>>,
    lru: ConcurrentQueue<TxHash>,
}

#[derive(Clone)]
pub struct BackrunBundleStore {
    data: Arc<BackrunData>,
    metrics: OpRBuilderMetrics,
}

impl Debug for BackrunBundleStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackrunBundleStore")
            .field("by_target_tx_count", &self.data.by_target_tx.len())
            .finish()
    }
}

impl BackrunBundleStore {
    pub fn new(buffer_size: usize) -> Self {
        Self {
            data: Arc::new(BackrunData {
                by_target_tx: dashmap::DashMap::new(),
                lru: ConcurrentQueue::bounded(buffer_size),
            }),
            metrics: OpRBuilderMetrics::default(),
        }
    }

    pub fn insert(&self, bundle: AcceptedBundle) -> Result<(), String> {
        if bundle.txs.len() < 2 {
            return Err("Bundle must have at least 2 transactions (target + backrun)".to_string());
        }

        let target_tx_hash = bundle.txs[0].tx_hash();
        let backrun_txs: Vec<Recovered<OpTxEnvelope>> = bundle.txs[1..].to_vec();
        let backrun_sender = backrun_txs[0].signer();

        if self.data.lru.is_full()
            && let Ok(evicted_hash) = self.data.lru.pop()
        {
            self.data.by_target_tx.remove(&evicted_hash);
            warn!(
                target: "backrun_bundles",
                evicted_target = ?evicted_hash,
                "Evicted old backrun bundle"
            );
        }

        let _ = self.data.lru.push(target_tx_hash);

        let total_priority_fee: u128 = backrun_txs
            .iter()
            .map(|tx| tx.max_priority_fee_per_gas().unwrap_or(0))
            .sum();

        let stored_bundle = StoredBackrunBundle {
            bundle_id: *bundle.uuid(),
            sender: backrun_sender,
            backrun_txs,
            total_priority_fee,
        };

        let replaced = {
            let mut entry = self.data.by_target_tx.entry(target_tx_hash).or_default();
            entry.insert(backrun_sender, stored_bundle).is_some()
        };

        if replaced {
            info!(
                target: "backrun_bundles",
                target_tx = ?target_tx_hash,
                sender = ?backrun_sender,
                bundle_id = ?bundle.uuid(),
                "Replaced existing backrun bundle from same sender"
            );
        }

        self.metrics
            .backrun_bundles_in_store
            .set(self.data.by_target_tx.len() as f64);

        Ok(())
    }

    pub fn get(&self, target_tx_hash: &TxHash) -> Option<Vec<StoredBackrunBundle>> {
        self.data.by_target_tx.get(target_tx_hash).map(|entry| {
            let mut bundles: Vec<_> = entry.values().cloned().collect();
            // Sort bundles by total_priority_fee (descending)
            bundles.sort_by(|a, b| b.total_priority_fee.cmp(&a.total_priority_fee));
            bundles
        })
    }

    pub fn remove(&self, target_tx_hash: &TxHash) {
        if let Some((_, bundles)) = self.data.by_target_tx.remove(target_tx_hash) {
            debug!(
                target: "backrun_bundles",
                target_tx = ?target_tx_hash,
                bundle_count = bundles.len(),
                "Removed backrun bundles"
            );

            self.metrics
                .backrun_bundles_in_store
                .set(self.data.by_target_tx.len() as f64);
        }
    }

    pub fn len(&self) -> usize {
        self.data.by_target_tx.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.by_target_tx.is_empty()
    }
}

impl Default for BackrunBundleStore {
    fn default() -> Self {
        Self::new(10_000)
    }
}

#[cfg_attr(not(test), rpc(server, namespace = "base"))]
#[cfg_attr(test, rpc(server, client, namespace = "base"))]
pub trait BaseBundlesApiExt {
    #[method(name = "sendBackrunBundle")]
    async fn send_backrun_bundle(&self, bundle: AcceptedBundle) -> RpcResult<()>;
}

pub(crate) struct BundlesApiExt {
    bundle_store: BackrunBundleStore,
    metrics: OpRBuilderMetrics,
}

impl BundlesApiExt {
    pub(crate) fn new(bundle_store: BackrunBundleStore) -> Self {
        Self {
            bundle_store,
            metrics: OpRBuilderMetrics::default(),
        }
    }
}

#[async_trait]
impl BaseBundlesApiExtServer for BundlesApiExt {
    async fn send_backrun_bundle(&self, bundle: AcceptedBundle) -> RpcResult<()> {
        self.metrics.backrun_bundles_received_total.increment(1);

        let start = Instant::now();
        self.bundle_store.insert(bundle).map_err(|e| {
            warn!(target: "backrun_bundles", error = %e, "Failed to store bundle");
            jsonrpsee::types::ErrorObject::owned(
                jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                format!("Failed to store bundle: {e}"),
                None::<()>,
            )
        })?;
        self.metrics
            .backrun_bundle_insert_duration
            .record(start.elapsed().as_secs_f64());

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::SignableTransaction;
    use alloy_primitives::{Address, TxHash, U256};
    use alloy_provider::network::TxSignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use op_alloy_consensus::OpTxEnvelope;
    use op_alloy_rpc_types::OpTransactionRequest;
    use tips_core::MeterBundleResponse;

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
        let envelope =
            OpTxEnvelope::Eip1559(txn.eip1559().cloned().unwrap().into_signed(sig).clone());
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
            },
        }
    }

    #[test]
    fn test_backrun_bundle_store() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        let target_tx = create_recovered_tx(&alice, 0, bob.address());
        let backrun_tx1 = create_recovered_tx(&alice, 1, bob.address());
        let backrun_tx2 = create_recovered_tx(&alice, 2, bob.address());

        let target_tx_hash = target_tx.tx_hash();

        let store = BackrunBundleStore::new(100);

        // Test insert fails with only 1 tx (need target + at least 1 backrun)
        let single_tx_bundle = create_test_accepted_bundle(vec![target_tx.clone()]);
        assert!(store.insert(single_tx_bundle).is_err());
        assert_eq!(store.len(), 0);

        // Test insert succeeds with 2+ txs
        let valid_bundle =
            create_test_accepted_bundle(vec![target_tx.clone(), backrun_tx1.clone()]);
        assert!(store.insert(valid_bundle).is_ok());
        assert_eq!(store.len(), 1);

        // Test get returns the backrun txs (not the target)
        let retrieved = store.get(&target_tx_hash).unwrap();
        assert_eq!(retrieved.len(), 1);
        assert_eq!(retrieved[0].backrun_txs.len(), 1);
        assert_eq!(retrieved[0].backrun_txs[0].tx_hash(), backrun_tx1.tx_hash());

        // Test same sender replaces previous bundle (not accumulate)
        let replacement_bundle =
            create_test_accepted_bundle(vec![target_tx.clone(), backrun_tx2.clone()]);
        assert!(store.insert(replacement_bundle).is_ok());
        assert_eq!(store.len(), 1);

        let retrieved = store.get(&target_tx_hash).unwrap();
        assert_eq!(retrieved.len(), 1); // Still 1 bundle (replaced, not accumulated)
        assert_eq!(retrieved[0].backrun_txs[0].tx_hash(), backrun_tx2.tx_hash()); // New tx

        // Test remove
        store.remove(&target_tx_hash);
        assert_eq!(store.len(), 0);
        assert!(store.get(&target_tx_hash).is_none());

        // Test remove on non-existent key doesn't panic
        store.remove(&TxHash::ZERO);
    }

    #[test]
    fn test_backrun_bundle_store_multiple_senders() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();
        let charlie = PrivateKeySigner::random();

        let target_tx = create_recovered_tx(&alice, 0, bob.address());
        let alice_backrun = create_recovered_tx(&alice, 1, bob.address());
        let charlie_backrun = create_recovered_tx(&charlie, 0, bob.address());

        let target_tx_hash = target_tx.tx_hash();
        let store = BackrunBundleStore::new(100);

        // Alice submits backrun
        let alice_bundle = create_test_accepted_bundle(vec![target_tx.clone(), alice_backrun]);
        store.insert(alice_bundle).unwrap();

        // Charlie submits backrun for same target
        let charlie_bundle = create_test_accepted_bundle(vec![target_tx.clone(), charlie_backrun]);
        store.insert(charlie_bundle).unwrap();

        // Both bundles should exist (different senders)
        let retrieved = store.get(&target_tx_hash).unwrap();
        assert_eq!(retrieved.len(), 2);
    }

    #[test]
    fn test_backrun_bundle_store_lru_eviction() {
        let alice = PrivateKeySigner::random();
        let bob = PrivateKeySigner::random();

        // Small buffer to test eviction
        let store = BackrunBundleStore::new(2);

        // Insert 3 bundles, first should be evicted
        for nonce in 0..3u64 {
            let target = create_recovered_tx(&alice, nonce * 2, bob.address());
            let backrun = create_recovered_tx(&alice, nonce * 2 + 1, bob.address());
            let bundle = create_test_accepted_bundle(vec![target, backrun]);
            let _ = store.insert(bundle);
        }

        // Only 2 should remain due to LRU eviction
        assert_eq!(store.len(), 2);
    }
}
