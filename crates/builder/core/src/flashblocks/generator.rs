use std::sync::Arc;

use alloy_primitives::B256;
use futures::{Future, FutureExt};
use parking_lot::Mutex;
use reth_basic_payload_builder::{HeaderForPayload, PayloadConfig, PrecachedState};
use reth_execution_cache::SavedCache;
use reth_node_api::{NodePrimitives, PayloadKind};
use reth_payload_builder::{
    BuildNewPayload, KeepPayloadJobAlive, PayloadBuilderError, PayloadId, PayloadJob,
    PayloadJobGenerator,
};
use reth_payload_primitives::{BuiltPayload, PayloadAttributes};
use reth_primitives_traits::HeaderTy;
use reth_provider::{BlockReaderIdExt, CanonStateNotification, StateProviderFactory};
use reth_revm::cached::CachedReads;
use reth_tasks::Runtime;
use tokio::{sync::watch, time::Sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

use crate::{PayloadBuilder, PayloadJobDeadline};

/// Creates payload jobs that build blocks from Engine API payload attributes.
///
/// Each generated job delegates payload construction to the configured [`PayloadBuilder`] and
/// manages its execution, resolution, cancellation, and deadline.
#[derive(Debug)]
pub struct BlockPayloadJobGenerator<Client, Builder> {
    /// The client that can interact with the chain.
    client: Client,
    /// How to spawn building tasks
    executor: Runtime,
    /// The type responsible for building payloads.
    ///
    /// See [`PayloadBuilder`]
    builder: Builder,
    /// Whether to ensure only one payload is being processed at a time
    ensure_only_one_payload: bool,
    /// The last payload being processed
    last_payload: Arc<Mutex<CancellationToken>>,
    /// Calculates and constructs payload job deadline timers.
    deadline: PayloadJobDeadline,
    /// Stored `cached_reads` for new payload jobs.
    pre_cached: Option<PrecachedState>,
}

// === impl BlockPayloadJobGenerator ===

impl<Client, Builder> BlockPayloadJobGenerator<Client, Builder> {
    /// Creates a new [`BlockPayloadJobGenerator`] with the given config and custom
    /// [`PayloadBuilder`]
    pub fn with_builder(
        client: Client,
        executor: Runtime,
        builder: Builder,
        ensure_only_one_payload: bool,
        extra_block_deadline: std::time::Duration,
    ) -> Self {
        Self {
            client,
            executor,
            builder,
            ensure_only_one_payload,
            last_payload: Arc::new(Mutex::new(CancellationToken::new())),
            deadline: PayloadJobDeadline::new(extra_block_deadline),
            pre_cached: None,
        }
    }

    /// Returns the pre-cached reads for the given parent header if it matches the cached state's
    /// block.
    fn maybe_pre_cached(&self, parent: B256) -> Option<CachedReads> {
        self.pre_cached.as_ref().filter(|pc| pc.block == parent).map(|pc| pc.cached.clone())
    }
}

impl<Client, Builder> PayloadJobGenerator for BlockPayloadJobGenerator<Client, Builder>
where
    Client: StateProviderFactory
        + BlockReaderIdExt<Header = HeaderForPayload<Builder::BuiltPayload>>
        + Clone
        + Unpin
        + 'static,
    Builder: PayloadBuilder + Unpin + 'static,
    Builder::Attributes: Unpin + Clone,
    Builder::BuiltPayload: Unpin + Clone,
{
    type Job = BlockPayloadJob<Builder>;

    /// This is invoked when the node receives payload attributes from the beacon node via
    /// `engine_forkchoiceUpdatedVX`
    fn new_payload_job(
        &self,
        input: BuildNewPayload<<Builder as PayloadBuilder>::Attributes>,
        id: PayloadId,
    ) -> Result<Self::Job, PayloadBuilderError> {
        let cancel_token = if self.ensure_only_one_payload {
            // Cancel existing payload
            {
                let last_payload = self.last_payload.lock();
                last_payload.cancel();
            }

            // Create and set new cancellation token with a fresh lock
            let cancel_token = CancellationToken::new();
            {
                let mut last_payload = self.last_payload.lock();
                *last_payload = cancel_token.clone();
            }
            cancel_token
        } else {
            CancellationToken::new()
        };

        let parent_header = if input.parent_hash.is_zero() {
            // use latest block if parent is zero: genesis block
            self.client
                .latest_header()?
                .ok_or_else(|| PayloadBuilderError::MissingParentBlock(input.parent_hash))?
        } else {
            self.client
                .sealed_header_by_hash(input.parent_hash)?
                .ok_or_else(|| PayloadBuilderError::MissingParentBlock(input.parent_hash))?
        };

        info!("Spawn block building job");

        let deadline = self.deadline.sleep(input.attributes.timestamp());

        // Extract hash before moving parent_header into Arc to avoid cloning
        let parent_hash = parent_header.hash();
        let execution_cache = input.cache;
        let config = PayloadConfig::new(Arc::new(parent_header), input.attributes, id);

        // Create shared mutex for synchronizing cancellation with payload publishing
        let publish_guard = Arc::new(Mutex::new(()));

        let mut job = BlockPayloadJob {
            executor: self.executor.clone(),
            builder: self.builder.clone(),
            config,
            payload_rx: None,
            build_error: Arc::new(Mutex::new(None)),
            cancel: cancel_token,
            publish_guard,
            deadline,
            cached_reads: self.maybe_pre_cached(parent_hash),
            execution_cache,
        };

        job.spawn_build_job();

        Ok(job)
    }

    fn on_new_state<N: NodePrimitives>(&mut self, new_state: CanonStateNotification<N>) {
        let mut cached = CachedReads::default();

        // extract the state from the notification and put it into the cache
        let committed = new_state.committed();
        let new_execution_outcome = committed.execution_outcome();
        for (addr, acc) in new_execution_outcome.bundle_accounts_iter() {
            if let Some(info) = acc.info.clone() {
                // we want to pre-cache existing accounts and their storage
                // this only includes changed accounts and storage but is better than nothing
                let storage =
                    acc.storage.iter().map(|(key, slot)| (*key, slot.present_value)).collect();
                cached.insert_account(addr, info, storage);
            }
        }

        self.pre_cached = Some(PrecachedState { block: committed.tip().hash(), cached });
    }
}

use std::{
    pin::Pin,
    task::{Context, Poll},
};

/// A [`PayloadJob`] that manages the asynchronous construction of a block payload.
///
/// [`PayloadJobGenerator::new_payload_job`] creates this job when the
/// [`PayloadBuilderService`](reth_payload_builder::PayloadBuilderService) receives new payload
/// attributes from an Engine API forkchoice update. The service polls the job to enforce its
/// deadline and resolve payload requests, while the job runs its [`PayloadBuilder`] on a blocking
/// task and receives updated built payloads through a watch channel.
pub struct BlockPayloadJob<Builder>
where
    Builder: PayloadBuilder,
{
    /// The configuration for how the payload will be created.
    pub(crate) config: PayloadConfig<Builder::Attributes, HeaderForPayload<Builder::BuiltPayload>>,
    /// How to spawn building tasks
    pub(crate) executor: Runtime,
    /// The type responsible for building payloads.
    ///
    /// See [`PayloadBuilder`]
    pub(crate) builder: Builder,
    /// Receiver for the latest payload from the builder task.
    /// `None` until `spawn_build_job` is called; taken by `resolve_kind` into [`ResolvePayload`].
    pub(crate) payload_rx: Option<watch::Receiver<Option<Builder::BuiltPayload>>>,
    /// The error the build task last failed with, if any.
    ///
    /// The build task can only report failure by dropping `payload_rx`'s sender, which loses
    /// the underlying cause. This carries that cause over to [`ResolvePayload`] so its error
    /// reflects why the build task exited rather than a generic message.
    pub(crate) build_error: Arc<Mutex<Option<String>>>,
    /// Cancellation token for the running job
    pub(crate) cancel: CancellationToken,
    /// Mutex to synchronize cancellation with payload publishing.
    pub(crate) publish_guard: Arc<Mutex<()>>,
    pub(crate) deadline: Pin<Box<Sleep>>,
    /// Caches all disk reads for the state the new payloads build on
    ///
    /// This is used to avoid reading the same state over and over again when new attempts are
    /// triggered, because during the building process we'll repeatedly execute the transactions.
    pub(crate) cached_reads: Option<CachedReads>,
    /// Optional execution cache shared with the engine.
    pub(crate) execution_cache: Option<SavedCache>,
}

impl<Builder> std::fmt::Debug for BlockPayloadJob<Builder>
where
    Builder: PayloadBuilder,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockPayloadJob").finish_non_exhaustive()
    }
}

