//! Integration tests verifying that L1 sync-status queries are served from L1 state maintained by
//! the [`L1WatcherActor`] background loop, without per-request L1 RPC calls.

use std::{sync::Arc, time::Duration};

use alloy_eips::BlockId;
use alloy_primitives::B256;
use alloy_rpc_types_eth::{Block, Filter, Log};
use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_consensus_node::{
    L1BlockFetcher, L1WatcherActor, L1WatcherQueryExecutor, L1WatcherQueryProcessor, NodeActor,
    QueuedL1WatcherDerivationClient,
};
use base_consensus_rpc::{L1State, L1WatcherQueries};
use base_protocol::BlockInfo;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

/// Upper bound for a sync-status query to be considered prompt. Far below the conductor's 10s
/// per-call deadline and the 15s default L1 RPC timeout.
const PROMPT_RESPONSE: Duration = Duration::from_secs(1);

/// An L1 provider whose block lookups never complete, like a stalled L1 proxy.
///
/// Log lookups either succeed with no logs or stall too, depending on `logs_stall`.
///
/// Hand-rolled rather than `mockall`: these integration tests need lookups that never resolve,
/// which a mock's synchronous `returning` closure cannot express, and the crate's `cfg(test)`
/// automocks are not compiled for integration tests.
struct StalledL1Fetcher {
    logs_stall: bool,
}

#[async_trait]
impl L1BlockFetcher for StalledL1Fetcher {
    type Error = String;

    async fn get_logs(&self, _: Filter) -> Result<Vec<Log>, Self::Error> {
        if self.logs_stall {
            std::future::pending::<()>().await;
        }
        Ok(vec![])
    }

    async fn get_block(&self, _: BlockId) -> Result<Option<Block>, Self::Error> {
        std::future::pending().await
    }
}

fn block_at(number: u64) -> BlockInfo {
    BlockInfo {
        hash: B256::from([number as u8; 32]),
        number,
        parent_hash: B256::from([number.saturating_sub(1) as u8; 32]),
        timestamp: number * 12,
    }
}

