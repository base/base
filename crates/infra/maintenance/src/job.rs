use crate::Args;
use alloy_primitives::TxHash;
use alloy_primitives::map::HashMap;
use alloy_provider::Provider;
use alloy_rpc_types::BlockId;
use alloy_rpc_types::BlockNumberOrTag::Latest;
use alloy_rpc_types::eth::Block;
use anyhow::Result;
use base_reth_flashblocks_rpc::subscription::{Flashblock, FlashblocksReceiver};
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_network::eip2718::Decodable2718;
use op_alloy_network::{Optimism, TransactionResponse};
use op_alloy_rpc_types::Transaction;
use sqlx::types::chrono::Utc;
use std::collections::HashSet;
use std::time::Duration;
use tips_audit::{BundleEvent, BundleEventPublisher, DropReason};
use tips_datastore::BundleDatastore;
use tips_datastore::postgres::{BlockInfoUpdate, BundleFilter, BundleState, BundleWithMetadata};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

pub struct MaintenanceJob<S: BundleDatastore, P: Provider<Optimism>, K: BundleEventPublisher> {
    pub store: S,
    pub node: P,
    pub publisher: K,
    pub args: Args,
    fb_tx: mpsc::UnboundedSender<Flashblock>,
}

impl<S: BundleDatastore, P: Provider<Optimism>, K: BundleEventPublisher> MaintenanceJob<S, P, K> {
    pub fn new(
        store: S,
        node: P,
        publisher: K,
        args: Args,
        fb_tx: mpsc::UnboundedSender<Flashblock>,
    ) -> Self {
        Self {
            store,
            node,
            publisher,
            args,
            fb_tx,
        }
    }

    async fn execute(&self) -> Result<()> {
        let latest_block = self
            .node
            .get_block(BlockId::Number(Latest))
            .full()
            .await?
            .ok_or_else(|| anyhow::anyhow!("Failed to get latest block"))?;

        debug!(
            message = "Executing up to latest block",
            block_number = latest_block.number()
        );

        let block_info = self.store.get_current_block_info().await?;

        if let Some(current_block_info) = block_info {
            if latest_block.header.number > current_block_info.latest_block_number {
                // Process all blocks between stored latest and current latest
                for block_num in
                    (current_block_info.latest_block_number + 1)..=latest_block.header.number
                {
                    debug!(message = "Fetching block number", ?latest_block);

                    let block = self
                        .node
                        .get_block(BlockId::Number(alloy_rpc_types::BlockNumberOrTag::Number(
                            block_num,
                        )))
                        .full()
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("Failed to get block {block_num}"))?;

                    let hash = block.hash();
                    self.on_new_block(block).await?;
                    self.store
                        .commit_block_info(vec![BlockInfoUpdate {
                            block_number: block_num,
                            block_hash: hash,
                        }])
                        .await?;
                }
            }
        } else {
            warn!("No block info found in database, initializing with latest block as finalized");
            let block_update = BlockInfoUpdate {
                block_number: latest_block.header.number,
                block_hash: latest_block.header.hash,
            };
            self.store.commit_block_info(vec![block_update]).await?;
            self.store
                .finalize_blocks_before(latest_block.header.number + 1)
                .await?;
        }

        // Finalize blocks that are old enough
        if latest_block.header.number > self.args.finalization_depth {
            self.store
                .finalize_blocks_before(latest_block.header.number - self.args.finalization_depth)
                .await?;
        }