impl<Builder> PayloadJob for BlockPayloadJob<Builder>
where
    Builder: PayloadBuilder + Unpin + 'static,
    Builder::Attributes: Unpin + Clone,
    Builder::BuiltPayload: Unpin + Clone,
{
    type PayloadAttributes = Builder::Attributes;
    type ResolvePayloadFuture = ResolvePayload<Self::BuiltPayload>;
    type BuiltPayload = Builder::BuiltPayload;

    fn best_payload(&self) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        Err(PayloadBuilderError::Other("best_payload not supported; use resolve_kind".into()))
    }

    fn payload_attributes(&self) -> Result<Self::PayloadAttributes, PayloadBuilderError> {
        Ok(self.config.attributes.clone())
    }

    fn resolve_kind(
        &mut self,
        kind: PayloadKind,
    ) -> (Self::ResolvePayloadFuture, KeepPayloadJobAlive) {
        info!(kind = ?kind, "Resolve kind");

        // Acquire mutex before cancelling to synchronize with payload publishing.
        {
            let _guard = self.publish_guard.lock();
            self.cancel.cancel();
        }

        (
            ResolvePayload::new(self.payload_rx.take(), Arc::clone(&self.build_error)),
            KeepPayloadJobAlive::No,
        )
    }
}

/// Build arguments
#[derive(Debug)]
pub struct BuildArguments<Attributes, Payload: BuiltPayload> {
    /// Previously cached disk reads
    pub cached_reads: CachedReads,
    /// Optional execution cache shared with the engine.
    pub execution_cache: Option<SavedCache>,
    /// How to configure the payload.
    pub config: PayloadConfig<Attributes, HeaderTy<Payload::Primitives>>,
    /// A marker that can be used to cancel the job.
    pub cancel: CancellationToken,
    /// Mutex to synchronize cancellation with payload publishing.
    pub publish_guard: Arc<Mutex<()>>,
}

