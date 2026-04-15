//! Query processor for L1 watcher RPC requests.

use std::{sync::Arc, time::Instant};

use alloy_eips::BlockId;
use async_trait::async_trait;
use base_consensus_genesis::RollupConfig;
use base_consensus_rpc::{L1State, L1WatcherQueries};
use base_protocol::BlockInfo;
use futures::StreamExt;
use tokio::{
    select,
    sync::{mpsc, oneshot, watch},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use super::L1BlockFetcher;
use crate::{NodeActor, actors::l1_watcher::error::L1WatcherActorError};

/// Executes individual L1 watcher queries against live L1 RPC state.
#[derive(Debug)]
pub struct L1WatcherQueryExecutor<L1Provider>
where
    L1Provider: L1BlockFetcher,
{
    /// The rollup configuration to return for config queries.
    rollup_config: Arc<RollupConfig>,
    /// The L1 provider used for live block lookups.
    l1_provider: Arc<L1Provider>,
    /// Receiver for the most recent L1 head observed by the watcher actor.
    latest_head: watch::Receiver<Option<BlockInfo>>,
}

impl<L1Provider> Clone for L1WatcherQueryExecutor<L1Provider>
where
    L1Provider: L1BlockFetcher,
{
    fn clone(&self) -> Self {
        Self {
            rollup_config: Arc::clone(&self.rollup_config),
            l1_provider: Arc::clone(&self.l1_provider),
            latest_head: self.latest_head.clone(),
        }
    }
}

impl<L1Provider> L1WatcherQueryExecutor<L1Provider>
where
    L1Provider: L1BlockFetcher,
{
    /// Creates a new query executor.
    pub const fn new(
        rollup_config: Arc<RollupConfig>,
        l1_provider: Arc<L1Provider>,
        latest_head: watch::Receiver<Option<BlockInfo>>,
    ) -> Self {
        Self { rollup_config, l1_provider, latest_head }
    }

    /// Executes a single query.
    pub async fn execute(&self, query: L1WatcherQueries) {
        let query_started_at = Instant::now();

        debug!(target: "l1_watcher", "Started L1 watcher query");

        match query {
            L1WatcherQueries::Config(sender) => {
                self.execute_config_query(query_started_at, sender);
            }
            L1WatcherQueries::L1State(sender) => {
                self.execute_l1_state_query(query_started_at, sender).await;
            }
        }
    }

    /// Executes a config query.
    pub fn execute_config_query(
        &self,
        query_started_at: Instant,
        sender: oneshot::Sender<RollupConfig>,
    ) {
        if let Err(error) = sender.send((*self.rollup_config).clone()) {
            warn!(
                target: "l1_watcher",
                elapsed_ms = query_started_at.elapsed().as_millis() as u64,
                error = ?error,
                "Failed to send L1 watcher config response"
            );
        } else {
            debug!(
                target: "l1_watcher",
                elapsed_ms = query_started_at.elapsed().as_millis() as u64,
                "Completed L1 watcher config query"
            );
        }
    }

    /// Executes a live L1 state query.
    pub async fn execute_l1_state_query(
        &self,
        query_started_at: Instant,
        sender: oneshot::Sender<L1State>,
    ) {
        let current_l1 = *self.latest_head.borrow();
        let (head_l1, finalized_l1, safe_l1) = tokio::join!(
            self.query_block(BlockId::latest(), "latest"),
            self.query_block(BlockId::finalized(), "finalized"),
            self.query_block(BlockId::safe(), "safe"),
        );

        if let Err(error) = sender.send(L1State {
            current_l1,
            current_l1_finalized: finalized_l1,
            head_l1,
            safe_l1,
            finalized_l1,
        }) {
            warn!(
                target: "l1_watcher",
                elapsed_ms = query_started_at.elapsed().as_millis() as u64,
                error = ?error,
                "Failed to send L1 watcher state response"
            );
        } else {
            debug!(
                target: "l1_watcher",
                elapsed_ms = query_started_at.elapsed().as_millis() as u64,
                "Completed L1 watcher state query"
            );
        }
    }

    /// Queries a single tagged L1 block from the provider.
    pub async fn query_block(
        &self,
        block_id: BlockId,
        block_tag: &'static str,
    ) -> Option<BlockInfo> {
        trace!(target: "l1_watcher", block_tag, "Querying L1 provider block");

        match self.l1_provider.get_block(block_id).await {
            Ok(block) => block.map(|block| block.into_consensus().into()),
            Err(error) => {
                warn!(
                    target: "l1_watcher",
                    block_tag,
                    error = ?error,
                    "Failed to query L1 provider block"
                );
                None
            }
        }
    }
}

/// Actor that processes L1 watcher RPC queries with bounded concurrency.
#[derive(Debug)]
pub struct L1WatcherQueryProcessor<L1Provider>
where
    L1Provider: L1BlockFetcher,
{
    /// Executor used for the per-request query logic.
    executor: L1WatcherQueryExecutor<L1Provider>,
    /// Receiver for inbound L1 watcher queries.
    inbound_queries: mpsc::Receiver<L1WatcherQueries>,
    /// Shared cancellation token.
    cancellation: CancellationToken,
    /// Maximum number of concurrent query futures to process.
    query_concurrency: usize,
}

impl<L1Provider> L1WatcherQueryProcessor<L1Provider>
where
    L1Provider: L1BlockFetcher,
{
    /// Default concurrency for live L1 query handling.
    pub const DEFAULT_QUERY_CONCURRENCY: usize = 32;

    /// Creates a new query processor.
    pub fn new(
        rollup_config: Arc<RollupConfig>,
        l1_provider: L1Provider,
        inbound_queries: mpsc::Receiver<L1WatcherQueries>,
        latest_head: watch::Receiver<Option<BlockInfo>>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            executor: L1WatcherQueryExecutor::new(
                rollup_config,
                Arc::new(l1_provider),
                latest_head,
            ),
            inbound_queries,
            cancellation,
            query_concurrency: Self::DEFAULT_QUERY_CONCURRENCY,
        }
    }

    /// Overrides the maximum number of concurrent query futures.
    pub fn with_query_concurrency(mut self, query_concurrency: usize) -> Self {
        assert!(query_concurrency > 0, "query_concurrency must be greater than zero");
        self.query_concurrency = query_concurrency;
        self
    }
}

