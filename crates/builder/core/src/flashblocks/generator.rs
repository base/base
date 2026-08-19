use std::sync::Arc;

use alloy_primitives::B256;
use futures::{Future, FutureExt};
use parking_lot::Mutex;
use reth_basic_payload_builder::{HeaderForPayload, PayloadConfig, PrecachedState};
use reth_execution_cache::SavedCache;
use reth_node_api::{NodePrimitives, PayloadKind};
use reth_payload_builder::{
    BuildNewPayload, KeepPayloadJobAlive, PayloadBuilderError, PayloadBuilderLease, PayloadId,
    PayloadJob, PayloadJobGenerator,
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
        let mut resources = input.resources;
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
            execution_cache: resources.take_execution_cache(),
            leases: resources.take_leases(),
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
    /// Lifecycle leases retained until the detached worker finishes using engine resources.
    pub(crate) leases: Vec<PayloadBuilderLease>,
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
    /// Lifecycle leases protecting resources loaned by the engine.
    pub leases: Vec<PayloadBuilderLease>,
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
        let leases = std::mem::take(&mut self.leases);
        self.executor.spawn_blocking_task(Box::pin(async move {
            let args = BuildArguments {
                cached_reads,
                execution_cache,
                leases,
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
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicBool, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use alloy_eips::eip7685::Requests;
    use alloy_primitives::U256;
    use base_common_consensus::{BasePrimitives, BaseTransactionSigned};
    use base_execution_payload_builder::{
        BaseBuiltPayload, BasePayloadBuilderAttributes, BasePayloadTypes, PayloadPrimitives,
    };
    use futures::stream;
    use rand::rng;
    use reth_execution_cache::{ExecutionCache, SavedCache};
    use reth_node_api::{BuiltPayloadExecutedBlock, NodePrimitives, PayloadKind};
    use reth_payload_builder::{
        PayloadBuilderHandle, PayloadBuilderResources, PayloadBuilderService,
    };
    use reth_primitives_traits::SealedBlock;
    use reth_provider::test_utils::MockEthProvider;
    use reth_tasks::Runtime;
    use reth_testing_utils::generators::{BlockRangeParams, random_block_range};
    use tokio::sync::Semaphore;
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

    type TestAttributes = BasePayloadBuilderAttributes<BaseTransactionSigned>;

    #[derive(Debug)]
    struct LeaseDropProbe {
        dropped: Arc<AtomicBool>,
    }

    impl Drop for LeaseDropProbe {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    #[derive(Debug)]
    struct BuildControl {
        started: Semaphore,
        complete: Semaphore,
        cancelled: Semaphore,
        finalizing: Semaphore,
        finish_finalizing: Semaphore,
        published: Semaphore,
    }

    impl Default for BuildControl {
        fn default() -> Self {
            Self {
                started: Semaphore::new(0),
                complete: Semaphore::new(0),
                cancelled: Semaphore::new(0),
                finalizing: Semaphore::new(0),
                finish_finalizing: Semaphore::new(0),
                published: Semaphore::new(0),
            }
        }
    }

    #[derive(Clone, Debug)]
    struct ControlledBuilder {
        controls: Arc<Mutex<VecDeque<Arc<BuildControl>>>>,
    }

    impl ControlledBuilder {
        fn new(controls: impl IntoIterator<Item = Arc<BuildControl>>) -> Self {
            Self { controls: Arc::new(Mutex::new(controls.into_iter().collect())) }
        }
    }

    #[async_trait::async_trait]
    impl PayloadBuilder for ControlledBuilder {
        type Attributes = TestAttributes;
        type BuiltPayload = BaseBuiltPayload;

        async fn try_build(
            &self,
            args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
            payload_tx: &watch::Sender<Option<Self::BuiltPayload>>,
        ) -> Result<(), PayloadBuilderError> {
            let control =
                self.controls.lock().pop_front().expect("missing controlled build configuration");
            control.started.add_permits(1);

            tokio::select! {
                permit = control.complete.acquire() => {
                    permit.expect("complete semaphore closed").forget();
                }
                _ = args.cancel.cancelled() => {
                    control.cancelled.add_permits(1);
                }
            }

            control.finalizing.add_permits(1);
            control
                .finish_finalizing
                .acquire()
                .await
                .expect("finalization semaphore closed")
                .forget();

            let payload_id = args.config.payload_id();
            drop(args);
            payload_tx.send_replace(Some(BaseBuiltPayload::new(
                payload_id,
                Arc::new(SealedBlock::default()),
                U256::ZERO,
                None,
                None,
            )));
            control.published.add_permits(1);
            Ok(())
        }
    }

    struct LeaseTest;

    impl LeaseTest {
        const TIMEOUT: Duration = Duration::from_secs(5);

        fn generator(
            builder: ControlledBuilder,
            ensure_only_one_payload: bool,
            extra_deadline: Duration,
        ) -> (BlockPayloadJobGenerator<MockEthProvider, ControlledBuilder>, TestAttributes)
        {
            let mut rng = rng();
            let client = MockEthProvider::default();
            let blocks = random_block_range(
                &mut rng,
                1..=1,
                BlockRangeParams { tx_count: 0..1, ..Default::default() },
            );
            client.extend_blocks(blocks.into_iter().map(|block| {
                let hash = block.hash();
                (hash, block.unseal())
            }));

            let mut attributes = TestAttributes::default();
            attributes.payload_attributes.parent =
                client.latest_header().expect("latest header query failed").unwrap().hash();
            attributes.payload_attributes.timestamp =
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 60;
            let generator = BlockPayloadJobGenerator::with_builder(
                client,
                Runtime::test(),
                builder,
                ensure_only_one_payload,
                extra_deadline,
            );
            (generator, attributes)
        }

        fn resources(dropped: &Arc<AtomicBool>) -> PayloadBuilderResources {
            PayloadBuilderResources::default().with_lease(PayloadBuilderLease::new(
                LeaseDropProbe { dropped: Arc::clone(dropped) },
            ))
        }

        fn input(
            attributes: TestAttributes,
            dropped: &Arc<AtomicBool>,
        ) -> BuildNewPayload<TestAttributes> {
            BuildNewPayload {
                parent_hash: attributes.payload_attributes.parent,
                attributes,
                resources: Self::resources(dropped),
            }
        }

        async fn wait(signal: &Semaphore) {
            timeout(Self::TIMEOUT, signal.acquire())
                .await
                .expect("timed out waiting for build signal")
                .expect("build signal semaphore closed")
                .forget();
        }

        async fn wait_until_removed(
            handle: &PayloadBuilderHandle<BasePayloadTypes>,
            id: PayloadId,
        ) {
            timeout(Self::TIMEOUT, async {
                while handle.payload_timestamp(id).await.is_some() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("timed out waiting for payload job removal");
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
                resources: PayloadBuilderResources::default(),
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
            let payload_builder_resources = PayloadBuilderResources::new(Some(cache.clone()), None);
            let input = BuildNewPayload {
                attributes: attr.clone(),
                parent_hash,
                resources: payload_builder_resources,
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

    #[tokio::test]
    async fn normal_get_payload_holds_worker_lease_until_publication() {
        let control = Arc::new(BuildControl::default());
        let dropped = Arc::new(AtomicBool::new(false));
        let (generator, attributes) = LeaseTest::generator(
            ControlledBuilder::new([Arc::clone(&control)]),
            false,
            Duration::from_secs(20),
        );
        let parent_hash = attributes.payload_attributes.parent;
        let mut job = generator
            .new_payload_job(
                LeaseTest::input(attributes.clone(), &dropped),
                attributes.payload_id(&parent_hash),
            )
            .expect("payload job creation failed");

        LeaseTest::wait(&control.started).await;
        assert!(!dropped.load(Ordering::Acquire), "worker must retain the lease while building");

        let (resolved, _) = job.resolve_kind(PayloadKind::Earliest);
        LeaseTest::wait(&control.cancelled).await;
        LeaseTest::wait(&control.finalizing).await;
        assert!(!dropped.load(Ordering::Acquire), "lease must cover cancellation finalization");

        control.finish_finalizing.add_permits(1);
        let payload = timeout(LeaseTest::TIMEOUT, resolved)
            .await
            .expect("timed out resolving payload")
            .expect("payload resolution failed");

        assert_eq!(payload.id(), attributes.payload_id(&parent_hash));
        assert!(
            dropped.load(Ordering::Acquire),
            "worker lease must drop before publication wakes the resolver"
        );
    }

    #[tokio::test]
    async fn late_get_payload_resolves_after_worker_releases_lease() {
        let control = Arc::new(BuildControl::default());
        let dropped = Arc::new(AtomicBool::new(false));
        let (generator, attributes) = LeaseTest::generator(
            ControlledBuilder::new([Arc::clone(&control)]),
            false,
            Duration::from_secs(20),
        );
        let parent_hash = attributes.payload_attributes.parent;
        let mut job = generator
            .new_payload_job(
                LeaseTest::input(attributes.clone(), &dropped),
                attributes.payload_id(&parent_hash),
            )
            .expect("payload job creation failed");

        LeaseTest::wait(&control.started).await;
        assert!(!dropped.load(Ordering::Acquire), "worker must retain the lease while building");
        control.complete.add_permits(1);
        LeaseTest::wait(&control.finalizing).await;
        assert!(!dropped.load(Ordering::Acquire), "lease must cover finalization");
        control.finish_finalizing.add_permits(1);
        LeaseTest::wait(&control.published).await;
        assert!(
            dropped.load(Ordering::Acquire),
            "completed worker must release its lease before a delayed request"
        );

        let (resolved, _) = job.resolve_kind(PayloadKind::Earliest);
        let payload = timeout(LeaseTest::TIMEOUT, resolved)
            .await
            .expect("timed out resolving delayed payload")
            .expect("delayed payload resolution failed");
        assert_eq!(payload.id(), attributes.payload_id(&parent_hash));
    }

    #[tokio::test]
    async fn cancelled_resolve_future_keeps_detached_worker_lease() {
        let control = Arc::new(BuildControl::default());
        let dropped = Arc::new(AtomicBool::new(false));
        let (generator, attributes) = LeaseTest::generator(
            ControlledBuilder::new([Arc::clone(&control)]),
            false,
            Duration::from_secs(20),
        );
        let (service, handle) = PayloadBuilderService::<_, _, BasePayloadTypes>::new(
            generator,
            stream::empty::<CanonStateNotification<BasePrimitives>>(),
        );
        let service_task = tokio::spawn(service);
        let id = handle
            .send_new_payload(LeaseTest::input(attributes, &dropped))
            .await
            .expect("payload service dropped response")
            .expect("payload job creation failed");

        LeaseTest::wait(&control.started).await;
        let resolve_handle = handle.clone();
        let resolve_task =
            tokio::spawn(
                async move { resolve_handle.resolve_kind(id, PayloadKind::Earliest).await },
            );
        LeaseTest::wait(&control.cancelled).await;
        LeaseTest::wait(&control.finalizing).await;
        LeaseTest::wait_until_removed(&handle, id).await;

        resolve_task.abort();
        let _ = resolve_task.await;
        tokio::task::yield_now().await;
        assert!(
            !dropped.load(Ordering::Acquire),
            "dropping the service resolver must not release an active worker lease"
        );

        control.finish_finalizing.add_permits(1);
        LeaseTest::wait(&control.published).await;
        assert!(
            dropped.load(Ordering::Acquire),
            "worker must release its lease after finalization"
        );
        service_task.abort();
    }

    #[tokio::test]
    async fn deadline_removal_keeps_detached_worker_lease() {
        let control = Arc::new(BuildControl::default());
        let dropped = Arc::new(AtomicBool::new(false));
        let (generator, mut attributes) = LeaseTest::generator(
            ControlledBuilder::new([Arc::clone(&control)]),
            false,
            Duration::ZERO,
        );
        attributes.payload_attributes.timestamp = 0;
        let (service, handle) = PayloadBuilderService::<_, _, BasePayloadTypes>::new(
            generator,
            stream::empty::<CanonStateNotification<BasePrimitives>>(),
        );
        let service_task = tokio::spawn(service);
        let id = handle
            .send_new_payload(LeaseTest::input(attributes, &dropped))
            .await
            .expect("payload service dropped response")
            .expect("payload job creation failed");

        LeaseTest::wait(&control.started).await;
        LeaseTest::wait(&control.cancelled).await;
        LeaseTest::wait(&control.finalizing).await;
        LeaseTest::wait_until_removed(&handle, id).await;
        assert!(
            !dropped.load(Ordering::Acquire),
            "deadline removal must not release a finalizing worker lease"
        );

        control.finish_finalizing.add_permits(1);
        LeaseTest::wait(&control.published).await;
        assert!(
            dropped.load(Ordering::Acquire),
            "worker must release its lease after finalization"
        );
        service_task.abort();
    }

    #[tokio::test]
    async fn replacement_keeps_cancelled_worker_lease() {
        let first_control = Arc::new(BuildControl::default());
        let second_control = Arc::new(BuildControl::default());
        let first_dropped = Arc::new(AtomicBool::new(false));
        let second_dropped = Arc::new(AtomicBool::new(false));
        let builder =
            ControlledBuilder::new([Arc::clone(&first_control), Arc::clone(&second_control)]);
        let (generator, first_attributes) =
            LeaseTest::generator(builder, true, Duration::from_secs(20));
        let mut second_attributes = first_attributes.clone();
        second_attributes.payload_attributes.timestamp += 1;
        let (service, handle) = PayloadBuilderService::<_, _, BasePayloadTypes>::new(
            generator,
            stream::empty::<CanonStateNotification<BasePrimitives>>(),
        );
        let service_task = tokio::spawn(service);

        let first_id = handle
            .send_new_payload(LeaseTest::input(first_attributes, &first_dropped))
            .await
            .expect("payload service dropped first response")
            .expect("first payload job creation failed");
        LeaseTest::wait(&first_control.started).await;

        let second_id = handle
            .send_new_payload(LeaseTest::input(second_attributes, &second_dropped))
            .await
            .expect("payload service dropped second response")
            .expect("second payload job creation failed");
        LeaseTest::wait(&second_control.started).await;
        LeaseTest::wait(&first_control.cancelled).await;
        LeaseTest::wait(&first_control.finalizing).await;
        LeaseTest::wait_until_removed(&handle, first_id).await;
        assert!(
            !first_dropped.load(Ordering::Acquire),
            "replacement must not release the cancelled worker's lease during finalization"
        );

        first_control.finish_finalizing.add_permits(1);
        LeaseTest::wait(&first_control.published).await;
        assert!(
            first_dropped.load(Ordering::Acquire),
            "replaced worker must release its lease after finalization"
        );

        second_control.complete.add_permits(1);
        LeaseTest::wait(&second_control.finalizing).await;
        second_control.finish_finalizing.add_permits(1);
        LeaseTest::wait(&second_control.published).await;
        assert!(
            !second_dropped.load(Ordering::Acquire),
            "service intentionally retains its lease for a delayed getPayload"
        );
        handle
            .resolve_kind(second_id, PayloadKind::Earliest)
            .await
            .expect("second payload job missing")
            .expect("second payload resolution failed");
        assert!(
            second_dropped.load(Ordering::Acquire),
            "service lease must release after delayed payload resolution"
        );
        service_task.abort();
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