/// A [`PayloadJob`] is a future that's being polled by the `PayloadBuilderService`
impl<Builder> BlockPayloadJob<Builder>
where
    Builder: PayloadBuilder + Unpin + 'static,
    Builder::Attributes: Unpin + Clone,
    Builder::BuiltPayload: Unpin + Clone,
{
    /// Spawns a blocking task that builds the next payload using the current configuration.
    pub fn spawn_build_job(&mut self) {
        let builder = self.builder.clone();
        let payload_config = self.config.clone();
        let cancel = self.cancel.clone();
        let publish_guard = Arc::clone(&self.publish_guard);
        let build_error = Arc::clone(&self.build_error);

        let (watch_tx, watch_rx) = watch::channel(None);
        self.payload_rx = Some(watch_rx);
        let cached_reads = self.cached_reads.take().unwrap_or_default();
        let execution_cache = self.execution_cache.take();
        self.executor.spawn_blocking_task(Box::pin(async move {
            let args = BuildArguments {
                cached_reads,
                execution_cache,
                config: payload_config,
                cancel,
                publish_guard,
            };
            if let Err(e) = builder.try_build(args, &watch_tx).await {
                warn!(error = %e, "Payload build task failed");
                *build_error.lock() = Some(e.to_string());
            }
            // watch_tx is dropped here, after any failure cause has been recorded above,
            // so ResolvePayload always observes the cause before the sender's drop.
        }));
    }
}

/// A [`PayloadJob`] is a future that's being polled by the `PayloadBuilderService`
impl<Builder> Future for BlockPayloadJob<Builder>
where
    Builder: PayloadBuilder + Unpin + 'static,
    Builder::Attributes: Unpin + Clone,
    Builder::BuiltPayload: Unpin + Clone,
{
    type Output = Result<(), PayloadBuilderError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        trace!("Polling job");
        let this = self.get_mut();

        // Check if deadline is reached
        if this.deadline.as_mut().poll(cx).is_ready() {
            this.cancel.cancel();
            debug!("Deadline reached");
            return Poll::Ready(Ok(()));
        }

        // If cancelled via resolve_kind()
        if this.cancel.is_cancelled() {
            debug!("Job cancelled");
            return Poll::Ready(Ok(()));
        }

        Poll::Pending
    }
}

/// A future that resolves with the latest payload produced by the builder task.
///
/// Waits for the first non-`None` value from the watch channel.  If the sender
/// is dropped before any value is sent (build task failed or panicked), the
/// future resolves with an error describing why the build task exited, falling
/// back to a generic message if no cause was recorded.
pub struct ResolvePayload<T> {
    future: futures::future::BoxFuture<'static, Result<T, PayloadBuilderError>>,
}

