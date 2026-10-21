//! Query processor for L1 watcher RPC requests.

use std::sync::Arc;

use async_trait::async_trait;
use base_common_genesis::RollupConfig;
use base_consensus_rpc::{L1State, L1WatcherQueries};
use base_protocol::BlockInfo;
use tokio::{
    select,
    sync::{mpsc, watch},
};
use tokio_util::sync::CancellationToken;

use crate::{NodeActor, actors::l1_watcher::error::L1WatcherActorError};

/// Answers L1 watcher queries from L1 block refs maintained by background loops.
///
/// Queries never perform L1 network I/O: the [`super::L1WatcherActor`] polls the latest, safe,
/// and finalized L1 blocks and publishes them on watch channels, and the derivation actor
/// publishes its L1 origin. A query only reads the most recent value of each channel, so it
/// returns immediately with the last known L1 state even while the L1 RPC is unavailable.
#[derive(Debug)]
pub struct L1WatcherQueryExecutor {
    /// The rollup configuration to return for config queries.
    rollup_config: Arc<RollupConfig>,
    /// Receiver for the latest L1 head block.
    head_l1: watch::Receiver<Option<BlockInfo>>,
    /// Receiver for the latest L1 safe block.
    safe_l1: watch::Receiver<Option<BlockInfo>>,
    /// Receiver for the latest L1 finalized block.
    finalized_l1: watch::Receiver<Option<BlockInfo>>,
    /// Receiver for the most recent L1 origin reached by derivation.
    derivation_origin: watch::Receiver<Option<BlockInfo>>,
}

impl L1WatcherQueryExecutor {
    /// Creates a new query executor.
    pub const fn new(
        rollup_config: Arc<RollupConfig>,
        head_l1: watch::Receiver<Option<BlockInfo>>,
        safe_l1: watch::Receiver<Option<BlockInfo>>,
        finalized_l1: watch::Receiver<Option<BlockInfo>>,
        derivation_origin: watch::Receiver<Option<BlockInfo>>,
    ) -> Self {
        Self { rollup_config, head_l1, safe_l1, finalized_l1, derivation_origin }
    }

    /// Executes a single query.
    pub fn execute(&self, query: L1WatcherQueries) {
        let sent = match query {
            L1WatcherQueries::Config(sender) => {
                sender.send(self.rollup_config.with_runtime_upgrade_overrides()).is_ok()
            }
            L1WatcherQueries::L1State(sender) => sender.send(self.l1_state()).is_ok(),
        };
        if !sent {
            warn!(target: "l1_watcher", "L1 watcher query requester dropped before the response was sent");
        }
    }

    /// Returns the last known L1 state.
    pub fn l1_state(&self) -> L1State {
        let finalized_l1 = *self.finalized_l1.borrow();
        L1State {
            current_l1: *self.derivation_origin.borrow(),
            current_l1_finalized: finalized_l1,
            head_l1: *self.head_l1.borrow(),
            safe_l1: *self.safe_l1.borrow(),
            finalized_l1,
        }
    }
}

/// Actor that answers L1 watcher RPC queries.
#[derive(Debug)]
pub struct L1WatcherQueryProcessor {
    /// Executor used for the per-request query logic.
    executor: L1WatcherQueryExecutor,
    /// Receiver for inbound L1 watcher queries.
    inbound_queries: mpsc::Receiver<L1WatcherQueries>,
    /// Shared cancellation token.
    cancellation: CancellationToken,
}

impl L1WatcherQueryProcessor {
    /// Creates a new query processor.
    pub const fn new(
        executor: L1WatcherQueryExecutor,
        inbound_queries: mpsc::Receiver<L1WatcherQueries>,
        cancellation: CancellationToken,
    ) -> Self {
        Self { executor, inbound_queries, cancellation }
    }
}

#[async_trait]
impl NodeActor for L1WatcherQueryProcessor {
    type Error = L1WatcherActorError<BlockInfo>;
    type StartData = ();