#[async_trait]
impl<L1Provider> NodeActor for L1WatcherQueryProcessor<L1Provider>
where
    L1Provider: L1BlockFetcher + 'static,
{
    type Error = L1WatcherActorError<BlockInfo>;
    type StartData = ();

    async fn start(self, _: Self::StartData) -> Result<(), Self::Error> {
        let cancellation = self.cancellation.clone();
        let executor = self.executor.clone();
        let query_processing = ReceiverStream::new(self.inbound_queries).for_each_concurrent(
            self.query_concurrency,
            move |query| {
                let executor = executor.clone();
                async move {
                    executor.execute(query).await;
                }
            },
        );

        tokio::pin!(query_processing);

        select! {
            _ = cancellation.cancelled() => {
                info!(target: "l1_watcher", "Received shutdown signal. Exiting L1 watcher query processor.");
                Ok(())
            }
            _ = &mut query_processing => {
                if cancellation.is_cancelled() {
                    info!(target: "l1_watcher", "L1 watcher query processor cancelled after query stream completion.");
                    Ok(())
                } else {
                    error!(target: "l1_watcher", "L1 watcher query channel closed unexpectedly, exiting query processor task.");
                    Err(L1WatcherActorError::StreamEnded)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use alloy_consensus::Header;
    use alloy_eips::BlockNumberOrTag;
    use alloy_primitives::{B256, Bloom, U256};
    use alloy_rpc_types_eth::{Block, Header as RpcHeader, Log};
    use tokio::{sync::oneshot, time::Instant};

    use super::*;

    #[derive(Debug)]
    struct MockFetcher {
        latest: Option<BlockInfo>,
        finalized: Option<BlockInfo>,
        safe: Option<BlockInfo>,
        delay: Duration,
        current_calls: Arc<AtomicUsize>,
        max_calls: Arc<AtomicUsize>,
    }

    impl MockFetcher {
        fn with_delay(delay: Duration) -> Self {
            Self {
                latest: Some(Self::block_info(10)),
                finalized: Some(Self::block_info(9)),
                safe: Some(Self::block_info(8)),
                delay,
                current_calls: Arc::new(AtomicUsize::new(0)),
                max_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn block_info(number: u64) -> BlockInfo {
            BlockInfo {
                hash: B256::from([number as u8; 32]),
                number,
                parent_hash: B256::from([number.saturating_sub(1) as u8; 32]),
                timestamp: number * 10,
            }
        }

        fn block(block_info: BlockInfo) -> Block {
            Block::empty(RpcHeader::new(Header {
                parent_hash: block_info.parent_hash,
                number: block_info.number,
                timestamp: block_info.timestamp,
                logs_bloom: Bloom::ZERO,
                difficulty: U256::ZERO,
                ..Default::default()
            }))
        }
    }

    #[async_trait]
    impl L1BlockFetcher for MockFetcher {
        type Error = String;

        async fn get_logs(&self, _: alloy_rpc_types_eth::Filter) -> Result<Vec<Log>, Self::Error> {
            Ok(vec![])
        }

        async fn get_block(&self, id: BlockId) -> Result<Option<Block>, Self::Error> {
            let current = self.current_calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_calls.fetch_max(current, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.current_calls.fetch_sub(1, Ordering::SeqCst);

            let block_info = match id {
                BlockId::Number(BlockNumberOrTag::Latest) => self.latest,
                BlockId::Number(BlockNumberOrTag::Finalized) => self.finalized,
                BlockId::Number(BlockNumberOrTag::Safe) => self.safe,
                _ => None,
            };

            Ok(block_info.map(Self::block))
        }
    }

    fn executor(
        fetcher: MockFetcher,
        current_l1: Option<BlockInfo>,
    ) -> L1WatcherQueryExecutor<MockFetcher> {
        let (_latest_head_tx, latest_head_rx) = watch::channel(current_l1);
        L1WatcherQueryExecutor::new(
            Arc::new(RollupConfig::default()),
            Arc::new(fetcher),
            latest_head_rx,
        )
    }

    #[tokio::test]
    async fn l1_state_query_returns_live_state() {
        let current_l1 = Some(MockFetcher::block_info(11));
        let executor = executor(MockFetcher::with_delay(Duration::ZERO), current_l1);
        let (sender, receiver) = oneshot::channel();

        executor.execute(L1WatcherQueries::L1State(sender)).await;

        let state = receiver.await.expect("state query should return a response");
        assert_eq!(state.current_l1, current_l1);
        assert_eq!(state.head_l1.map(|block| block.number), Some(10));
        assert_eq!(state.finalized_l1.map(|block| block.number), Some(9));
        assert_eq!(state.safe_l1.map(|block| block.number), Some(8));
    }

    #[tokio::test]
    async fn l1_state_query_fetches_blocks_concurrently() {
        let fetcher = MockFetcher::with_delay(Duration::from_millis(10));
        let max_calls = Arc::clone(&fetcher.max_calls);
        let executor = executor(fetcher, None);
        let (sender, receiver) = oneshot::channel();

        executor.execute(L1WatcherQueries::L1State(sender)).await;

        let _ = receiver.await.expect("state query should return a response");
        assert!(max_calls.load(Ordering::SeqCst) >= 3, "expected live block fetches to overlap");
    }

    #[tokio::test]
    async fn query_processor_handles_multiple_queries_concurrently() {
        let fetcher = MockFetcher::with_delay(Duration::from_millis(20));
        let (_latest_head_tx, latest_head_rx) = watch::channel(None);
        let (query_tx, query_rx) = mpsc::channel(16);
        let cancellation = CancellationToken::new();
        let processor = L1WatcherQueryProcessor::new(
            Arc::new(RollupConfig::default()),
            fetcher,
            query_rx,
            latest_head_rx,
            cancellation.clone(),
        )
        .with_query_concurrency(2);

        let processor_task = tokio::spawn(processor.start(()));
        let (sender_one, receiver_one) = oneshot::channel();
        let (sender_two, receiver_two) = oneshot::channel();
        let started_at = Instant::now();

        query_tx
            .send(L1WatcherQueries::L1State(sender_one))
            .await
            .expect("first query should be sent");
        query_tx
            .send(L1WatcherQueries::L1State(sender_two))
            .await
            .expect("second query should be sent");

        let _ = receiver_one.await.expect("first query should complete");
        let _ = receiver_two.await.expect("second query should complete");
        assert!(
            started_at.elapsed() < Duration::from_millis(500),
            "expected two queries to complete concurrently"
        );

        cancellation.cancel();
        processor_task
            .await
            .expect("query processor task should join")
            .expect("query processor should exit cleanly");
    }
}