impl<T> std::fmt::Debug for ResolvePayload<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvePayload").finish_non_exhaustive()
    }
}

impl<T: Clone + Send + Sync + 'static> ResolvePayload<T> {
    fn new(
        payload_rx: Option<watch::Receiver<Option<T>>>,
        build_error: Arc<Mutex<Option<String>>>,
    ) -> Self {
        let future = async move {
            let Some(mut rx) = payload_rx else {
                return Err(PayloadBuilderError::Other("payload receiver missing".into()));
            };

            let payload = rx.wait_for(Option::is_some).await.map_err(|_| {
                build_error.lock().take().map_or(
                    PayloadBuilderError::Other("builder exited before producing payload".into()),
                    |err| PayloadBuilderError::Other(err.into()),
                )
            })?;

            Ok(payload.as_ref().expect("checked is_some by wait_for predicate").clone())
        }
        .boxed();

        Self { future }
    }
}

impl<T> Future for ResolvePayload<T> {
    type Output = Result<T, PayloadBuilderError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().future.as_mut().poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::eip7685::Requests;
    use alloy_primitives::U256;
    use base_common_consensus::BasePrimitives;
    use base_execution_payload_builder::{BasePayloadBuilderAttributes, PayloadPrimitives};
    use rand::rng;
    use reth_execution_cache::{ExecutionCache, SavedCache};
    use reth_node_api::{BuiltPayloadExecutedBlock, NodePrimitives};
    use reth_primitives_traits::SealedBlock;
    use reth_provider::test_utils::MockEthProvider;
    use reth_tasks::Runtime;
    use reth_testing_utils::generators::{BlockRangeParams, random_block_range};
    use tokio::time::{Duration, sleep, timeout};

    use super::*;

    #[derive(Debug, Clone)]
    struct MockBuilder<N> {
        events: Arc<Mutex<Vec<BlockEvent>>>,
        _marker: std::marker::PhantomData<N>,
    }

    impl<N> MockBuilder<N> {
        fn new() -> Self {
            Self { events: Arc::new(Mutex::new(vec![])), _marker: std::marker::PhantomData }
        }

        fn new_event(&self, event: BlockEvent) {
            let mut events = self.events.lock();
            events.push(event);
        }

        fn get_events(&self) -> Vec<BlockEvent> {
            let mut events = self.events.lock();
            std::mem::take(&mut *events)
        }
    }

    #[derive(Clone, Debug, Default)]
    struct MockPayload {
        block: SealedBlock<<BasePrimitives as NodePrimitives>::Block>,
        fees: U256,
        requests: Option<Requests>,
    }

    impl BuiltPayload for MockPayload {
        type Primitives = BasePrimitives;

        fn block(&self) -> &SealedBlock<<Self::Primitives as NodePrimitives>::Block> {
            &self.block
        }

        /// Returns the fees collected for the built block
        fn fees(&self) -> U256 {
            self.fees
        }

        /// Returns the entire execution data for the built block, if available.
        fn executed_block(&self) -> Option<BuiltPayloadExecutedBlock<Self::Primitives>> {
            None
        }

        /// Returns the EIP-7865 requests for the payload if any.
        fn requests(&self) -> Option<Requests> {
            self.requests.clone()
        }
    }

    #[derive(Debug, PartialEq, Clone)]
    enum BlockEvent {
        Started(Option<B256>),
        Cancelled,
    }

