//! [`NodeActor`] implementation for an L1 chain watcher that polls for L1 block updates over HTTP
//! RPC.

use std::{sync::Arc, time::Duration};

use alloy_eips::BlockId;
use alloy_primitives::Address;
use alloy_rpc_types_eth::{Filter, Log};
use async_trait::async_trait;
use base_consensus_genesis::{
    RollupConfig, SystemConfigLog, SystemConfigUpdate, UnsafeBlockSignerUpdate,
};
use base_protocol::BlockInfo;
use futures::{Stream, StreamExt};
use tokio::{
    select,
    sync::{
        mpsc::{self},
        watch,
    },
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

use super::{L1BlockFetcher, L1WatcherDerivationClient};
use crate::{
    Metrics, NodeActor,
    actors::{CancellableContext, l1_watcher::error::L1WatcherActorError},
};

/// Stateless helper that wraps the log-fetch retry loop for [`L1WatcherActor`].
///
/// Extracted as a separate type so the retry logic can be unit-tested independently of the actor's
/// full channel and stream setup.
#[derive(Debug)]
pub struct LogRetrier;

impl LogRetrier {
    /// Fetch logs matching `filter` with capped exponential backoff, retrying up to 10 times.
    ///
    /// Returns `Ok(Some(logs))` on success, `Ok(None)` if `cancel` fires during a backoff sleep,
    /// or `Err(`[`L1WatcherActorError::RetriesExhausted`]`)` once all attempts fail.
    ///
    /// `initial_backoff` and `max_backoff` are caller-supplied so tests can use tiny values.
    pub async fn fetch_logs_with_retry<F>(
        provider: &F,
        filter: Filter,
        cancel: &CancellationToken,
        block_info: BlockInfo,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Result<Option<Vec<Log>>, L1WatcherActorError<BlockInfo>>
    where
        F: L1BlockFetcher,
    {
        const MAX_RETRIES: u32 = 10;

        let mut backoff = initial_backoff;
        for attempt in 1..=MAX_RETRIES {
            match provider.get_logs(filter.clone()).await {
                Ok(logs) => return Ok(Some(logs)),
                Err(e) => {
                    warn!(
                        target: "l1_watcher",
                        error = %e,
                        block_hash = %block_info.hash,
                        block_number = block_info.number,
                        attempt,
                        "Failed to fetch logs for L1 head"
                    );
                    if attempt < MAX_RETRIES {
                        select! {
                            _ = cancel.cancelled() => return Ok(None),
                            _ = tokio::time::sleep(backoff) => {}
                        }
                        backoff = (backoff * 2).min(max_backoff);
                    }
                }
            }
        }

        error!(
            target: "l1_watcher",
            block_hash = %block_info.hash,
            block_number = block_info.number,
            "Exhausted retries fetching logs for L1 head"
        );
        Err(L1WatcherActorError::RetriesExhausted)
    }
}

/// An L1 chain watcher that checks for L1 block updates over RPC.
#[derive(Debug)]
pub struct L1WatcherActor<BlockStream, L1Provider, L1WatcherDerivationClient_>
where
    BlockStream: Stream<Item = BlockInfo> + Unpin + Send,
    L1Provider: L1BlockFetcher,
    L1WatcherDerivationClient_: L1WatcherDerivationClient,
{
    /// The [`RollupConfig`] to tell if ecotone is active.
    /// This is used to determine if the L1 watcher should check for unsafe block signer updates.
    rollup_config: Arc<RollupConfig>,
    /// The L1 provider.
    l1_provider: L1Provider,
    /// The latest L1 head block.
    latest_head: watch::Sender<Option<BlockInfo>>,
    /// Client used to interact with the [`crate::DerivationActor`].
    derivation_client: L1WatcherDerivationClient_,
    /// The block signer sender.
    block_signer_sender: Option<mpsc::Sender<Address>>,
    /// The cancellation token, shared between all tasks.
    cancellation: CancellationToken,
    /// A stream over the latest head.
    head_stream: BlockStream,
    /// A stream over the finalized block accepted as canonical.
    finalized_stream: BlockStream,
    /// Number of L1 blocks to keep distance from the L1 head for the derivation pipeline.
    /// When non-zero, derivation receives the block at `head - verifier_l1_confs` rather than
    /// the real head. The sequencer's watch channel always receives the real head.
    verifier_l1_confs: u64,
}
impl<BlockStream, L1Provider, L1WatcherDerivationClient_>
    L1WatcherActor<BlockStream, L1Provider, L1WatcherDerivationClient_>
where
    BlockStream: Stream<Item = BlockInfo> + Unpin + Send,
    L1Provider: L1BlockFetcher,
    L1WatcherDerivationClient_: L1WatcherDerivationClient,
{
    /// Instantiate a new [`L1WatcherActor`].
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        rollup_config: Arc<RollupConfig>,
        l1_provider: L1Provider,
        l1_head_updates_tx: watch::Sender<Option<BlockInfo>>,
        derivation_client: L1WatcherDerivationClient_,
        signer: Option<mpsc::Sender<Address>>,
        cancellation: CancellationToken,
        head_stream: BlockStream,
        finalized_stream: BlockStream,
        verifier_l1_confs: u64,
    ) -> Self {
        Self {
            rollup_config,
            l1_provider,
            latest_head: l1_head_updates_tx,
            derivation_client,
            block_signer_sender: signer,
            cancellation,
            head_stream,
            finalized_stream,
            verifier_l1_confs,
        }
    }
}

#[async_trait]
impl<BlockStream, L1Provider, L1WatcherDerivationClient_> NodeActor
    for L1WatcherActor<BlockStream, L1Provider, L1WatcherDerivationClient_>
where
    BlockStream: Stream<Item = BlockInfo> + Unpin + Send + 'static,
    L1Provider: L1BlockFetcher + 'static,
    L1WatcherDerivationClient_: L1WatcherDerivationClient + 'static,
{
    type Error = L1WatcherActorError<BlockInfo>;
    type StartData = ();

    /// Start the main processing loop.
    async fn start(mut self, _: Self::StartData) -> Result<(), Self::Error> {
        const INITIAL_BACKOFF: Duration = Duration::from_millis(50);
        const MAX_BACKOFF: Duration = Duration::from_millis(500);

        Metrics::l1_verifier_confs_depth().set(self.verifier_l1_confs as f64);
        if self.verifier_l1_confs > 0 {
            info!(
                target: "l1_watcher",
                verifier_l1_confs = self.verifier_l1_confs,
                "Verifier L1 confirmation delay enabled"
            );
        }

        let cancel = self.cancellation.clone();

        loop {
            select! {
                _ = cancel.cancelled() => {
                    // Exit the task on cancellation.
                    info!(
                        target: "l1_watcher",
                        "Received shutdown signal. Exiting L1 watcher task."
                    );

                    return Ok(());
                },
                new_head = self.head_stream.next() => match new_head {
                    None => {
                        return Err(L1WatcherActorError::StreamEnded);
                    }
                    Some(head_block_info) => {
                        // Always broadcast the real head so the sequencer's
                        // DelayedL1OriginSelectorProvider can compute its own offset.
                        self.latest_head.send_replace(Some(head_block_info));

                        // Apply verifier confirmation delay: derive from
                        // `head - verifier_l1_confs` when the chain is deep enough.
                        let derivation_block = if self.verifier_l1_confs > 0
                            && head_block_info.number >= self.verifier_l1_confs
                        {
                            let target = head_block_info.number - self.verifier_l1_confs;
                            match self.l1_provider.get_block(BlockId::Number(target.into())).await {
                                Ok(Some(block)) => block.into_consensus().into(),
                                Ok(None) => {
                                    Metrics::l1_verifier_delayed_fetch_errors().increment(1);
                                    warn!(
                                        target: "l1_watcher",
                                        head = head_block_info.number,
                                        target,
                                        "Delayed L1 block not found; skipping head update"
                                    );
                                    continue;
                                }
                                Err(e) => {
                                    Metrics::l1_verifier_delayed_fetch_errors().increment(1);
                                    warn!(
                                        target: "l1_watcher",
                                        error = %e,
                                        head = head_block_info.number,
                                        target,
                                        "Failed to fetch delayed L1 block; skipping head update"
                                    );
                                    continue;
                                }
                            }
                        } else {
                            head_block_info
                        };

                        Metrics::l1_verifier_derivation_head()
                            .absolute(derivation_block.number);
                        self.derivation_client.send_new_l1_head(derivation_block).await.map_err(|e| {
                            warn!(target: "l1_watcher", error = %e, "Error sending l1 head update to derivation actor");
                            L1WatcherActorError::DerivationClientError(e)
                        })?;

                        // For each log, attempt to construct a [`SystemConfigLog`].
                        // Build the [`SystemConfigUpdate`] from the log.
                        // If the update is an Unsafe block signer update, send the address
                        // to the block signer sender.
                        let filter_address = self.rollup_config.l1_system_config_address;
                        let filter = Filter::new()
                            .address(filter_address)
                            .select(derivation_block.hash);

                        let Some(logs) = LogRetrier::fetch_logs_with_retry(
                            &self.l1_provider,
                            filter,
                            &cancel,
                            derivation_block,
                            INITIAL_BACKOFF,
                            MAX_BACKOFF,
                        )
                        .await?
                        else {
                            return Ok(());
                        };
                        let ecotone_active =
                            self.rollup_config.is_ecotone_active(derivation_block.timestamp);
                        for log in logs {
                            let sys_cfg_log = SystemConfigLog::new(log.into(), ecotone_active);
                            if let Ok(SystemConfigUpdate::UnsafeBlockSigner(UnsafeBlockSignerUpdate { unsafe_block_signer })) = sys_cfg_log.build() {
                                info!(
                                    target: "l1_watcher",
                                    "Unsafe block signer update: {unsafe_block_signer}"
                                );
                                if let Some(ref block_signer_sender) = self.block_signer_sender && let Err(e) = block_signer_sender.send(unsafe_block_signer).await {
                                    error!(
                                        target: "l1_watcher",
                                        "Error sending unsafe block signer update: {e}"
                                    );
                                }
                            }
                        }
                    },
                },
                new_finalized = self.finalized_stream.next() => match new_finalized {
                    None => {
                        return Err(L1WatcherActorError::StreamEnded);
                    }
                    Some(finalized_block_info) => {
                        self.derivation_client.send_finalized_l1_block(finalized_block_info).await.map_err(|e| {
                            warn!(target: "l1_watcher", error = %e, "Error sending finalized l1 block update to derivation actor");
                            L1WatcherActorError::DerivationClientError(e)
                        })?;
                    }
                }
            }
        }
    }
}

impl<BlockStream, L1Provider, L1WatcherDerivationClient_> CancellableContext
    for L1WatcherActor<BlockStream, L1Provider, L1WatcherDerivationClient_>
where
    BlockStream: Stream<Item = BlockInfo> + Unpin + Send + 'static,
    L1Provider: L1BlockFetcher,
    L1WatcherDerivationClient_: L1WatcherDerivationClient + 'static,
{
    fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.cancellation.cancelled()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU32, Ordering},
        },
    };

    use alloy_eips::BlockId;
    use alloy_primitives::B256;
    use alloy_rpc_types_eth::{Block, Filter, Log};
    use async_trait::async_trait;
    use base_consensus_genesis::RollupConfig;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::DerivationClientResult;

    type BoxedBlockStream = Pin<Box<dyn Stream<Item = BlockInfo> + Unpin + Send>>;

    // ---------------------------------------------------------------------------
    // Mock L1BlockFetcher used by LogRetrier tests (unchanged)
    // ---------------------------------------------------------------------------

    struct MockFetcher {
        call_count: Arc<AtomicU32>,
        fail_count: u32,
    }

    impl MockFetcher {
        fn always_fail() -> Self {
            Self { call_count: Arc::new(AtomicU32::new(0)), fail_count: u32::MAX }
        }

        fn fail_times(n: u32) -> Self {
            Self { call_count: Arc::new(AtomicU32::new(0)), fail_count: n }
        }

        fn always_succeed() -> Self {
            Self::fail_times(0)
        }
    }

    #[async_trait]
    impl L1BlockFetcher for MockFetcher {
        type Error = String;

        async fn get_logs(&self, _: Filter) -> Result<Vec<Log>, Self::Error> {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            if count < self.fail_count { Err("transient error".to_string()) } else { Ok(vec![]) }
        }

        async fn get_block(&self, _: BlockId) -> Result<Option<Block>, Self::Error> {
            Ok(None)
        }
    }

    fn dummy_block() -> BlockInfo {
        BlockInfo { hash: B256::ZERO, number: 0, parent_hash: B256::ZERO, timestamp: 0 }
    }

    // ---------------------------------------------------------------------------
    // Configurable fetcher for verifier L1 confs tests
    // ---------------------------------------------------------------------------

    /// Response behaviour for [`ConfigurableFetcher::get_block`].
    enum GetBlockBehavior {
        /// Always return a default [`Block`].
        Default,
        /// Always return `None`.
        None,
        /// Always return an error.
        Err,
    }

    struct ConfigurableFetcher {
        get_block_behavior: GetBlockBehavior,
        get_block_requested_ids: Arc<Mutex<Vec<BlockId>>>,
    }

    impl ConfigurableFetcher {
        fn returning_default_block() -> Self {
            Self {
                get_block_behavior: GetBlockBehavior::Default,
                get_block_requested_ids: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn returning_none() -> Self {
            Self {
                get_block_behavior: GetBlockBehavior::None,
                get_block_requested_ids: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn returning_error() -> Self {
            Self {
                get_block_behavior: GetBlockBehavior::Err,
                get_block_requested_ids: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl L1BlockFetcher for ConfigurableFetcher {
        type Error = String;

        async fn get_logs(&self, _: Filter) -> Result<Vec<Log>, Self::Error> {
            Ok(vec![])
        }

        async fn get_block(&self, id: BlockId) -> Result<Option<Block>, Self::Error> {
            self.get_block_requested_ids.lock().unwrap().push(id);
            match self.get_block_behavior {
                GetBlockBehavior::Default => Ok(Some(Block::default())),
                GetBlockBehavior::None => Ok(None),
                GetBlockBehavior::Err => Err("provider error".to_string()),
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Mock derivation client that captures sent blocks
    // ---------------------------------------------------------------------------

    #[derive(Debug, Clone, Default)]
    struct RecordingDerivationClient {
        heads: Arc<Mutex<Vec<BlockInfo>>>,
        finalized: Arc<Mutex<Vec<BlockInfo>>>,
    }

    #[async_trait]
    impl L1WatcherDerivationClient for RecordingDerivationClient {
        async fn send_finalized_l1_block(&self, block: BlockInfo) -> DerivationClientResult<()> {
            self.finalized.lock().unwrap().push(block);
            Ok(())
        }

        async fn send_new_l1_head(&self, block: BlockInfo) -> DerivationClientResult<()> {
            self.heads.lock().unwrap().push(block);
            Ok(())
        }
    }

    impl RecordingDerivationClient {
        fn sent_heads(&self) -> Vec<BlockInfo> {
            self.heads.lock().unwrap().clone()
        }
    }

    // ---------------------------------------------------------------------------
    // Actor test helpers
    // ---------------------------------------------------------------------------

    fn block_at(number: u64) -> BlockInfo {
        BlockInfo {
            hash: B256::from([number as u8; 32]),
            number,
            parent_hash: B256::ZERO,
            timestamp: number * 12,
        }
    }

    /// Build and run an [`L1WatcherActor`] to completion (stream ends → `StreamEnded`).
    ///
    /// Returns the derivation client for assertion and the watch receiver for the raw head.
    async fn run_actor<F: L1BlockFetcher>(
        fetcher: F,
        head_blocks: Vec<BlockInfo>,
        verifier_l1_confs: u64,
    ) -> (RecordingDerivationClient, watch::Receiver<Option<BlockInfo>>) {
        let derivation_client = RecordingDerivationClient::default();
        let (l1_head_tx, l1_head_rx) = watch::channel(None);
        let cancel = CancellationToken::new();

        let head_stream: BoxedBlockStream = Box::pin(futures::stream::iter(head_blocks));
        // Finalized stream that never yields — actor will exit via head stream ending.
        let finalized_stream: BoxedBlockStream = Box::pin(futures::stream::pending());

        let actor = L1WatcherActor::new(
            Arc::new(RollupConfig::default()),
            fetcher,
            l1_head_tx,
            derivation_client.clone(),
            None,
            cancel,
            head_stream,
            finalized_stream,
            verifier_l1_confs,
        );

        // The actor loop will process all head_stream items then return StreamEnded.
        let _ = actor.start(()).await;

        (derivation_client, l1_head_rx)
    }

    // ---------------------------------------------------------------------------
    // LogRetrier tests (unchanged)
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn fetch_logs_succeeds_on_first_try() {
        let cancel = CancellationToken::new();
        let result = LogRetrier::fetch_logs_with_retry(
            &MockFetcher::always_succeed(),
            Filter::new(),
            &cancel,
            dummy_block(),
            Duration::from_nanos(1),
            Duration::from_nanos(1),
        )
        .await;
        assert!(matches!(result, Ok(Some(_))));
    }

    #[tokio::test]
    async fn fetch_logs_retries_and_eventually_succeeds() {
        let cancel = CancellationToken::new();
        let result = LogRetrier::fetch_logs_with_retry(
            &MockFetcher::fail_times(3),
            Filter::new(),
            &cancel,
            dummy_block(),
            Duration::from_nanos(1),
            Duration::from_nanos(1),
        )
        .await;
        assert!(matches!(result, Ok(Some(_))));
    }

    #[tokio::test]
    async fn fetch_logs_exhausted_retries_returns_error() {
        let cancel = CancellationToken::new();
        let result = LogRetrier::fetch_logs_with_retry(
            &MockFetcher::always_fail(),
            Filter::new(),
            &cancel,
            dummy_block(),
            Duration::from_nanos(1),
            Duration::from_nanos(1),
        )
        .await;
        assert!(matches!(result, Err(L1WatcherActorError::RetriesExhausted)));
    }

    #[tokio::test]
    async fn fetch_logs_cancelled_during_backoff_returns_none() {
        let cancel = CancellationToken::new();
        // Pre-cancel so the very first backoff sleep resolves to the cancel arm.
        cancel.cancel();
        let result = LogRetrier::fetch_logs_with_retry(
            &MockFetcher::always_fail(),
            Filter::new(),
            &cancel,
            dummy_block(),
            Duration::from_secs(10), // long sleep ensures only the cancel arm fires
            Duration::from_secs(10),
        )
        .await;
        assert!(matches!(result, Ok(None)));
    }

    // ---------------------------------------------------------------------------
    // Verifier L1 confs tests
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn zero_confs_forwards_real_head_to_derivation() {
        let head = block_at(100);
        let (client, rx) =
            run_actor(ConfigurableFetcher::returning_default_block(), vec![head], 0).await;

        // Derivation receives the real head block.
        let heads = client.sent_heads();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].number, 100);

        // Watch channel also has the real head.
        assert_eq!(rx.borrow().unwrap().number, 100);
    }

    #[tokio::test]
    async fn confs_delay_fetches_earlier_block_for_derivation() {
        let fetcher = ConfigurableFetcher::returning_default_block();
        let head = block_at(100);
        let (client, rx) = run_actor(fetcher, vec![head], 4).await;

        // Derivation receives the *delayed* block (default Block → BlockInfo with number 0).
        let heads = client.sent_heads();
        assert_eq!(heads.len(), 1);
        // The fetcher returns Block::default() which converts to BlockInfo with number 0.
        assert_eq!(heads[0].number, 0);

        // Watch channel still has the real head.
        assert_eq!(rx.borrow().unwrap().number, 100);
    }

    #[tokio::test]
    async fn confs_delay_requests_correct_block_number() {
        let fetcher = ConfigurableFetcher::returning_default_block();
        let ids = Arc::clone(&fetcher.get_block_requested_ids);
        let head = block_at(20);
        let _ = run_actor(fetcher, vec![head], 4).await;

        let requested = ids.lock().unwrap().clone();
        assert_eq!(requested.len(), 1);
        assert_eq!(requested[0], BlockId::Number(16u64.into()));
    }

    #[tokio::test]
    async fn confs_delay_multiple_heads_each_fetched() {
        let fetcher = ConfigurableFetcher::returning_default_block();
        let ids = Arc::clone(&fetcher.get_block_requested_ids);
        let _ = run_actor(fetcher, vec![block_at(10), block_at(14), block_at(20)], 4).await;

        let requested: Vec<_> = ids.lock().unwrap().iter().map(|id| format!("{id:?}")).collect();
        assert_eq!(requested.len(), 3);
    }

    #[tokio::test]
    async fn confs_shallow_chain_forwards_real_head() {
        // Head number (2) < verifier_l1_confs (4), so no delay is applied.
        let fetcher = ConfigurableFetcher::returning_default_block();
        let ids = Arc::clone(&fetcher.get_block_requested_ids);
        let (client, _rx) = run_actor(fetcher, vec![block_at(2)], 4).await;

        // No get_block call should have been made.
        assert!(ids.lock().unwrap().is_empty());

        // Derivation receives the real head.
        let heads = client.sent_heads();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].number, 2);
    }

    #[tokio::test]
    async fn confs_delayed_block_not_found_skips_derivation() {
        let (client, rx) =
            run_actor(ConfigurableFetcher::returning_none(), vec![block_at(100)], 4).await;

        // Derivation should NOT receive any head — the block was not found.
        assert!(client.sent_heads().is_empty());

        // Watch channel still has the real head.
        assert_eq!(rx.borrow().unwrap().number, 100);
    }

    #[tokio::test]
    async fn confs_delayed_block_fetch_error_skips_derivation() {
        let (client, rx) =
            run_actor(ConfigurableFetcher::returning_error(), vec![block_at(100)], 4).await;

        // Derivation should NOT receive any head — the fetch errored.
        assert!(client.sent_heads().is_empty());

        // Watch channel still has the real head.
        assert_eq!(rx.borrow().unwrap().number, 100);
    }

    #[tokio::test]
    async fn confs_mixed_shallow_and_deep_heads() {
        // First head is too shallow (3 < 4), second is deep enough (10 >= 4).
        let fetcher = ConfigurableFetcher::returning_default_block();
        let ids = Arc::clone(&fetcher.get_block_requested_ids);
        let (client, _rx) = run_actor(fetcher, vec![block_at(3), block_at(10)], 4).await;

        let heads = client.sent_heads();
        // Two derivation sends: block 3 forwarded directly, block 10 fetched as delayed.
        assert_eq!(heads.len(), 2);
        assert_eq!(heads[0].number, 3);
        // Block::default() converts to number 0.
        assert_eq!(heads[1].number, 0);

        // Only the second head triggered a get_block call.
        assert_eq!(ids.lock().unwrap().len(), 1);
        assert_eq!(ids.lock().unwrap()[0], BlockId::Number(6u64.into()));
    }
}