/// A running L1 watcher and query processor wired together as in the rollup node.
struct L1WatcherHarness {
    head_blocks: UnboundedSender<BlockInfo>,
    safe_blocks: UnboundedSender<BlockInfo>,
    finalized_blocks: UnboundedSender<BlockInfo>,
    head_l1: watch::Receiver<Option<BlockInfo>>,
    safe_l1: watch::Receiver<Option<BlockInfo>>,
    finalized_l1: watch::Receiver<Option<BlockInfo>>,
    queries: mpsc::Sender<L1WatcherQueries>,
    cancellation: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl L1WatcherHarness {
    fn start(fetcher: StalledL1Fetcher, derivation_origin: Option<BlockInfo>) -> Self {
        let (head_blocks, head_stream) = unbounded();
        let (safe_blocks, safe_stream) = unbounded();
        let (finalized_blocks, finalized_stream) = unbounded();
        let (head_tx, head_l1) = watch::channel(None);
        let (safe_tx, safe_l1) = watch::channel(None);
        let (finalized_tx, finalized_l1) = watch::channel(None);
        let (_derivation_origin_tx, derivation_origin_rx) = watch::channel(derivation_origin);
        let (derivation_tx, mut derivation_rx) = mpsc::channel(1024);
        let (queries, query_rx) = mpsc::channel(1024);
        let cancellation = CancellationToken::new();
        let rollup_config = Arc::new(RollupConfig::default());

        let processor = L1WatcherQueryProcessor::new(
            L1WatcherQueryExecutor::new(
                Arc::clone(&rollup_config),
                head_l1.clone(),
                safe_l1.clone(),
                finalized_l1.clone(),
                derivation_origin_rx,
            ),
            query_rx,
            cancellation.clone(),
        );
        let watcher = L1WatcherActor::new(
            rollup_config,
            fetcher,
            head_tx,
            safe_tx,
            finalized_tx,
            QueuedL1WatcherDerivationClient::new(derivation_tx),
            None,
            cancellation.clone(),
            head_stream,
            safe_stream,
            finalized_stream,
            0,
            Default::default(),
        );

        let tasks = vec![
            tokio::spawn(async move {
                let _ = processor.start(()).await;
            }),
            tokio::spawn(async move {
                let _ = watcher.start(()).await;
            }),
            tokio::spawn(async move { while derivation_rx.recv().await.is_some() {} }),
        ];

        Self {
            head_blocks,
            safe_blocks,
            finalized_blocks,
            head_l1,
            safe_l1,
            finalized_l1,
            queries,
            cancellation,
            tasks,
        }
    }

    /// Feeds new L1 blocks to the watcher's polling streams, one at a time, and waits until the
    /// watcher publishes each of them.
    ///
    /// The head goes last: with a stalled provider the watcher may never return from processing
    /// a head.
    async fn observe(&mut self, head: BlockInfo, safe: BlockInfo, finalized: BlockInfo) {
        for (stream, receiver, block) in [
            (&self.safe_blocks, &mut self.safe_l1, safe),
            (&self.finalized_blocks, &mut self.finalized_l1, finalized),
            (&self.head_blocks, &mut self.head_l1, head),
        ] {
            stream.unbounded_send(block).expect("block stream should be open");
            timeout(PROMPT_RESPONSE, receiver.wait_for(|observed| *observed == Some(block)))
                .await
                .expect("watcher should observe the new block")
                .expect("watcher should be running");
        }
    }

    /// Queries the L1 state, failing unless the response arrives promptly.
    async fn l1_state(&self) -> L1State {
        let (sender, receiver) = oneshot::channel();
        self.queries.send(L1WatcherQueries::L1State(sender)).await.expect("query should be sent");
        timeout(PROMPT_RESPONSE, receiver)
            .await
            .expect("L1 state query should not wait on the stalled L1 provider")
            .expect("L1 state query should return a response")
    }
}

impl Drop for L1WatcherHarness {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.tasks.iter().for_each(JoinHandle::abort);
    }
}

fn assert_l1_state(
    state: &L1State,
    current: Option<BlockInfo>,
    head: BlockInfo,
    safe: BlockInfo,
    finalized: BlockInfo,
) {
    assert_eq!(state.current_l1, current);
    assert_eq!(state.head_l1, Some(head));
    assert_eq!(state.safe_l1, Some(safe));
    assert_eq!(state.finalized_l1, Some(finalized));
    assert_eq!(state.current_l1_finalized, Some(finalized));
}

#[tokio::test]
async fn l1_state_is_served_from_watched_blocks_while_l1_block_lookups_stall() {
    let current_l1 = Some(block_at(95));
    let mut harness = L1WatcherHarness::start(StalledL1Fetcher { logs_stall: false }, current_l1);

    let unknown = harness.l1_state().await;
    assert_eq!(unknown.current_l1, current_l1);
    assert_eq!(unknown.head_l1, None);
    assert_eq!(unknown.safe_l1, None);
    assert_eq!(unknown.finalized_l1, None);

    harness.observe(block_at(100), block_at(68), block_at(36)).await;
    assert_l1_state(
        &harness.l1_state().await,
        current_l1,
        block_at(100),
        block_at(68),
        block_at(36),
    );

    harness.observe(block_at(132), block_at(100), block_at(68)).await;
    assert_l1_state(
        &harness.l1_state().await,
        current_l1,
        block_at(132),
        block_at(100),
        block_at(68),
    );
}

#[tokio::test]
async fn l1_state_returns_last_known_blocks_while_l1_provider_is_fully_stalled() {
    let mut harness = L1WatcherHarness::start(StalledL1Fetcher { logs_stall: true }, None);

    // The watcher publishes the head and then stalls fetching its system config logs.
    harness.observe(block_at(100), block_at(68), block_at(36)).await;

    for _ in 0..3 {
        assert_l1_state(&harness.l1_state().await, None, block_at(100), block_at(68), block_at(36));
    }
}