    #[async_trait::async_trait]
    impl<N> PayloadBuilder for MockBuilder<N>
    where
        N: PayloadPrimitives,
    {
        type Attributes = BasePayloadBuilderAttributes<N::SignedTx>;
        type BuiltPayload = MockPayload;

        async fn try_build(
            &self,
            args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
            _payload_tx: &watch::Sender<Option<Self::BuiltPayload>>,
        ) -> Result<(), PayloadBuilderError> {
            self.new_event(BlockEvent::Started(
                args.execution_cache.as_ref().map(SavedCache::executed_block_hash),
            ));

            loop {
                if args.cancel.is_cancelled() {
                    self.new_event(BlockEvent::Cancelled);
                    return Ok(());
                }

                // Small sleep to prevent tight loop
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[tokio::test]
    async fn test_payload_generator() -> eyre::Result<()> {
        let mut rng = rng();

        let client = MockEthProvider::default();
        let executor = Runtime::test();
        let builder = MockBuilder::<BasePrimitives>::new();

        let (start, count) = (1, 10);
        let blocks = random_block_range(
            &mut rng,
            start..=start + count - 1,
            BlockRangeParams { tx_count: 0..2, ..Default::default() },
        );

        client.extend_blocks(blocks.iter().cloned().map(|b| (b.hash(), b.unseal())));

        let generator = BlockPayloadJobGenerator::with_builder(
            client.clone(),
            executor,
            builder.clone(),
            false,
            std::time::Duration::from_secs(1),
        );

        // this is not nice but necessary
        let mut attr = BasePayloadBuilderAttributes::default();
        attr.payload_attributes.parent = client.latest_header()?.unwrap().hash();

        {
            let parent_hash = attr.payload_attributes.parent;
            let input = BuildNewPayload {
                attributes: attr.clone(),
                parent_hash,
                cache: None,
                state_root_handle: None,
            };
            let job = generator.new_payload_job(input, attr.payload_id(&parent_hash))?;
            let _ = job.await;

            // you need to give one second for the job to be dropped and cancelled the internal job
            tokio::time::sleep(Duration::from_secs(1)).await;

            let events = builder.get_events();
            assert_eq!(events, vec![BlockEvent::Started(None), BlockEvent::Cancelled]);
        }

        {
            // job resolve triggers cancellations from the build task
            let parent_hash = attr.payload_attributes.parent;
            let cache = SavedCache::new(parent_hash, ExecutionCache::new(1_000));
            let input = BuildNewPayload {
                attributes: attr.clone(),
                parent_hash,
                cache: Some(cache.clone()),
                state_root_handle: None,
            };
            let mut job = generator.new_payload_job(input, attr.payload_id(&parent_hash))?;
            let _ = job.resolve();
            let _ = job.await;

            tokio::time::sleep(Duration::from_secs(1)).await;

            let events = builder.get_events();
            assert_eq!(events, vec![BlockEvent::Started(Some(parent_hash)), BlockEvent::Cancelled]);
            assert!(cache.is_available(), "builder must release the execution cache after exit");
        }

        Ok(())
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct MockPayloadValue(u64);

    #[tokio::test]
    async fn test_resolve_payload_waits_for_first_value() {
        let (tx, rx) = watch::channel::<Option<MockPayloadValue>>(None);
        tokio::spawn(async move {
            sleep(Duration::from_millis(50)).await;
            tx.send_replace(Some(MockPayloadValue(7)));
        });
        let payload = timeout(
            Duration::from_secs(1),
            ResolvePayload::new(Some(rx), Arc::new(Mutex::new(None))),
        )
        .await
        .expect("timed out")
        .expect("missing payload");
        assert_eq!(payload, MockPayloadValue(7));
    }

    #[tokio::test]
    async fn test_resolve_payload_immediate_value() {
        let (tx, rx) = watch::channel::<Option<MockPayloadValue>>(None);
        tx.send_replace(Some(MockPayloadValue(3)));
        let payload = ResolvePayload::new(Some(rx), Arc::new(Mutex::new(None)))
            .await
            .expect("should resolve immediately");
        assert_eq!(payload, MockPayloadValue(3));
    }

    #[tokio::test]
    async fn test_resolve_payload_errors_when_sender_dropped() {
        let (tx, rx) = watch::channel::<Option<MockPayloadValue>>(None);
        drop(tx);
        ResolvePayload::new(Some(rx), Arc::new(Mutex::new(None)))
            .await
            .expect_err("should error when sender dropped without value");
    }

    #[tokio::test]
    async fn test_resolve_payload_propagates_build_error() {
        let (tx, rx) = watch::channel::<Option<MockPayloadValue>>(None);
        let build_error = Arc::new(Mutex::new(None));
        *build_error.lock() = Some("state root computation failed".to_string());
        drop(tx);
        let err = ResolvePayload::new(Some(rx), build_error)
            .await
            .expect_err("should error when sender dropped without value");
        assert!(err.to_string().contains("state root computation failed"));
    }

    #[tokio::test]
    async fn test_resolve_payload_errors_when_rx_missing() {
        ResolvePayload::<MockPayloadValue>::new(None, Arc::new(Mutex::new(None)))
            .await
            .expect_err("should error when receiver is missing");
    }
}