        Ok(())
    }

    async fn periodic_maintenance(&self) -> Result<()> {
        let cutoff_time = Utc::now() - Duration::from_secs(self.args.finalization_depth * 2);

        let removed_included_bundle_uuids =
            self.store.remove_old_included_bundles(cutoff_time).await?;

        if !removed_included_bundle_uuids.is_empty() {
            info!(
                deleted_count = removed_included_bundle_uuids.len(),
                "Removed old included bundles"
            );
        }

        let current_time = Utc::now().timestamp() as u64;

        let expired_bundle_uuids = self.store.remove_timed_out_bundles(current_time).await?;

        if !expired_bundle_uuids.is_empty() {
            let events: Vec<BundleEvent> = expired_bundle_uuids
                .iter()
                .map(|&bundle_id| BundleEvent::Dropped {
                    bundle_id,
                    reason: DropReason::TimedOut,
                })
                .collect();

            self.publisher.publish_all(events).await?;

            info!(
                deleted_count = expired_bundle_uuids.len(),
                "Deleted expired bundles with timeout"
            );
        }

        Ok(())
    }

    pub async fn run(&self, mut fb_rx: mpsc::UnboundedReceiver<Flashblock>) -> Result<()> {
        let mut maintenance_interval =
            tokio::time::interval(Duration::from_millis(self.args.maintenance_interval_ms));
        let mut execution_interval =
            tokio::time::interval(Duration::from_millis(self.args.rpc_poll_interval));

        loop {
            tokio::select! {
                _ = maintenance_interval.tick() => {
                    info!(message = "starting maintenance");
                    match self.periodic_maintenance().await {
                        Ok(_) => {
                            info!(message = "Periodic maintenance completed");
                        },
                        Err(err) => {
                            error!(message = "Error in periodic maintenance", error = %err);
                        }

                    }
                }
                _ = execution_interval.tick() => {
                    info!(message = "starting execution run");
                    match self.execute().await {
                        Ok(_) => {
                            info!(message = "Successfully executed maintenance run");
                        }
                        Err(e) => {
                            error!(message = "Error executing maintenance run", error = %e);
                        }
                    }
                }
                Some(flashblock) = fb_rx.recv() => {
                    info!(message = "starting flashblock processing");
                    match self.process_flashblock(flashblock).await {
                        Ok(_) => {
                            info!(message = "Successfully processed flashblock");
                        }
                        Err(e) => {
                            error!(message = "Error processing flashblock", error = %e);
                        }
                    }
                }
            }
        }
    }

    pub async fn process_transactions(&self, transaction_hashes: Vec<TxHash>) -> Result<Vec<Uuid>> {
        let filter = BundleFilter::new()
            .with_status(BundleState::Ready)
            .with_txn_hashes(transaction_hashes.clone());

        let bundles = self.store.select_bundles(filter).await?;

        if bundles.is_empty() {
            return Ok(vec![]);
        }

        let bundle_match = map_transactions_to_bundles(bundles, transaction_hashes);
        Ok(bundle_match
            .matched
            .values()
            .map(|uuid_str| Uuid::parse_str(uuid_str).unwrap())
            .collect())
    }

    pub async fn on_new_block(&self, block: Block<Transaction>) -> Result<()> {
        let transaction_hashes: Vec<TxHash> = block
            .transactions
            .as_transactions()
            .unwrap_or(&[])
            .iter()
            .filter(|tx| !tx.inner.inner.is_deposit())
            .map(|tx| tx.tx_hash())
            .collect();

        if transaction_hashes.is_empty() {
            return Ok(());
        }

        let bundle_uuids = self.process_transactions(transaction_hashes).await?;

        if !bundle_uuids.is_empty() && self.args.update_included_by_builder {
            let updated_uuids = self
                .store
                .update_bundles_state(
                    bundle_uuids.clone(),
                    vec![BundleState::Ready],
                    BundleState::IncludedByBuilder,
                )
                .await?;

            info!(
                updated_count = updated_uuids.len(),
                block_hash = %block.header.hash,
                "Updated bundle states to IncludedByBuilder"
            );
        }

        let events: Vec<BundleEvent> = bundle_uuids
            .into_iter()
            .map(|bundle_id| BundleEvent::BlockIncluded {
                bundle_id,
                block_number: block.header.number,
                block_hash: block.header.hash,
            })
            .collect();

        self.publisher.publish_all(events).await?;

        Ok(())
    }

    async fn process_flashblock(&self, flashblock: Flashblock) -> Result<()> {
        let transaction_hashes: Vec<TxHash> = flashblock
            .diff
            .transactions
            .iter()
            .map(|tx| OpTxEnvelope::decode_2718_exact(tx).unwrap().tx_hash())
            .collect();

        if transaction_hashes.is_empty() {
            return Ok(());
        }

        let bundle_uuids = self.process_transactions(transaction_hashes).await?;

        let events: Vec<BundleEvent> = bundle_uuids
            .into_iter()
            .map(|bundle_id| BundleEvent::FlashblockIncluded {
                bundle_id,
                block_number: flashblock.metadata.block_number,
                flashblock_index: flashblock.index,
            })
            .collect();

        self.publisher.publish_all(events).await?;

        Ok(())
    }
}

