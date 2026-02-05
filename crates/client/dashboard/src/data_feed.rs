//! SSE event stream producer for dashboard data.

use std::{convert::Infallible, sync::Arc, time::Duration};

use alloy_consensus::{BlockHeader as _, Transaction, Typed2718};
use futures_util::StreamExt;
use reth_chain_state::CanonStateSubscriptions;
use reth_primitives_traits::transaction::TxHashRef;
use reth_provider::BlockReader;
use reth_transaction_pool::TransactionPool;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tracing::{debug, error, info};

use crate::{
    collectors::{
        BlockCollector, PeerCollector, SystemCollector, TxFlowCollector, TxFlowCounters,
        TxPoolCollector, get_tx_nodes, receipt_to_web, tx_to_web,
    },
    types::{DashboardEvent, NodeData, ReceiptForWeb, TransactionForWeb},
};

/// Maximum number of events to buffer in the broadcast channel.
const CHANNEL_CAPACITY: usize = 256;

/// Interval for periodic metrics collection (system, peers, txpool).
const METRICS_INTERVAL: Duration = Duration::from_secs(1);

/// Data feed that produces SSE events from node data sources.
pub(crate) struct DataFeed<Provider, Pool> {
    /// Event broadcast sender.
    tx: broadcast::Sender<DashboardEvent>,
    /// Provider for block/state data.
    provider: Provider,
    /// Transaction pool.
    pool: Pool,
    /// Callback that returns (connected, inbound, outbound) peer counts.
    peers_fn: Arc<dyn Fn() -> (usize, usize, usize) + Send + Sync>,
    /// Shared transaction flow counters.
    tx_flow_counters: Arc<TxFlowCounters>,
}

