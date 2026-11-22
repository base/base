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
    BaseFeature, RollupConfig, SystemConfigLog, SystemConfigUpdate, UnsafeBlockSignerUpdate,
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
    /// Fetch logs matching `filter`, retrying with capped exponential backoff until the fetch
    /// succeeds or `cancel` fires.
    ///
    /// Returns `Some(logs)` on success and `None` once `cancel` fires, whether during a request
    /// or a backoff sleep.
    ///
    /// There is no attempt limit. Every L1 RPC failure is treated as transient, because exiting
    /// cannot help: a restarted node needs the same L1 to start and to make progress. Retrying
    /// keeps the node alive through an L1 outage, so it resumes as soon as L1 recovers. Each
    /// request is still bounded by the provider's request timeout.
    ///
    /// `initial_backoff` and `max_backoff` are caller-supplied so tests can use tiny values.
    pub async fn fetch_logs_with_retry<F>(
        provider: &F,
        filter: Filter,
        cancel: &CancellationToken,
        block_info: BlockInfo,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Option<Vec<Log>>
    where
        F: L1BlockFetcher,
    {
        let mut backoff = initial_backoff;
        let mut attempt: u64 = 0;
        loop {
            attempt += 1;
            let result = select! {
                _ = cancel.cancelled() => return None,
                result = provider.get_logs(filter.clone()) => result,
            };
            match result {
                Ok(logs) => return Some(logs),
                Err(e) => {
                    warn!(
                        target: "l1_watcher",
                        error = %e,
                        block_hash = %block_info.hash,
                        block_number = block_info.number,
                        attempt,
                        retry_in = ?backoff,
                        "Failed to fetch logs for L1 head"
                    );
                    select! {
                        _ = cancel.cancelled() => return None,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
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
        // Caps the retry interval during an L1 outage so the watcher resumes within a few
        // seconds of L1 recovering without flooding the provider while it is down.
        const MAX_BACKOFF: Duration = Duration::from_secs(5);
        // How often to check whether a head whose logs keep failing has been reorged out, once
        // a newer head is waiting. One L1 slot.
        const REORG_CHECK_INTERVAL: Duration = Duration::from_secs(12);
        // Bound on one reorg check, so a slow L1 cannot hold up the finalized stream or the
        // log fetch for long.
        const REORG_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

        Metrics::l1_verifier_confs_depth().set(self.verifier_l1_confs as f64);
        if self.verifier_l1_confs > 0 {
            info!(
                target: "l1_watcher",
                verifier_l1_confs = self.verifier_l1_confs,
                "Verifier L1 confirmation delay enabled"
            );
        }

        let cancel = self.cancellation.clone();

        // A head that arrived while logs for an older head were still being fetched. It is
        // processed next, ahead of the streams.
        let mut superseding_head: Option<BlockInfo> = None;

        loop {
            let head_block_info = match superseding_head.take() {
                Some(head) => head,
                None => select! {
                    _ = cancel.cancelled() => {
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
                        Some(head_block_info) => head_block_info,
                    },
                    new_safe = self.safe_stream.next() => match new_safe {
                        None => {
                            return Err(L1WatcherActorError::StreamEnded);
                        }
                        Some(safe_block_info) => {
                            self.safe_head.send_replace(Some(safe_block_info));
                            continue;
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
                            continue;
                        }
                    },
                },
            };

            // Always broadcast the real head so the sequencer's
            // DelayedL1OriginSelectorProvider can compute its own offset.
            self.latest_head.send_replace(Some(head_block_info));

            // Update the shared atomic so the ConfDepthProvider in the
            // derivation pipeline can enforce the confirmation depth cutoff.
            self.l1_head_number.store(head_block_info.number, Ordering::Relaxed);

            let derivation_block =
                if self.verifier_l1_confs > 0 && head_block_info.number >= self.verifier_l1_confs {
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

            Metrics::l1_verifier_derivation_head().absolute(derivation_block.number);
            self.derivation_client.send_new_l1_head(derivation_block).await.map_err(|e| {
                warn!(target: "l1_watcher", error = %e, "Error sending l1 head update to derivation actor");
                L1WatcherActorError::DerivationClientError(e)
            })?;

            let filter_address = self.rollup_config.l1_system_config_address;
            let filter = Filter::new().address(filter_address).select(derivation_block.hash);

            // Log requests are retried until they succeed or the node shuts down, so an L1
            // outage pauses this watcher instead of exiting the node. While waiting, the safe and
            // finalized streams are still served. The first head that arrives meanwhile is kept
            // and processed next, and the head stream is not polled again until then.
            //
            // This head is abandoned only once L1 reports a different block at its height, that
            // is, once it has been reorged out and the RPC may never serve its logs. A canonical
            // head is never abandoned, because its logs may carry an unsafe block signer update
            // that later heads' filters would not include.
            let fetch = LogRetrier::fetch_logs_with_retry(
                &self.l1_provider,
                filter,
                &cancel,
                derivation_block,
                INITIAL_BACKOFF,
                MAX_BACKOFF,
            );
            tokio::pin!(fetch);
            let mut next_reorg_check = tokio::time::Instant::now() + REORG_CHECK_INTERVAL;
            let mut newer_head: Option<BlockInfo> = None;
            let logs = loop {
                select! {
                    logs = &mut fetch => match logs {
                        Some(logs) => break Some(logs),
                        None => return Ok(()),
                    },
                    new_head = self.head_stream.next(), if newer_head.is_none() => match new_head {
                        None => return Err(L1WatcherActorError::StreamEnded),
                        Some(new_head) => newer_head = Some(new_head),
                    },
                    new_safe = self.safe_stream.next() => match new_safe {
                        None => return Err(L1WatcherActorError::StreamEnded),
                        Some(safe_block_info) => {
                            self.safe_head.send_replace(Some(safe_block_info));
                        }
                    },
                    new_finalized = self.finalized_stream.next() => match new_finalized {
                        None => return Err(L1WatcherActorError::StreamEnded),
                        Some(finalized_block_info) => {
                            self.finalized_head.send_replace(Some(finalized_block_info));
                            self.derivation_client.send_finalized_l1_block(finalized_block_info).await.map_err(|e| {
                                warn!(target: "l1_watcher", error = %e, "Error sending finalized l1 block update to derivation actor");
                                L1WatcherActorError::DerivationClientError(e)
                            })?;
                        }
                    },
                    _ = tokio::time::sleep_until(next_reorg_check), if newer_head.is_some() => {
                        let canonical = select! {
                            _ = cancel.cancelled() => return Ok(()),
                            block = tokio::time::timeout(
                                REORG_CHECK_TIMEOUT,
                                self.l1_provider
                                    .get_block(BlockId::Number(derivation_block.number.into())),
                            ) => block,
                        };
                        if let Ok(Ok(Some(block))) = canonical
                            && block.header.hash != derivation_block.hash
                        {
                            break None;
                        }
                        next_reorg_check = tokio::time::Instant::now() + REORG_CHECK_INTERVAL;
                    }
                }
            };
            superseding_head = newer_head;
            let Some(logs) = logs else {
                warn!(
                    target: "l1_watcher",
                    block_hash = %derivation_block.hash,
                    block_number = derivation_block.number,
                    new_head = ?superseding_head.map(|head| head.number),
                    "L1 head was reorged out before its logs were fetched; moving to the newer head"
                );
                continue;
            };
            let ecotone_active = self.rollup_config.is_feature_active_cascading(
                BaseFeature::BlobAwareL1Fees,
                derivation_block.timestamp,
            );
            for log in logs {
                let sys_cfg_log = SystemConfigLog::new(log.into(), ecotone_active);
                if let Ok(SystemConfigUpdate::UnsafeBlockSigner(UnsafeBlockSignerUpdate {
                    unsafe_block_signer,
                })) = sys_cfg_log.build()
                {
                    info!(
                        target: "l1_watcher",
                        signer = %unsafe_block_signer,
                        "Unsafe block signer update"
                    );
                    if let Some(ref block_signer_sender) = self.block_signer_sender
                        && let Err(e) = block_signer_sender.send(unsafe_block_signer).await
                    {
                        error!(
                            target: "l1_watcher",
                            error = %e,
                            "Error sending unsafe block signer update"
                        );
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

    /// Fetcher whose log requests never complete, standing in for a hung L1 request.
    ///
    /// Hand-rolled like the other fetcher doubles in this module: `L1BlockFetcher` has no
    /// automock, and a mockall expectation cannot return a future that never resolves.
    struct PendingFetcher;

    #[async_trait]
    impl L1BlockFetcher for PendingFetcher {
        type Error = String;

        async fn get_logs(&self, _: Filter) -> Result<Vec<Log>, Self::Error> {
            futures::future::pending().await
        }

        async fn get_block(&self, _: BlockId) -> Result<Option<Block>, Self::Error> {
            Ok(None)
        }
    }

    /// Fetcher whose log requests fail a set number of times for one block hash, standing in
    /// for an RPC that is briefly flaky for, or no longer serves, that block. Reports
    /// `canonical` as the block hash at every height and records the hashes whose logs were
    /// fetched.
    ///
    /// Hand-rolled like the other fetcher doubles in this module: `L1BlockFetcher` has no
    /// automock, and the fake needs failure state shared across calls keyed on the filter.
    struct HashFailingFetcher {
        failing: B256,
        failures_left: AtomicU32,
        canonical: B256,
        hang_block_lookup: bool,
        fetched: Arc<Mutex<Vec<B256>>>,
    }

    impl HashFailingFetcher {
        fn new(failing: B256, failures: u32, canonical: B256) -> Self {
            Self {
                failing,
                failures_left: AtomicU32::new(failures),
                canonical,
                hang_block_lookup: false,
                fetched: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Makes block lookups never complete, standing in for a hung L1 request.
        fn with_hanging_block_lookup(mut self) -> Self {
            self.hang_block_lookup = true;
            self
        }
    }

    #[async_trait]
    impl L1BlockFetcher for HashFailingFetcher {
        type Error = String;

        async fn get_logs(&self, filter: Filter) -> Result<Vec<Log>, Self::Error> {
            let hash = filter.get_block_hash().expect("watcher filters logs by block hash");
            if hash == self.failing
                && self
                    .failures_left
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok()
            {
                return Err("unknown block".to_string());
            }
            self.fetched.lock().unwrap().push(hash);
            Ok(vec![])
        }

        async fn get_block(&self, _: BlockId) -> Result<Option<Block>, Self::Error> {
            if self.hang_block_lookup {
                std::future::pending::<()>().await;
            }
            let mut block: Block = Block::default();
            block.header.hash = self.canonical;
            Ok(Some(block))
        }
    }

    fn dummy_block() -> BlockInfo {
        BlockInfo { hash: B256::ZERO, number: 0, parent_hash: B256::ZERO, timestamp: 0 }
    }

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

        let _ = actor.start(()).await;

        (derivation_client, l1_head_rx, l1_head_number)
    }

    /// Run an [`L1WatcherActor`] with the given head and safe streams for at most `run_for`,
    /// with verifier confirmations disabled.
    ///
    /// Returns whether the actor exited on its own within that time, the derivation client, and
    /// the safe-head watch receiver.
    async fn run_actor_for<F: L1BlockFetcher>(
        fetcher: F,
        head_stream: BoxedBlockStream,
        safe_stream: BoxedBlockStream,
        run_for: Duration,
    ) -> (bool, RecordingDerivationClient, watch::Receiver<Option<BlockInfo>>) {
        let derivation_client = RecordingDerivationClient::default();
        let (safe_tx, safe_rx) = watch::channel(None);
        let actor = L1WatcherActor::new(
            Arc::new(RollupConfig::default()),
            fetcher,
            watch::channel(None).0,
            safe_tx,
            watch::channel(None).0,
            derivation_client.clone(),
            None,
            CancellationToken::new(),
            head_stream,
            safe_stream,
            Box::pin(futures::stream::pending()),
            0,
            Arc::new(AtomicU64::new(0)),
        );
        let exited = tokio::time::timeout(run_for, actor.start(())).await.is_ok();
        (exited, derivation_client, safe_rx)
    }

    /// A stream that yields `blocks` and then stays open without yielding.
    fn open_stream(blocks: Vec<BlockInfo>) -> BoxedBlockStream {
        Box::pin(futures::stream::iter(blocks).chain(futures::stream::pending()))
    }

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
        assert!(result.is_some());
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
        assert!(result.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_logs_keeps_retrying_through_long_outage() {
        // Fail far more often than any fixed attempt budget would allow; the retrier must
        // keep going rather than give up and take the node down.
        let cancel = CancellationToken::new();
        let fetcher = MockFetcher::fail_times(1_000);
        let call_count = Arc::clone(&fetcher.call_count);
        let result = LogRetrier::fetch_logs_with_retry(
            &fetcher,
            Filter::new(),
            &cancel,
            dummy_block(),
            Duration::from_nanos(1),
            Duration::from_nanos(1),
        )
        .await;
        assert!(result.is_some());
        assert_eq!(call_count.load(Ordering::SeqCst), 1_001);
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_logs_cancelled_during_backoff_returns_none() {
        let cancel = CancellationToken::new();
        let fetcher = MockFetcher::always_fail();
        let retry = LogRetrier::fetch_logs_with_retry(
            &fetcher,
            Filter::new(),
            &cancel,
            dummy_block(),
            Duration::from_secs(10), // long sleep ensures only the cancel arm can end it
            Duration::from_secs(10),
        );
        // The first request fails at once, so the retrier is in its backoff sleep by the time
        // the cancellation fires.
        let cancel_later = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(retry, cancel_later);
        assert!(result.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_logs_cancelled_during_request_returns_none() {
        let cancel = CancellationToken::new();
        let retry = LogRetrier::fetch_logs_with_retry(
            &PendingFetcher,
            Filter::new(),
            &cancel,
            dummy_block(),
            Duration::from_secs(10),
            Duration::from_secs(10),
        );
        // The request never completes, so the retrier is waiting on it when cancelled.
        let cancel_later = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(retry, cancel_later);
        assert!(result.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn actor_survives_long_l1_log_outage() {
        // With the production backoff, 30 consecutive failures span well over a minute of
        // simulated outage. The actor must ride it out instead of exiting.
        let fetcher = MockFetcher::fail_times(30);
        let call_count = Arc::clone(&fetcher.call_count);
        let (exited, client, _) = run_actor_for(
            fetcher,
            open_stream(vec![block_at(1)]),
            open_stream(vec![]),
            Duration::from_secs(10 * 60),
        )
        .await;

        assert!(!exited, "actor exited during the L1 outage");
        assert_eq!(client.sent_heads().len(), 1);
        assert_eq!(call_count.load(Ordering::SeqCst), 31);
    }

    #[tokio::test(start_paused = true)]
    async fn newer_head_replaces_head_whose_logs_cannot_be_fetched() {
        // Logs for a reorged-out block may never be served again. Once L1 reports a different
        // block at its height, a newer head must take over so the stale block cannot pin the
        // watcher.
        let stale = block_at(1);
        // L1 now reports a different block at the stale head's height.
        let fetcher = HashFailingFetcher::new(stale.hash, u32::MAX, B256::repeat_byte(0xff));
        let fetched = Arc::clone(&fetcher.fetched);

        let (exited, client, _) = run_actor_for(
            fetcher,
            open_stream(vec![stale, block_at(2)]),
            open_stream(vec![]),
            Duration::from_secs(5 * 60),
        )
        .await;

        assert!(!exited);
        let sent: Vec<_> = client.sent_heads().iter().map(|b| b.number).collect();
        assert_eq!(sent, vec![1, 2]);
        assert_eq!(*fetched.lock().unwrap(), vec![block_at(2).hash]);
    }

    #[tokio::test(start_paused = true)]
    async fn briefly_failing_head_logs_are_fetched_before_newer_head() {
        // A newer head arriving during a short L1 hiccup must not cost the current head its
        // logs, which may carry an unsafe block signer update.
        let first = block_at(1);
        let fetcher = HashFailingFetcher::new(first.hash, 3, first.hash);
        let fetched = Arc::clone(&fetcher.fetched);

        let (exited, client, _) = run_actor_for(
            fetcher,
            open_stream(vec![first, block_at(2)]),
            open_stream(vec![]),
            Duration::from_secs(5 * 60),
        )
        .await;

        assert!(!exited);
        let sent: Vec<_> = client.sent_heads().iter().map(|b| b.number).collect();
        assert_eq!(sent, vec![1, 2]);
        assert_eq!(*fetched.lock().unwrap(), vec![first.hash, block_at(2).hash]);
    }

    #[tokio::test(start_paused = true)]
    async fn hung_reorg_check_does_not_stall_safe_head_updates() {
        // A reorg check runs once a newer head waits. If it hangs during the outage, the safe
        // block arriving later must still be published, and the head must not be abandoned.
        let first = block_at(1);
        let fetcher =
            HashFailingFetcher::new(first.hash, u32::MAX, first.hash).with_hanging_block_lookup();
        let late_safe: BoxedBlockStream = Box::pin(
            futures::stream::once(Box::pin(async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                block_at(7)
            }))
            .chain(futures::stream::pending()),
        );

        let (exited, client, safe_rx) = run_actor_for(
            fetcher,
            open_stream(vec![first, block_at(2)]),
            late_safe,
            Duration::from_secs(5 * 60),
        )
        .await;

        assert!(!exited);
        assert_eq!(safe_rx.borrow().unwrap().number, 7);
        let sent: Vec<_> = client.sent_heads().iter().map(|b| b.number).collect();
        assert_eq!(sent, vec![1]);
    }

    #[tokio::test(start_paused = true)]
    async fn canonical_head_is_not_abandoned_during_long_outage() {
        // A canonical head's logs may carry a signer update that later heads' filters miss, so
        // the watcher keeps retrying it however long the outage lasts, even with a newer head
        // waiting.
        let first = block_at(1);
        let fetcher = HashFailingFetcher::new(first.hash, u32::MAX, first.hash);
        let fetched = Arc::clone(&fetcher.fetched);

        let (exited, client, _) = run_actor_for(
            fetcher,
            open_stream(vec![first, block_at(2)]),
            open_stream(vec![]),
            Duration::from_secs(30 * 60),
        )
        .await;

        assert!(!exited);
        let sent: Vec<_> = client.sent_heads().iter().map(|b| b.number).collect();
        assert_eq!(sent, vec![1]);
        assert!(fetched.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn safe_head_updates_while_head_logs_are_failing() {
        let head = block_at(1);
        let fetcher = HashFailingFetcher::new(head.hash, u32::MAX, head.hash);

        let (exited, _, safe_rx) = run_actor_for(
            fetcher,
            open_stream(vec![head]),
            open_stream(vec![block_at(7)]),
            Duration::from_secs(5 * 60),
        )
        .await;

        assert!(!exited);
        assert_eq!(safe_rx.borrow().unwrap().number, 7);
    }

    #[tokio::test]
    async fn zero_confs_forwards_real_head_to_derivation() {
        let head = block_at(100);
        let (client, rx, l1_head_number) =
            run_actor(ConfigurableFetcher::returning_default_block(), vec![head], 0).await;

        let heads = client.sent_heads();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].number, 100);

        assert_eq!(rx.borrow().unwrap().number, 100);

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

        assert_eq!(rx.borrow().unwrap().number, 100);

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

        assert!(ids.lock().unwrap().is_empty());

        let heads = client.sent_heads();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].number, 2);
    }

    #[tokio::test]
    async fn confs_delayed_block_not_found_skips_derivation() {
        let (client, rx, _) =
            run_actor(ConfigurableFetcher::returning_none(), vec![block_at(100)], 4).await;

        assert!(client.sent_heads().is_empty());

        assert_eq!(rx.borrow().unwrap().number, 100);
    }

    #[tokio::test]
    async fn confs_delayed_block_fetch_error_skips_derivation() {
        let (client, rx, _) =
            run_actor(ConfigurableFetcher::returning_error(), vec![block_at(100)], 4).await;

        assert!(client.sent_heads().is_empty());

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

        assert_eq!(l1_head_number.load(Ordering::Relaxed), 30);
    }

    #[tokio::test]
    async fn confs_mixed_shallow_and_deep_heads() {
        // First head is too shallow (3 < 4), second is deep enough (10 >= 4).
        let fetcher = ConfigurableFetcher::returning_default_block();
        let ids = Arc::clone(&fetcher.get_block_requested_ids);
        let (client, _rx, _) = run_actor(fetcher, vec![block_at(3), block_at(10)], 4).await;

        let heads = client.sent_heads();
        assert_eq!(heads.len(), 2);
        assert_eq!(heads[0].number, 3);
        assert_eq!(heads[1].number, 0);

        assert_eq!(ids.lock().unwrap().len(), 1);
        assert_eq!(ids.lock().unwrap()[0], BlockId::Number(6u64.into()));
    }
}
