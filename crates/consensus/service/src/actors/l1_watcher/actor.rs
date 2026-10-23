//! [`NodeActor`] implementation for an L1 chain watcher that polls for L1 block updates over HTTP
//! RPC.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use alloy_eips::BlockId;
use alloy_primitives::Address;
use alloy_rpc_types_eth::{Filter, Log};
use async_trait::async_trait;
use base_common_genesis::{
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
    time::{Instant, sleep_until},
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
    /// Total wall-clock budget for fetching the logs of one L1 head, across all attempts and
    /// backoff sleeps.
    ///
    /// Exhausting the budget fails the actor, which shuts the node down. That restart is the
    /// current recovery path when the fetch is pinned to a block hash that L1 has reorged away,
    /// and it reloads the unsafe block signer from the `SystemConfig` contract after rotations
    /// the watcher missed while it was stalled here. The budget therefore has to outlast
    /// typical L1 proxy outages (30-60s observed) without restarting every replica that shares
    /// the proxy. It is a wall-clock bound rather than an attempt count so that outage
    /// tolerance does not shrink when the per-request L1 timeout is lowered: 150s matches the
    /// previous worst case of ten attempts at a 15s request timeout.
    pub const RETRY_BUDGET: Duration = Duration::from_secs(150);

    /// Fetch logs matching `filter`, retrying with capped exponential backoff until a request
    /// succeeds, `cancel` fires, or `retry_budget` elapses.
    ///
    /// Returns `Ok(Some(logs))` on success, `Ok(None)` once `cancel` fires (during a backoff
    /// sleep or while a request is in flight), or
    /// `Err(`[`L1WatcherActorError::RetriesExhausted`]`)` once `retry_budget` elapses, including
    /// while a request is still in flight.
    ///
    /// The backoff and budget durations are caller-supplied so tests can use tiny values.
    pub async fn fetch_logs_with_retry<F>(
        provider: &F,
        filter: Filter,
        cancel: &CancellationToken,
        block_info: BlockInfo,
        initial_backoff: Duration,
        max_backoff: Duration,
        retry_budget: Duration,
    ) -> Result<Option<Vec<Log>>, L1WatcherActorError<BlockInfo>>
    where
        F: L1BlockFetcher,
    {
        let deadline = Instant::now() + retry_budget;
        let mut backoff = initial_backoff;
        let mut attempt: u64 = 0;
        while Instant::now() < deadline {
            attempt += 1;
            let result = select! {
                _ = cancel.cancelled() => return Ok(None),
                _ = sleep_until(deadline) => break,
                result = provider.get_logs(filter.clone()) => result,
            };
            match result {
                Ok(logs) => return Ok(Some(logs)),
                Err(e) => warn!(
                    target: "l1_watcher",
                    error = %e,
                    block_hash = %block_info.hash,
                    block_number = block_info.number,
                    attempt,
                    backoff_ms = backoff.as_millis() as u64,
                    "Failed to fetch logs for L1 head; retrying"
                ),
            }
            select! {
                _ = cancel.cancelled() => return Ok(None),
                _ = sleep_until(deadline.min(Instant::now() + backoff)) => {}
            }
            backoff = (backoff * 2).min(max_backoff);
        }

        error!(
            target: "l1_watcher",
            block_hash = %block_info.hash,
            block_number = block_info.number,
            attempts = attempt,
            retry_budget_ms = retry_budget.as_millis() as u64,
            "Exhausted retry budget fetching logs for L1 head"
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
    /// The latest L1 safe block.
    safe_head: watch::Sender<Option<BlockInfo>>,
    /// The latest L1 finalized block.
    finalized_head: watch::Sender<Option<BlockInfo>>,
    /// Client used to interact with the [`crate::DerivationActor`].
    derivation_client: L1WatcherDerivationClient_,
    /// The block signer sender.
    block_signer_sender: Option<mpsc::Sender<Address>>,
    /// The cancellation token, shared between all tasks.
    cancellation: CancellationToken,
    /// A stream over the latest head.
    head_stream: BlockStream,
    /// A stream over the latest safe block.
    safe_stream: BlockStream,
    /// A stream over the finalized block accepted as canonical.
    finalized_stream: BlockStream,
    /// Number of L1 blocks to keep distance from the L1 head for the derivation pipeline.
    /// When non-zero, derivation receives the block at `head - verifier_l1_confs` rather than
    /// the real head. The sequencer's watch channel always receives the real head.
    verifier_l1_confs: u64,
    /// Shared atomic holding the current L1 head block number.
    ///
    /// Updated on every new L1 head so the [`ConfDepthProvider`] in the derivation pipeline
    /// can enforce the confirmation depth cutoff at the chain-provider level.
    ///
    /// [`ConfDepthProvider`]: base_consensus_providers::ConfDepthProvider
    l1_head_number: Arc<AtomicU64>,
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
        safe_head_tx: watch::Sender<Option<BlockInfo>>,
        finalized_head_tx: watch::Sender<Option<BlockInfo>>,
        derivation_client: L1WatcherDerivationClient_,
        signer: Option<mpsc::Sender<Address>>,
        cancellation: CancellationToken,
        head_stream: BlockStream,
        safe_stream: BlockStream,
        finalized_stream: BlockStream,
        verifier_l1_confs: u64,
        l1_head_number: Arc<AtomicU64>,
    ) -> Self {
        Self {
            rollup_config,
            l1_provider,
            latest_head: l1_head_updates_tx,
            safe_head: safe_head_tx,
            finalized_head: finalized_head_tx,
            derivation_client,
            block_signer_sender: signer,
            cancellation,
            head_stream,
            safe_stream,
            finalized_stream,
            verifier_l1_confs,
            l1_head_number,
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

                        // Update the shared atomic so the ConfDepthProvider in the
                        // derivation pipeline can enforce the confirmation depth cutoff.
                        self.l1_head_number.store(head_block_info.number, Ordering::Relaxed);

                        // Apply verifier confirmation delay: derive from
                        // `head - verifier_l1_confs` when the chain is deep enough.
                        let derivation_block = if self.verifier_l1_confs > 0
                            && head_block_info.number >= self.verifier_l1_confs
                        {
                            let target = head_block_info.number - self.verifier_l1_confs;
                            // Delayed block reads are best effort. A request timeout is handled
                            // like any other fetch error: skip this head and try the next one.
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

                        // Log requests are retried within a time budget; exhausting it fails the
                        // actor, and `None` means shutdown.
                        let Some(logs) = LogRetrier::fetch_logs_with_retry(
                            &self.l1_provider,
                            filter,
                            &cancel,
                            derivation_block,
                            INITIAL_BACKOFF,
                            MAX_BACKOFF,
                            LogRetrier::RETRY_BUDGET,
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
                new_safe = self.safe_stream.next() => match new_safe {
                    None => {
                        return Err(L1WatcherActorError::StreamEnded);
                    }
                    Some(safe_block_info) => {
                        self.safe_head.send_replace(Some(safe_block_info));
                    }
                },
                new_finalized = self.finalized_stream.next() => match new_finalized {
                    None => {
                        return Err(L1WatcherActorError::StreamEnded);
                    }
                    Some(finalized_block_info) => {
                        self.finalized_head.send_replace(Some(finalized_block_info));
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
    use base_common_genesis::RollupConfig;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::DerivationClientResult;

    type BoxedBlockStream = Pin<Box<dyn Stream<Item = BlockInfo> + Unpin + Send>>;

    // ---------------------------------------------------------------------------
    // Mock L1BlockFetcher used by LogRetrier tests.
    // ---------------------------------------------------------------------------

    struct MockFetcher {
        call_count: Arc<AtomicU32>,
        call_times: Arc<Mutex<Vec<tokio::time::Instant>>>,
        fail_count: u32,
    }

    impl MockFetcher {
        fn always_fail() -> Self {
            Self::fail_times(u32::MAX)
        }

        fn fail_times(n: u32) -> Self {
            Self {
                call_count: Arc::new(AtomicU32::new(0)),
                call_times: Arc::new(Mutex::new(Vec::new())),
                fail_count: n,
            }
        }

        fn always_succeed() -> Self {
            Self::fail_times(0)
        }
    }

    #[async_trait]
    impl L1BlockFetcher for MockFetcher {
        type Error = String;

        async fn get_logs(&self, _: Filter) -> Result<Vec<Log>, Self::Error> {
            self.call_times.lock().unwrap().push(tokio::time::Instant::now());
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            if count < self.fail_count { Err("transient error".to_string()) } else { Ok(vec![]) }
        }

        async fn get_block(&self, _: BlockId) -> Result<Option<Block>, Self::Error> {
            Ok(None)
        }
    }

    /// Fetcher whose requests never complete, like a stalled L1 endpoint.
    ///
    /// Hand-rolled because a mockall `returning` closure must produce the method's result
    /// immediately, so it cannot express a request that never resolves.
    struct HangingFetcher;

    #[async_trait]
    impl L1BlockFetcher for HangingFetcher {
        type Error = String;

        async fn get_logs(&self, _: Filter) -> Result<Vec<Log>, Self::Error> {
            std::future::pending().await
        }

        async fn get_block(&self, _: BlockId) -> Result<Option<Block>, Self::Error> {
            std::future::pending().await
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
        /// Return an error once, then a default [`Block`].
        ErrorOnceThenDefault,
    }

    struct ConfigurableFetcher {
        get_block_behavior: GetBlockBehavior,
        get_block_requested_ids: Arc<Mutex<Vec<BlockId>>>,
        get_block_call_count: Arc<AtomicU32>,
    }

    impl ConfigurableFetcher {
        fn returning_default_block() -> Self {
            Self {
                get_block_behavior: GetBlockBehavior::Default,
                get_block_requested_ids: Arc::new(Mutex::new(Vec::new())),
                get_block_call_count: Arc::new(AtomicU32::new(0)),
            }
        }

        fn returning_none() -> Self {
            Self {
                get_block_behavior: GetBlockBehavior::None,
                get_block_requested_ids: Arc::new(Mutex::new(Vec::new())),
                get_block_call_count: Arc::new(AtomicU32::new(0)),
            }
        }

        fn returning_error() -> Self {
            Self {
                get_block_behavior: GetBlockBehavior::Err,
                get_block_requested_ids: Arc::new(Mutex::new(Vec::new())),
                get_block_call_count: Arc::new(AtomicU32::new(0)),
            }
        }

        fn error_once_then_default() -> Self {
            Self {
                get_block_behavior: GetBlockBehavior::ErrorOnceThenDefault,
                get_block_requested_ids: Arc::new(Mutex::new(Vec::new())),
                get_block_call_count: Arc::new(AtomicU32::new(0)),
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
            let call_count = self.get_block_call_count.fetch_add(1, Ordering::SeqCst);
            match self.get_block_behavior {
                GetBlockBehavior::ErrorOnceThenDefault if call_count == 0 => {
                    Err("provider request timed out".to_string())
                }
                GetBlockBehavior::Default | GetBlockBehavior::ErrorOnceThenDefault => {
                    Ok(Some(Block::default()))
                }
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
    /// Returns the derivation client, the watch receiver for the raw head, and the
    /// shared atomic holding the L1 head number.
    async fn run_actor<F: L1BlockFetcher>(
        fetcher: F,
        head_blocks: Vec<BlockInfo>,
        verifier_l1_confs: u64,
    ) -> (RecordingDerivationClient, watch::Receiver<Option<BlockInfo>>, Arc<AtomicU64>) {
        let derivation_client = RecordingDerivationClient::default();
        let (l1_head_tx, l1_head_rx) = watch::channel(None);
        let cancel = CancellationToken::new();
        let l1_head_number = Arc::new(AtomicU64::new(0));

        let head_stream: BoxedBlockStream = Box::pin(futures::stream::iter(head_blocks));
        // Safe and finalized streams never yield — actor will exit via head stream ending.
        let safe_stream: BoxedBlockStream = Box::pin(futures::stream::pending());
        let finalized_stream: BoxedBlockStream = Box::pin(futures::stream::pending());

        let actor = L1WatcherActor::new(
            Arc::new(RollupConfig::default()),
            fetcher,
            l1_head_tx,
            watch::channel(None).0,
            watch::channel(None).0,
            derivation_client.clone(),
            None,
            cancel,
            head_stream,
            safe_stream,
            finalized_stream,
            verifier_l1_confs,
            Arc::clone(&l1_head_number),
        );

        // The actor loop will process all head_stream items then return StreamEnded.
        let _ = actor.start(()).await;

        (derivation_client, l1_head_rx, l1_head_number)
    }

    // ---------------------------------------------------------------------------
    // LogRetrier tests
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
            LogRetrier::RETRY_BUDGET,
        )
        .await;
        assert!(matches!(result, Ok(Some(_))));
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_logs_keeps_retrying_within_the_budget_until_l1_recovers() {
        let cancel = CancellationToken::new();
        let fetcher = MockFetcher::fail_times(50);
        let call_count = Arc::clone(&fetcher.call_count);
        let result = LogRetrier::fetch_logs_with_retry(
            &fetcher,
            Filter::new(),
            &cancel,
            dummy_block(),
            Duration::from_millis(50),
            Duration::from_millis(500),
            LogRetrier::RETRY_BUDGET,
        )
        .await;
        assert!(matches!(result, Ok(Some(_))));
        assert_eq!(call_count.load(Ordering::SeqCst), 51);
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_logs_gives_up_when_a_request_hangs_past_the_budget() {
        let cancel = CancellationToken::new();
        let started = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            2 * LogRetrier::RETRY_BUDGET,
            LogRetrier::fetch_logs_with_retry(
                &HangingFetcher,
                Filter::new(),
                &cancel,
                dummy_block(),
                Duration::from_millis(50),
                Duration::from_millis(500),
                LogRetrier::RETRY_BUDGET,
            ),
        )
        .await
        .expect("the retry budget must bound an in-flight request");
        assert!(matches!(result, Err(L1WatcherActorError::RetriesExhausted)));
        assert_eq!(started.elapsed(), LogRetrier::RETRY_BUDGET);
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_logs_spaces_failed_attempts_by_capped_backoff() {
        let cancel = CancellationToken::new();
        let fetcher = MockFetcher::always_fail();
        let call_times = Arc::clone(&fetcher.call_times);
        let result = LogRetrier::fetch_logs_with_retry(
            &fetcher,
            Filter::new(),
            &cancel,
            dummy_block(),
            Duration::from_secs(1),
            Duration::from_secs(4),
            Duration::from_secs(20),
        )
        .await;
        assert!(matches!(result, Err(L1WatcherActorError::RetriesExhausted)));

        let call_times = call_times.lock().unwrap();
        let gaps: Vec<u64> =
            call_times.windows(2).map(|pair| (pair[1] - pair[0]).as_secs()).collect();
        assert_eq!(gaps, [1, 2, 4, 4, 4, 4]);
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_logs_stops_attempting_once_the_budget_elapses_during_backoff() {
        let cancel = CancellationToken::new();
        let fetcher = MockFetcher::always_fail();
        let call_times = Arc::clone(&fetcher.call_times);
        let budget = Duration::from_secs(30);
        let started = tokio::time::Instant::now();
        let result = LogRetrier::fetch_logs_with_retry(
            &fetcher,
            Filter::new(),
            &cancel,
            dummy_block(),
            Duration::from_secs(20),
            Duration::from_secs(20),
            budget,
        )
        .await;
        assert!(matches!(result, Err(L1WatcherActorError::RetriesExhausted)));
        assert_eq!(started.elapsed(), budget);
        let offsets: Vec<u64> =
            call_times.lock().unwrap().iter().map(|at| (*at - started).as_secs()).collect();
        assert_eq!(offsets, [0, 20]);
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_logs_cancelled_during_backoff_returns_without_waiting_out_the_sleep() {
        let cancel = CancellationToken::new();
        let fetcher = MockFetcher::always_fail();
        let call_count = Arc::clone(&fetcher.call_count);
        let started = tokio::time::Instant::now();
        let retrier = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                LogRetrier::fetch_logs_with_retry(
                    &fetcher,
                    Filter::new(),
                    &cancel,
                    dummy_block(),
                    Duration::from_secs(3600),
                    Duration::from_secs(3600),
                    Duration::from_secs(7200),
                )
                .await
            }
        });

        // Yielding keeps the runtime busy, so paused time does not auto-advance meanwhile.
        while call_count.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        cancel.cancel();
        assert!(matches!(retrier.await.unwrap(), Ok(None)));
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_logs_cancelled_during_in_flight_request_returns_none() {
        let cancel = CancellationToken::new();
        let retrier = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                LogRetrier::fetch_logs_with_retry(
                    &HangingFetcher,
                    Filter::new(),
                    &cancel,
                    dummy_block(),
                    Duration::from_millis(50),
                    Duration::from_millis(500),
                    LogRetrier::RETRY_BUDGET,
                )
                .await
            }
        });

        tokio::time::sleep(LogRetrier::RETRY_BUDGET / 2).await;
        cancel.cancel();
        let cancelled_at = tokio::time::Instant::now();
        assert!(matches!(retrier.await.unwrap(), Ok(None)));
        assert_eq!(cancelled_at.elapsed(), Duration::ZERO);
    }

    // ---------------------------------------------------------------------------
    // Verifier L1 confs tests
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn zero_confs_forwards_real_head_to_derivation() {
        let head = block_at(100);
        let (client, rx, l1_head_number) =
            run_actor(ConfigurableFetcher::returning_default_block(), vec![head], 0).await;

        // Derivation receives the real head block.
        let heads = client.sent_heads();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].number, 100);

        // Watch channel also has the real head.
        assert_eq!(rx.borrow().unwrap().number, 100);

        // L1 head number atomic is updated to the real head.
        assert_eq!(l1_head_number.load(Ordering::Relaxed), 100);
    }

    #[tokio::test]
    async fn confs_delay_fetches_earlier_block_for_derivation() {
        let fetcher = ConfigurableFetcher::returning_default_block();
        let head = block_at(100);
        let (client, rx, l1_head_number) = run_actor(fetcher, vec![head], 4).await;

        // Derivation receives the *delayed* block (default Block → BlockInfo with number 0).
        let heads = client.sent_heads();
        assert_eq!(heads.len(), 1);
        // The fetcher returns Block::default() which converts to BlockInfo with number 0.
        assert_eq!(heads[0].number, 0);

        // Watch channel still has the real head.
        assert_eq!(rx.borrow().unwrap().number, 100);

        // L1 head number atomic is set to the real head (not the delayed one).
        assert_eq!(l1_head_number.load(Ordering::Relaxed), 100);
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
        let (client, _rx, _) = run_actor(fetcher, vec![block_at(2)], 4).await;

        // No get_block call should have been made.
        assert!(ids.lock().unwrap().is_empty());

        // Derivation receives the real head.
        let heads = client.sent_heads();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].number, 2);
    }

    #[tokio::test]
    async fn confs_delayed_block_not_found_skips_derivation() {
        let (client, rx, _) =
            run_actor(ConfigurableFetcher::returning_none(), vec![block_at(100)], 4).await;

        // Derivation should NOT receive any head — the block was not found.
        assert!(client.sent_heads().is_empty());

        // Watch channel still has the real head.
        assert_eq!(rx.borrow().unwrap().number, 100);
    }

    #[tokio::test]
    async fn confs_delayed_block_fetch_error_skips_derivation() {
        let (client, rx, _) =
            run_actor(ConfigurableFetcher::returning_error(), vec![block_at(100)], 4).await;

        // Derivation should NOT receive any head — the fetch errored.
        assert!(client.sent_heads().is_empty());

        // Watch channel still has the real head.
        assert_eq!(rx.borrow().unwrap().number, 100);
    }

    #[tokio::test]
    async fn confs_delayed_block_timeout_skips_head_and_continues() {
        let fetcher = ConfigurableFetcher::error_once_then_default();
        let ids = Arc::clone(&fetcher.get_block_requested_ids);
        let (client, rx, _) = run_actor(fetcher, vec![block_at(100), block_at(104)], 4).await;

        let heads = client.sent_heads();
        assert_eq!(heads.len(), 1);
        // The first provider timeout is skipped. The second request succeeds with
        // Block::default().
        assert_eq!(heads[0].number, 0);
        assert_eq!(rx.borrow().unwrap().number, 104);

        let requested = ids.lock().unwrap().clone();
        assert_eq!(requested, vec![BlockId::Number(96u64.into()), BlockId::Number(100u64.into())]);
    }

    #[tokio::test]
    async fn confs_l1_head_number_tracks_last_head() {
        // Verify the shared atomic always holds the most recent L1 head number,
        // which the ConfDepthProvider reads to enforce the confirmation cutoff.
        let fetcher = ConfigurableFetcher::returning_default_block();
        let (_, _, l1_head_number) =
            run_actor(fetcher, vec![block_at(10), block_at(20), block_at(30)], 4).await;

        // After processing all three heads, the atomic should hold the last one.
        assert_eq!(l1_head_number.load(Ordering::Relaxed), 30);
    }

    #[tokio::test]
    async fn confs_mixed_shallow_and_deep_heads() {
        // First head is too shallow (3 < 4), second is deep enough (10 >= 4).
        let fetcher = ConfigurableFetcher::returning_default_block();
        let ids = Arc::clone(&fetcher.get_block_requested_ids);
        let (client, _rx, _) = run_actor(fetcher, vec![block_at(3), block_at(10)], 4).await;

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