impl<Provider, Pool> DataFeed<Provider, Pool>
where
    Provider: BlockReader + CanonStateSubscriptions + Clone + Send + Sync + 'static,
    Pool: TransactionPool + Clone + Send + Sync + 'static,
{
    /// Creates a new data feed.
    pub(crate) fn new(
        provider: Provider,
        pool: Pool,
        peers_fn: Arc<dyn Fn() -> (usize, usize, usize) + Send + Sync>,
    ) -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx, provider, pool, peers_fn, tx_flow_counters: Arc::new(TxFlowCounters::new()) }
    }

    /// Starts the data feed background tasks.
    pub(crate) fn start(&self, network_name: String, client_name: String) {
        info!(message = "Starting dashboard data feed");

        // Spawn canonical state subscription task
        self.spawn_block_task();

        // Spawn periodic metrics task
        self.spawn_metrics_task(network_name.clone(), client_name.clone());

        // Send initial data
        self.send_initial_data(network_name, client_name);
    }

    /// Subscribes to the event stream.
    /// Returns a boxed stream that owns its resources.
    pub(crate) fn subscribe(
        &self,
    ) -> std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<warp::sse::Event, Infallible>> + Send>,
    > {
        let rx = self.tx.subscribe();
        #[allow(clippy::option_if_let_else)]
        Box::pin(BroadcastStream::new(rx).filter_map(|result| async move {
            match result {
                Ok(event) => {
                    let event_type = event.event_type();
                    match serde_json::to_string(&event) {
                        Ok(data) => {
                            Some(Ok(warp::sse::Event::default().event(event_type).data(data)))
                        }
                        Err(e) => {
                            error!(error = %e, "Failed to serialize dashboard event");
                            None
                        }
                    }
                }
                Err(_) => None, // Lagged, skip
            }
        }))
    }

    /// Sends initial data events (node info and Sankey nodes).
    fn send_initial_data(&self, network_name: String, client_name: String) {
        // Send node data. Note: sync_type and pruning_mode are hardcoded placeholders.
        // Retrieving actual values would require deeper integration with reth's config.
        let node_data = NodeData {
            uptime: 0,
            instance: String::new(),
            network: network_name,
            sync_type: "Full".to_string(),
            pruning_mode: "Archive".to_string(),
            version: client_name,
            commit: String::new(),
            runtime: "Rust".to_string(),
            gas_token: "ETH".to_string(),
        };
        let _ = self.tx.send(DashboardEvent::NodeData(node_data));

        // Send Sankey diagram node definitions
        let tx_nodes = get_tx_nodes();
        let _ = self.tx.send(DashboardEvent::TxNodes(tx_nodes));
    }

    /// Spawns the block subscription task.
    fn spawn_block_task(&self) {
        let tx = self.tx.clone();
        let provider = self.provider.clone();
        let tx_flow_counters = Arc::clone(&self.tx_flow_counters);

        tokio::spawn(async move {
            let mut block_collector = BlockCollector::new();
            let mut stream = BroadcastStream::new(provider.subscribe_to_canonical_state());

            while let Some(Ok(notification)) = stream.next().await {
                // Collect all block data as owned values to avoid borrow issues
                let blocks_data: Vec<_> = {
                    let committed = notification.committed();
                    committed
                        .blocks_iter()
                        .map(|block| {
                            let header = block.header();
                            let block_hash = block.hash();
                            let block_num = header.number();
                            let extra_data_len = header.extra_data().len();

                            // Extract all transaction data as owned values
                            let tx_data: Vec<_> = block
                                .transactions_with_sender()
                                .map(|(sender, tx)| {
                                    (
                                        *tx.tx_hash(),
                                        *sender,
                                        tx.to(),
                                        tx.ty(),
                                        tx.max_priority_fee_per_gas().unwrap_or(0),
                                        tx.max_fee_per_gas(),
                                        tx.gas_price().unwrap_or(0),
                                        tx.gas_limit(),
                                        tx.nonce(),
                                        tx.value(),
                                        tx.input().clone(),
                                        tx.blob_versioned_hashes()
                                            .map(|h| h.len() as u8)
                                            .unwrap_or(0),
                                    )
                                })
                                .collect();

                            // Calculate block size using the owned data
                            let block_size = tx_data.iter().map(|t| t.10.len()).sum::<usize>()
                                + extra_data_len
                                + 500; // Header overhead estimate

                            // Clone header data we need
                            (
                                block_num,
                                block_hash,
                                block_size,
                                header.gas_limit(),
                                header.gas_used(),
                                header.beneficiary(),
                                header.timestamp(),
                                header.base_fee_per_gas(),
                                header.blob_gas_used(),
                                header.excess_blob_gas(),
                                header.extra_data().to_vec(),
                                tx_data,
                            )
                        })
                        .collect()
                };

                // Process the collected block data
                for (
                    block_num,
                    block_hash,
                    block_size,
                    gas_limit,
                    gas_used,
                    beneficiary,
                    timestamp,
                    base_fee_per_gas,
                    blob_gas_used,
                    excess_blob_gas,
                    extra_data,
                    tx_data,
                ) in blocks_data
                {
                    let tx_count = tx_data.len();

                    // Convert transactions to web format
                    let web_txs: Vec<TransactionForWeb> = tx_data
                        .iter()
                        .map(
                            |(
                                hash,
                                sender,
                                to,
                                ty,
                                max_priority_fee,
                                max_fee,
                                gas_price,
                                gas_limit,
                                nonce,
                                value,
                                input,
                                blob_count,
                            )| {
                                tx_to_web(
                                    *hash,
                                    *sender,
                                    *to,
                                    *ty,
                                    *max_priority_fee,
                                    *max_fee,
                                    *gas_price,
                                    *gas_limit,
                                    *nonce,
                                    *value,
                                    input.clone(),
                                    *blob_count,
                                )
                            },
                        )
                        .collect();

                    // Create placeholder receipts using gas estimates. Actual receipt data
                    // (logs, contract addresses) would require querying executed receipts.
                    let web_receipts: Vec<ReceiptForWeb> = tx_data
                        .iter()
                        .map(|(_, _, _, _, _, _, gas_price, gas_limit, _, _, _, _)| {
                            receipt_to_web(*gas_limit, *gas_price, None, None, None, vec![], true)
                        })
                        .collect();

                    // Update finality estimates
                    if block_num > 64 {
                        block_collector.update_finality(
                            block_num.saturating_sub(32),
                            block_num.saturating_sub(64),
                        );
                    }

                    // Record transactions added to block for Sankey
                    tx_flow_counters.record_block_adds(tx_count as u64);

                    // Build fork choice data inline since we have owned header data
                    let block_web = crate::types::BlockForWeb {
                        extra_data: format!("0x{}", hex::encode(&extra_data)),
                        gas_limit: format!("0x{gas_limit:x}"),
                        gas_used: format!("0x{gas_used:x}"),
                        hash: format!("0x{block_hash:x}"),
                        beneficiary: format!("0x{beneficiary:x}"),
                        number: format!("0x{block_num:x}"),
                        size: format!("0x{block_size:x}"),
                        timestamp: format!("0x{timestamp:x}"),
                        base_fee_per_gas: base_fee_per_gas
                            .map(|f| format!("0x{f:x}"))
                            .unwrap_or_else(|| "0x0".to_string()),
                        blob_gas_used: blob_gas_used
                            .map(|g| format!("0x{g:x}"))
                            .unwrap_or_else(|| "0x0".to_string()),
                        excess_blob_gas: excess_blob_gas
                            .map(|g| format!("0x{g:x}"))
                            .unwrap_or_else(|| "0x0".to_string()),
                        tx: web_txs,
                        receipts: web_receipts,
                    };

                    let fork_choice = crate::types::ForkChoiceData {
                        head: block_web,
                        safe: format!("0x{:x}", block_collector.safe_block()),
                        finalized: format!("0x{:x}", block_collector.finalized_block()),
                    };

                    debug!(block_number = %block_num, "Dashboard: new block");
                    let _ = tx.send(DashboardEvent::ForkChoice(fork_choice));
                }
            }
        });
    }

    /// Spawns the periodic metrics collection task.
    fn spawn_metrics_task(&self, network_name: String, client_name: String) {
        let tx = self.tx.clone();
        let pool = self.pool.clone();
        let peers_fn = Arc::clone(&self.peers_fn);
        let tx_flow_counters = Arc::clone(&self.tx_flow_counters);

        tokio::spawn(async move {
            let mut system_collector = SystemCollector::new();
            let peer_collector = PeerCollector::new();
            let txpool_collector = TxPoolCollector::new();
            let tx_flow_collector = TxFlowCollector::new(tx_flow_counters);

            let mut interval = tokio::time::interval(METRICS_INTERVAL);

            loop {
                interval.tick().await;

                // Collect and send system stats
                let system_data = system_collector.collect();
                let _ = tx.send(DashboardEvent::System(system_data.clone()));

                // Collect and send peer stats
                let (connected, inbound, outbound) = peers_fn();
                let peers_data = peer_collector.collect(connected, inbound, outbound);
                let _ = tx.send(DashboardEvent::Peers(peers_data));

                // Collect and send txpool stats (for Sankey links)
                let pool_size = txpool_collector.collect(&pool);
                let tx_pool_data =
                    tx_flow_collector.collect(pool_size.pending_count, pool_size.queued_count);
                let _ = tx.send(DashboardEvent::TxLinks(tx_pool_data));

                // Send periodic node data updates (with uptime)
                let node_data = NodeData {
                    uptime: system_data.uptime,
                    instance: String::new(),
                    network: network_name.clone(),
                    sync_type: "Full".to_string(),
                    pruning_mode: "Archive".to_string(),
                    version: client_name.clone(),
                    commit: String::new(),
                    runtime: "Rust".to_string(),
                    gas_token: "ETH".to_string(),
                };
                let _ = tx.send(DashboardEvent::NodeData(node_data));
            }
        });
    }
}

impl<Provider, Pool> std::fmt::Debug for DataFeed<Provider, Pool> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataFeed").finish_non_exhaustive()
    }
}
