//! [`NodeActor`] implementation for an L1 chain watcher that polls for L1 block updates over HTTP
//! RPC.

use std::{sync::Arc, time::Duration};

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
    NodeActor,
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
                        // Send the head update event to all consumers.
                        self.latest_head.send_replace(Some(head_block_info));
                        self.derivation_client.send_new_l1_head(head_block_info).await.map_err(|e| {
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
                            .select(head_block_info.hash);

                        let Some(logs) = LogRetrier::fetch_logs_with_retry(
                            &self.l1_provider,
                            filter,
                            &cancel,
                            head_block_info,
                            INITIAL_BACKOFF,
                            MAX_BACKOFF,
                        )
                        .await?
                        else {
                            return Ok(());
                        };
                        let ecotone_active = self.rollup_config.is_ecotone_active(head_block_info.timestamp);
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
    use std::sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    };

    use alloy_eips::BlockId;
    use alloy_primitives::B256;
    use alloy_rpc_types_eth::{Block, Filter, Log};
    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    use super::*;

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
}