    async fn start(mut self, _: Self::StartData) -> Result<(), Self::Error> {
        loop {
            select! {
                _ = self.cancellation.cancelled() => {
                    info!(target: "l1_watcher", "Received shutdown signal. Exiting L1 watcher query processor.");
                    return Ok(());
                }
                query = self.inbound_queries.recv() => match query {
                    Some(query) => self.executor.execute(query),
                    None if self.cancellation.is_cancelled() => {
                        info!(target: "l1_watcher", "L1 watcher query processor cancelled after query stream completion.");
                        return Ok(());
                    }
                    None => {
                        error!(target: "l1_watcher", "L1 watcher query channel closed unexpectedly, exiting query processor task.");
                        return Err(L1WatcherActorError::StreamEnded);
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use tokio::sync::oneshot;

    use super::*;

    fn block_info(number: u64) -> BlockInfo {
        BlockInfo {
            hash: B256::from([number as u8; 32]),
            number,
            parent_hash: B256::from([number.saturating_sub(1) as u8; 32]),
            timestamp: number * 10,
        }
    }

    #[tokio::test]
    async fn query_processor_answers_with_latest_published_l1_refs() {
        let (head_tx, head_rx) = watch::channel(None);
        let (safe_tx, safe_rx) = watch::channel(None);
        let (finalized_tx, finalized_rx) = watch::channel(None);
        let (origin_tx, origin_rx) = watch::channel(None);
        let (query_tx, query_rx) = mpsc::channel(16);
        let cancellation = CancellationToken::new();
        let processor = L1WatcherQueryProcessor::new(
            L1WatcherQueryExecutor::new(
                Arc::new(RollupConfig::default()),
                head_rx,
                safe_rx,
                finalized_rx,
                origin_rx,
            ),
            query_rx,
            cancellation.clone(),
        );
        let processor_task = tokio::spawn(processor.start(()));

        let query = async || {
            let (sender, receiver) = oneshot::channel();
            query_tx.send(L1WatcherQueries::L1State(sender)).await.expect("query should be sent");
            receiver.await.expect("state query should return a response")
        };

        let unknown = query().await;
        assert_eq!(unknown.current_l1, None);
        assert_eq!(unknown.head_l1, None);
        assert_eq!(unknown.safe_l1, None);
        assert_eq!(unknown.finalized_l1, None);
        assert_eq!(unknown.current_l1_finalized, None);

        origin_tx.send_replace(Some(block_info(7)));
        head_tx.send_replace(Some(block_info(10)));
        safe_tx.send_replace(Some(block_info(9)));
        finalized_tx.send_replace(Some(block_info(8)));

        let known = query().await;
        assert_eq!(known.current_l1, Some(block_info(7)));
        assert_eq!(known.head_l1, Some(block_info(10)));
        assert_eq!(known.safe_l1, Some(block_info(9)));
        assert_eq!(known.finalized_l1, Some(block_info(8)));
        assert_eq!(known.current_l1_finalized, Some(block_info(8)));

        cancellation.cancel();
        processor_task
            .await
            .expect("query processor task should join")
            .expect("query processor should exit cleanly");
    }

    #[tokio::test]
    async fn query_processor_errors_when_query_channel_closes_unexpectedly() {
        let (_head_tx, head_rx) = watch::channel(None);
        let (query_tx, query_rx) = mpsc::channel(1);
        let processor = L1WatcherQueryProcessor::new(
            L1WatcherQueryExecutor::new(
                Arc::new(RollupConfig::default()),
                head_rx.clone(),
                head_rx.clone(),
                head_rx.clone(),
                head_rx,
            ),
            query_rx,
            CancellationToken::new(),
        );
        drop(query_tx);

        let result = processor.start(()).await;
        assert!(matches!(result, Err(L1WatcherActorError::StreamEnded)));
    }
}