impl<S: BundleDatastore, P: Provider<Optimism>, K: BundleEventPublisher> FlashblocksReceiver
    for MaintenanceJob<S, P, K>
{
    fn on_flashblock_received(&self, flashblock: Flashblock) {
        if let Err(e) = self.fb_tx.send(flashblock) {
            error!("Failed to send flashblock to queue: {:?}", e);
        }
    }
}

struct BundleMatch {
    matched: HashMap<TxHash, String>,
    unmatched_transactions: HashSet<TxHash>,
    unmatched_bundles: HashSet<String>,
}

struct TrieNode {
    next: HashMap<TxHash, TrieNode>,
    bundle_uuid: Option<String>,
}

impl TrieNode {
    fn new() -> Self {
        Self {
            next: HashMap::default(),
            bundle_uuid: None,
        }
    }

    fn get(&mut self, t: &TxHash) -> &mut TrieNode {
        self.next.entry(*t).or_insert_with(TrieNode::new)
    }

    fn adv(&self, t: &TxHash) -> Option<&TrieNode> {
        self.next.get(t)
    }

    fn has_further_items(&self) -> bool {
        !self.next.is_empty()
    }
}

/// Map transactions that were included in a block, to the bundles
/// This method only supports two cases, non duplicate single bundle transactions (e.g. standard mempool)
/// or non ambiguous bundles, e.g. [(a,b), (a), (b)] is not supported
fn map_transactions_to_bundles(
    bundles: Vec<BundleWithMetadata>,
    transactions: Vec<TxHash>,
) -> BundleMatch {
    let bundle_uuids: Vec<String> = bundles
        .iter()
        .map(|b| b.bundle.replacement_uuid.clone().unwrap())
        .collect();

    let mut result = BundleMatch {
        matched: HashMap::default(),
        unmatched_transactions: transactions.iter().copied().collect(),
        unmatched_bundles: bundle_uuids.iter().cloned().collect(),
    };

    let mut trie = TrieNode::new();
    for (bundle, uuid) in bundles.into_iter().zip(bundle_uuids.iter()) {
        let mut trie_ptr = &mut trie;
        for txn in &bundle.txn_hashes {
            trie_ptr = trie_ptr.get(txn);
        }
        trie_ptr.bundle_uuid = Some(uuid.clone());
    }

    let mut i = 0;
    while i < transactions.len() {
        let mut trie_ptr = &trie;
        let mut txn_path = Vec::new();
        let start_index = i;

        while i < transactions.len() {
            let txn = transactions[i];

            if let Some(next_node) = trie_ptr.adv(&txn) {
                trie_ptr = next_node;
                txn_path.push(txn);
                i += 1;

                if let Some(ref bundle_uuid) = trie_ptr.bundle_uuid {
                    if trie_ptr.has_further_items() {
                        warn!(message = "ambiguous transaction, in multiple bundles", txn = %txn);
                    }

                    for &path_txn in &txn_path {
                        result.matched.insert(path_txn, bundle_uuid.clone());
                        result.unmatched_transactions.remove(&path_txn);
                    }
                    result.unmatched_bundles.remove(bundle_uuid);
                    break;
                }
            } else {
                i = start_index + 1;
                break;
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{TxHash, b256};
    use alloy_rpc_types_mev::EthSendBundle;
    use sqlx::types::chrono::Utc;
    use tips_datastore::postgres::BundleState;

    const TX_1: TxHash = b256!("1111111111111111111111111111111111111111111111111111111111111111");
    const TX_2: TxHash = b256!("2222222222222222222222222222222222222222222222222222222222222222");
    const TX_3: TxHash = b256!("3333333333333333333333333333333333333333333333333333333333333333");
    const TX_4: TxHash = b256!("4444444444444444444444444444444444444444444444444444444444444444");
    const TX_UNMATCHED: TxHash =
        b256!("9999999999999999999999999999999999999999999999999999999999999999");

    fn create_test_bundle(uuid: &str, txn_hashes: Vec<TxHash>) -> BundleWithMetadata {
        let replacement_uuid = Some(uuid.to_string());
        let mut bundle = EthSendBundle::default();
        bundle.replacement_uuid = replacement_uuid;

        BundleWithMetadata {
            bundle,
            txn_hashes,
            senders: vec![],
            min_base_fee: 0,
            state: BundleState::Ready,
            state_changed_at: Utc::now(),
        }
    }

    #[test]
    fn test_empty_inputs() {
        let result = map_transactions_to_bundles(vec![], vec![]);
        assert!(result.matched.is_empty());
        assert!(result.unmatched_transactions.is_empty());
        assert!(result.unmatched_bundles.is_empty());
    }

    #[test]
    fn test_single_transaction_bundles() {
        let bundles = vec![
            create_test_bundle("bundle1", vec![TX_1]),
            create_test_bundle("bundle2", vec![TX_2]),
            create_test_bundle("bundle3", vec![TX_3]),
        ];
        let transactions = vec![TX_1, TX_3, TX_2, TX_UNMATCHED];

        let result = map_transactions_to_bundles(bundles, transactions);

        assert_eq!(result.matched.len(), 3);
        assert_eq!(result.unmatched_transactions.len(), 1);
        assert!(result.unmatched_transactions.contains(&TX_UNMATCHED));
        assert!(result.unmatched_bundles.is_empty());

        assert!(result.matched.contains_key(&TX_1));
        assert_eq!(result.matched.get(&TX_1).unwrap(), "bundle1");
        assert!(result.matched.contains_key(&TX_2));
        assert_eq!(result.matched.get(&TX_2).unwrap(), "bundle2");
        assert!(result.matched.contains_key(&TX_3));
        assert_eq!(result.matched.get(&TX_3).unwrap(), "bundle3");
    }

    #[test]
    fn test_multi_transaction_bundles() {
        let bundles = vec![
            create_test_bundle("bundle1", vec![TX_1, TX_2]),
            create_test_bundle("bundle2", vec![TX_3]),
            create_test_bundle("bundle3", vec![TX_4]),
        ];
        let transactions = vec![TX_1, TX_2, TX_4, TX_3, TX_UNMATCHED];
        let result = map_transactions_to_bundles(bundles, transactions);

        assert_eq!(result.matched.len(), 4);
        assert_eq!(result.matched.get(&TX_1).unwrap(), "bundle1");
        assert_eq!(result.matched.get(&TX_2).unwrap(), "bundle1");
        assert_eq!(result.matched.get(&TX_3).unwrap(), "bundle2");
        assert_eq!(result.matched.get(&TX_4).unwrap(), "bundle3");
        assert_eq!(result.unmatched_transactions.len(), 1);
        assert!(result.unmatched_transactions.contains(&TX_UNMATCHED));
    }

    #[test]
    fn test_partial_bundles_dont_match() {
        let bundles = vec![create_test_bundle("bundle1", vec![TX_1, TX_2, TX_3])];
        let transactions = vec![TX_1, TX_2];

        let result = map_transactions_to_bundles(bundles, transactions);

        assert_eq!(result.matched.len(), 0);
        assert_eq!(result.unmatched_transactions.len(), 2);
        assert_eq!(result.unmatched_bundles.len(), 1);
    }

    #[test]
    fn test_ambiguous_bundle_match() {
        let bundles = vec![
            create_test_bundle("bundle1", vec![TX_1]),
            create_test_bundle("bundle2", vec![TX_1, TX_2]),
        ];
        let transactions = vec![TX_1, TX_2];

        let result = map_transactions_to_bundles(bundles, transactions);

        assert_eq!(result.matched.len(), 1);
        assert_eq!(result.matched.get(&TX_1).unwrap(), "bundle1");
        assert!(result.unmatched_transactions.contains(&TX_2));
        assert!(result.unmatched_bundles.contains("bundle2"));
    }
}
