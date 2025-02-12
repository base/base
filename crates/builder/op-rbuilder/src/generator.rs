use futures_util::Future;
use futures_util::FutureExt;
use reth::providers::BlockReaderIdExt;
use reth::{
    builder::{components::PayloadServiceBuilder, node::FullNodeTypes, BuilderContext},
    payload::PayloadBuilderHandle,
    providers::CanonStateSubscriptions,
    transaction_pool::TransactionPool,
};
use reth::{providers::StateProviderFactory, tasks::TaskSpawner};
use reth_basic_payload_builder::{BasicPayloadJobGeneratorConfig, PayloadConfig};
use reth_node_api::NodeTypesWithEngine;
use reth_node_api::PayloadBuilderAttributes;
use reth_node_api::PayloadKind;
use reth_node_api::PayloadTypes;
use reth_node_api::TxTy;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::OpEngineTypes;
use reth_optimism_payload_builder::OpPayloadBuilderAttributes;
use reth_optimism_primitives::OpPrimitives;
use reth_payload_builder::PayloadBuilderService;
use reth_payload_builder::PayloadJobGenerator;
use reth_payload_builder::{KeepPayloadJobAlive, PayloadBuilderError, PayloadJob};
use reth_payload_primitives::BuiltPayload;
use reth_revm::cached::CachedReads;
use reth_transaction_pool::PoolTransaction;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::oneshot;
use tokio::sync::Notify;
use tokio::time::Duration;
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct CustomOpPayloadBuilder<Builder> {
    builder: Builder,
}

impl<Builder> CustomOpPayloadBuilder<Builder> {
    pub fn new(builder: Builder) -> Self {
        Self { builder }
    }
}

impl<Node, Pool, Builder> PayloadServiceBuilder<Node, Pool> for CustomOpPayloadBuilder<Builder>
where
    Node: FullNodeTypes<
        Types: NodeTypesWithEngine<
            Engine = OpEngineTypes,
            ChainSpec = OpChainSpec,
            Primitives = OpPrimitives,
        >,
    >,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
    Builder: PayloadBuilder<
            Pool,
            <Node as FullNodeTypes>::Provider,
            Attributes = OpPayloadBuilderAttributes,
        > + Unpin
        + 'static,
    <Builder as PayloadBuilder<Pool, Node::Provider>>::BuiltPayload:
        Into<<OpEngineTypes as PayloadTypes>::BuiltPayload> + Unpin + Clone,
{
    async fn spawn_payload_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<<Node::Types as NodeTypesWithEngine>::Engine>> {
        tracing::info!("Spawning a custom payload builder");
        let payload_job_config = BasicPayloadJobGeneratorConfig::default();

        let payload_generator = BlockPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            pool,
            ctx.task_executor().clone(),
            payload_job_config,
            self.builder,
            false,
        );

        let (payload_service, payload_builder) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor()
            .spawn_critical("custom payload builder service", Box::pin(payload_service));

        tracing::info!("Custom payload service started");

        Ok(payload_builder)
    }
}

/// A trait for building payloads that encapsulate Ethereum transactions.
///
/// This trait provides the `try_build` method to construct a transaction payload
/// using `BuildArguments`. It returns a `Result` indicating success or a
/// `PayloadBuilderError` if building fails.
///
/// Generic parameters `Pool` and `Client` represent the transaction pool and
/// Ethereum client types.
pub trait PayloadBuilder<Pool, Client>: Send + Sync + Clone {
    /// The payload attributes type to accept for building.
    type Attributes: PayloadBuilderAttributes;
    /// The type of the built payload.
    type BuiltPayload: BuiltPayload;

    /// Tries to build a transaction payload using provided arguments.
    ///
    /// Constructs a transaction payload based on the given arguments,
    /// returning a `Result` indicating success or an error if building fails.
    ///
    /// # Arguments
    ///
    /// - `args`: Build arguments containing necessary components.
    ///
    /// # Returns
    ///
    /// A `Result` indicating the build outcome or an error.
    fn try_build(
        &self,
        args: BuildArguments<Pool, Client, Self::Attributes>,
        best_payload: BlockCell<Self::BuiltPayload>,
    ) -> Result<(), PayloadBuilderError>;
}

/// The generator type that creates new jobs that builds empty blocks.
#[derive(Debug)]
pub struct BlockPayloadJobGenerator<Client, Pool, Tasks, Builder> {
    /// The client that can interact with the chain.
    client: Client,
    /// txpool
    pool: Pool,
    /// How to spawn building tasks
    executor: Tasks,
    /// The configuration for the job generator.
    _config: BasicPayloadJobGeneratorConfig,
    /// The type responsible for building payloads.
    ///
    /// See [PayloadBuilder]
    builder: Builder,
    /// Whether to ensure only one payload is being processed at a time
    ensure_only_one_payload: bool,
    /// The last payload being processed
    last_payload: Arc<Mutex<CancellationToken>>,
}

// === impl EmptyBlockPayloadJobGenerator ===

impl<Client, Pool, Tasks, Builder> BlockPayloadJobGenerator<Client, Pool, Tasks, Builder> {
    /// Creates a new [EmptyBlockPayloadJobGenerator] with the given config and custom
    /// [PayloadBuilder]
    pub fn with_builder(
        client: Client,
        pool: Pool,
        executor: Tasks,
        config: BasicPayloadJobGeneratorConfig,
        builder: Builder,
        ensure_only_one_payload: bool,
    ) -> Self {
        Self {
            client,
            pool,
            executor,
            _config: config,
            builder,
            ensure_only_one_payload,
            last_payload: Arc::new(Mutex::new(CancellationToken::new())),
        }
    }
}

impl<Client, Pool, Tasks, Builder> PayloadJobGenerator
    for BlockPayloadJobGenerator<Client, Pool, Tasks, Builder>
where
    Client: StateProviderFactory
        + BlockReaderIdExt<Header = alloy_consensus::Header>
        + Clone
        + Unpin
        + 'static,
    Pool: TransactionPool + Unpin + 'static,
    Tasks: TaskSpawner + Clone + Unpin + 'static,
    Builder: PayloadBuilder<Pool, Client> + Unpin + 'static,
    <Builder as PayloadBuilder<Pool, Client>>::Attributes: Unpin + Clone,
    <Builder as PayloadBuilder<Pool, Client>>::BuiltPayload: Unpin + Clone,
{
    type Job = BlockPayloadJob<Client, Pool, Tasks, Builder>;

    /// This is invoked when the node receives payload attributes from the beacon node via
    /// `engine_forkchoiceUpdatedV1`
    fn new_payload_job(
        &self,
        attributes: <Builder as PayloadBuilder<Pool, Client>>::Attributes,
    ) -> Result<Self::Job, PayloadBuilderError> {
        let cancel_token = if self.ensure_only_one_payload {
            // Cancel existing payload
            {
                let last_payload = self.last_payload.lock().unwrap();
                last_payload.cancel();
            }

            // Create and set new cancellation token with a fresh lock
            let cancel_token = CancellationToken::new();
            {
                let mut last_payload = self.last_payload.lock().unwrap();
                *last_payload = cancel_token.clone();
            }
            cancel_token
        } else {
            CancellationToken::new()
        };

        let parent_header = if attributes.parent().is_zero() {
            // use latest block if parent is zero: genesis block
            self.client
                .latest_header()?
                .ok_or_else(|| PayloadBuilderError::MissingParentBlock(attributes.parent()))?
        } else {
            self.client
                .sealed_header_by_hash(attributes.parent())?
                .ok_or_else(|| PayloadBuilderError::MissingParentBlock(attributes.parent()))?
        };

        info!("Spawn block building job");

        // The deadline is critical for payload availability. If we reach the deadline,
        // the payload job stops and cannot be queried again. With tight deadlines close
        // to the block number, we risk reaching the deadline before the node queries the payload.
        //
        // Adding 0.5 seconds as wiggle room since block times are shorter here.
        // TODO: A better long-term solution would be to implement cancellation logic
        // that cancels existing jobs when receiving new block building requests.
        let deadline = job_deadline(attributes.timestamp()) + Duration::from_millis(500);

        let deadline = Box::pin(tokio::time::sleep(deadline));
        let config = PayloadConfig::new(Arc::new(parent_header.clone()), attributes);

        let mut job = BlockPayloadJob {
            client: self.client.clone(),
            pool: self.pool.clone(),
            executor: self.executor.clone(),
            builder: self.builder.clone(),
            config,
            cell: BlockCell::new(),
            cancel: cancel_token,
            deadline,
            build_complete: None,
        };

        job.spawn_build_job();

        Ok(job)
    }
}

use std::{
    pin::Pin,
    task::{Context, Poll},
};

/// A [PayloadJob] that builds empty blocks.
pub struct BlockPayloadJob<Client, Pool, Tasks, Builder>
where
    Builder: PayloadBuilder<Pool, Client>,
{
    /// The configuration for how the payload will be created.
    pub(crate) config: PayloadConfig<Builder::Attributes>,
    /// The client that can interact with the chain.
    pub(crate) client: Client,
    /// The transaction pool.
    pub(crate) pool: Pool,
    /// How to spawn building tasks
    pub(crate) executor: Tasks,
    /// The type responsible for building payloads.
    ///
    /// See [PayloadBuilder]
    pub(crate) builder: Builder,
    /// The cell that holds the built payload.
    pub(crate) cell: BlockCell<Builder::BuiltPayload>,
    /// Cancellation token for the running job
    pub(crate) cancel: CancellationToken,
    pub(crate) deadline: Pin<Box<Sleep>>, // Add deadline
    pub(crate) build_complete: Option<oneshot::Receiver<Result<(), PayloadBuilderError>>>,
}

impl<Client, Pool, Tasks, Builder> PayloadJob for BlockPayloadJob<Client, Pool, Tasks, Builder>
where
    Client: StateProviderFactory + Clone + Unpin + 'static,
    Pool: TransactionPool + Unpin + 'static,
    Tasks: TaskSpawner + Clone + 'static,
    Builder: PayloadBuilder<Pool, Client> + Unpin + 'static,
    <Builder as PayloadBuilder<Pool, Client>>::Attributes: Unpin + Clone,
    <Builder as PayloadBuilder<Pool, Client>>::BuiltPayload: Unpin + Clone,
{
    type PayloadAttributes = Builder::Attributes;
    type ResolvePayloadFuture = ResolvePayload<Self::BuiltPayload>;
    type BuiltPayload = Builder::BuiltPayload;

    fn best_payload(&self) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        unimplemented!()
    }

    fn payload_attributes(&self) -> Result<Self::PayloadAttributes, PayloadBuilderError> {
        Ok(self.config.attributes.clone())
    }

    fn resolve_kind(
        &mut self,
        kind: PayloadKind,
    ) -> (Self::ResolvePayloadFuture, KeepPayloadJobAlive) {
        tracing::info!("Resolve kind {:?}", kind);

        // check if self.cell has a payload
        self.cancel.cancel();

        let resolve_future = ResolvePayload::new(self.cell.wait_for_value());
        (resolve_future, KeepPayloadJobAlive::No)
    }
}

pub struct BuildArguments<Pool, Client, Attributes> {
    /// How to interact with the chain.
    pub client: Client,
    /// The transaction pool.
    ///
    /// Or the type that provides the transactions to build the payload.
    pub pool: Pool,
    /// Previously cached disk reads
    pub cached_reads: CachedReads,
    /// How to configure the payload.
    pub config: PayloadConfig<Attributes>,
    /// A marker that can be used to cancel the job.
    pub cancel: CancellationToken,
}

/// A [PayloadJob] is a future that's being polled by the `PayloadBuilderService`
impl<Client, Pool, Tasks, Builder> BlockPayloadJob<Client, Pool, Tasks, Builder>
where
    Client: StateProviderFactory + Clone + Unpin + 'static,
    Pool: TransactionPool + Unpin + 'static,
    Tasks: TaskSpawner + Clone + 'static,
    Builder: PayloadBuilder<Pool, Client> + Unpin + 'static,
    <Builder as PayloadBuilder<Pool, Client>>::Attributes: Unpin + Clone,
    <Builder as PayloadBuilder<Pool, Client>>::BuiltPayload: Unpin + Clone,
{
    pub fn spawn_build_job(&mut self) {
        let builder = self.builder.clone();
        let client = self.client.clone();
        let pool = self.pool.clone();
        let payload_config = self.config.clone();
        let cell = self.cell.clone();
        let cancel = self.cancel.clone();

        let (tx, rx) = oneshot::channel();
        self.build_complete = Some(rx);

        self.executor.spawn_blocking(Box::pin(async move {
            let args = BuildArguments {
                client,
                pool,
                cached_reads: Default::default(),
                config: payload_config,
                cancel,
            };

            let result = builder.try_build(args, cell);
            let _ = tx.send(result);
        }));
    }
}

/// A [PayloadJob] is a a future that's being polled by the `PayloadBuilderService`
impl<Client, Pool, Tasks, Builder> Future for BlockPayloadJob<Client, Pool, Tasks, Builder>
where
    Client: StateProviderFactory + Clone + Unpin + 'static,
    Pool: TransactionPool + Unpin + 'static,
    Tasks: TaskSpawner + Clone + 'static,
    Builder: PayloadBuilder<Pool, Client> + Unpin + 'static,
    <Builder as PayloadBuilder<Pool, Client>>::Attributes: Unpin + Clone,
    <Builder as PayloadBuilder<Pool, Client>>::BuiltPayload: Unpin + Clone,
{
    type Output = Result<(), PayloadBuilderError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        tracing::trace!("Polling job");
        let this = self.get_mut();

        // Check if deadline is reached
        if this.deadline.as_mut().poll(cx).is_ready() {
            this.cancel.cancel();
            tracing::debug!("Deadline reached");
            return Poll::Ready(Ok(()));
        }

        // If cancelled via resolve_kind()
        if this.cancel.is_cancelled() {
            tracing::debug!("Job cancelled");
            return Poll::Ready(Ok(()));
        }

        Poll::Pending
    }
}

// A future that resolves when a payload becomes available in the BlockCell
pub struct ResolvePayload<T> {
    future: WaitForValue<T>,
}

impl<T> ResolvePayload<T> {
    pub fn new(future: WaitForValue<T>) -> Self {
        Self { future }
    }
}

impl<T: Clone> Future for ResolvePayload<T> {
    type Output = Result<T, PayloadBuilderError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.get_mut().future.poll_unpin(cx) {
            Poll::Ready(value) => Poll::Ready(Ok(value)),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Clone)]
pub struct BlockCell<T> {
    inner: Arc<Mutex<Option<T>>>,
    notify: Arc<Notify>,
}

impl<T: Clone> BlockCell<T> {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn is_some(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.is_some()
    }

    pub fn set(&self, value: T) {
        let mut inner = self.inner.lock().unwrap();
        *inner = Some(value);
        self.notify.notify_one();
    }

    pub fn get(&self) -> Option<T> {
        let inner = self.inner.lock().unwrap();
        inner.clone()
    }

    // Return a future that resolves when value is set
    pub fn wait_for_value(&self) -> WaitForValue<T> {
        WaitForValue { cell: self.clone() }
    }
}

#[derive(Clone)]
// Future that resolves when a value is set in BlockCell
pub struct WaitForValue<T> {
    cell: BlockCell<T>,
}

impl<T: Clone> Future for WaitForValue<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(value) = self.cell.get() {
            Poll::Ready(value)
        } else {
            // Instead of register, we use notified() to get a future
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

impl<T: Clone> Default for BlockCell<T> {
    fn default() -> Self {
        Self::new()
    }
}

fn job_deadline(unix_timestamp_secs: u64) -> std::time::Duration {
    let unix_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Safe subtraction that handles the case where timestamp is in the past
    let duration_until = unix_timestamp_secs.saturating_sub(unix_now);

    if duration_until == 0 {
        // Enforce a minimum block time of 1 second by rounding up any duration less than 1 second
        Duration::from_secs(1)
    } else {
        Duration::from_secs(duration_until)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eips::eip7685::Requests;
    use alloy_primitives::U256;
    use rand::thread_rng;
    use reth::tasks::TokioTaskExecutor;
    use reth_chain_state::ExecutedBlock;
    use reth_node_api::NodePrimitives;
    use reth_optimism_payload_builder::payload::OpPayloadBuilderAttributes;
    use reth_optimism_primitives::OpPrimitives;
    use reth_primitives::SealedBlockFor;
    use reth_provider::test_utils::MockEthProvider;
    use reth_testing_utils::generators::{random_block_range, BlockRangeParams};
    use reth_transaction_pool::noop::NoopTransactionPool;
    use tokio::task;
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn test_block_cell_wait_for_value() {
        let cell = BlockCell::new();

        // Spawn a task that will set the value after a delay
        let cell_clone = cell.clone();
        task::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            cell_clone.set(42);
        });

        // Wait for the value and verify
        let wait_future = cell.wait_for_value();
        let result = wait_future.await;
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_block_cell_immediate_value() {
        let cell = BlockCell::new();
        cell.set(42);

        // Value should be immediately available
        let wait_future = cell.wait_for_value();
        let result = wait_future.await;
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_block_cell_multiple_waiters() {
        let cell = BlockCell::new();

        // Spawn multiple waiters
        let wait1 = task::spawn({
            let cell = cell.clone();
            async move { cell.wait_for_value().await }
        });

        let wait2 = task::spawn({
            let cell = cell.clone();
            async move { cell.wait_for_value().await }
        });

        // Set value after a delay
        sleep(Duration::from_millis(100)).await;
        cell.set(42);

        // All waiters should receive the value
        assert_eq!(wait1.await.unwrap(), 42);
        assert_eq!(wait2.await.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_block_cell_update_value() {
        let cell = BlockCell::new();

        // Set initial value
        cell.set(42);

        // Set new value
        cell.set(43);

        // Waiter should get the latest value
        let result = cell.wait_for_value().await;
        assert_eq!(result, 43);
    }

    #[derive(Debug, Clone)]
    struct MockBuilder {
        events: Arc<Mutex<Vec<BlockEvent>>>,
    }

    impl MockBuilder {
        fn new() -> Self {
            Self {
                events: Arc::new(Mutex::new(vec![])),
            }
        }

        fn new_event(&self, event: BlockEvent) {
            let mut events = self.events.lock().unwrap();
            events.push(event);
        }

        fn get_events(&self) -> Vec<BlockEvent> {
            let mut events = self.events.lock().unwrap();
            std::mem::take(&mut *events)
        }
    }

    #[derive(Clone, Debug, Default)]
    struct MockPayload;

    impl BuiltPayload for MockPayload {
        type Primitives = OpPrimitives;

        fn block(&self) -> &SealedBlockFor<<Self::Primitives as NodePrimitives>::Block> {
            unimplemented!()
        }

        /// Returns the fees collected for the built block
        fn fees(&self) -> U256 {
            unimplemented!()
        }

        /// Returns the entire execution data for the built block, if available.
        fn executed_block(&self) -> Option<ExecutedBlock<Self::Primitives>> {
            None
        }

        /// Returns the EIP-7865 requests for the payload if any.
        fn requests(&self) -> Option<Requests> {
            unimplemented!()
        }
    }

    #[derive(Debug, PartialEq, Clone)]
    enum BlockEvent {
        Started,
        Cancelled,
    }

    impl<Pool, Client> PayloadBuilder<Pool, Client> for MockBuilder {
        type Attributes = OpPayloadBuilderAttributes;
        type BuiltPayload = MockPayload;

        fn try_build(
            &self,
            args: BuildArguments<Pool, Client, Self::Attributes>,
            _best_payload: BlockCell<Self::BuiltPayload>,
        ) -> Result<(), PayloadBuilderError> {
            self.new_event(BlockEvent::Started);

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
    async fn test_job_deadline() {
        // Test future deadline
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let future_timestamp = now + Duration::from_secs(2);
        // 2 seconds from now
        let deadline = job_deadline(future_timestamp.as_secs());
        assert!(deadline <= Duration::from_secs(2));
        assert!(deadline > Duration::from_secs(0));

        // Test past deadline
        let past_timestamp = now - Duration::from_secs(10);
        let deadline = job_deadline(past_timestamp.as_secs());
        // Should default to 1 second when timestamp is in the past
        assert_eq!(deadline, Duration::from_secs(1));

        // Test current timestamp
        let deadline = job_deadline(now.as_secs());
        // Should use 1 second when timestamp is current
        assert_eq!(deadline, Duration::from_secs(1));
    }

    #[tokio::test]
    async fn test_payload_generator() -> eyre::Result<()> {
        let mut rng = thread_rng();

        let pool = NoopTransactionPool::default();
        let client = MockEthProvider::default();
        let executor = TokioTaskExecutor::default();
        let config = BasicPayloadJobGeneratorConfig::default();
        let builder = MockBuilder::new();

        let (start, count) = (1, 10);
        let blocks = random_block_range(
            &mut rng,
            start..=start + count - 1,
            BlockRangeParams {
                tx_count: 0..2,
                ..Default::default()
            },
        );

        client.extend_blocks(blocks.iter().cloned().map(|b| (b.hash(), b.unseal())));

        let generator = BlockPayloadJobGenerator::with_builder(
            client.clone(),
            pool,
            executor,
            config,
            builder.clone(),
            false,
        );

        // this is not nice but necessary
        let mut attr = OpPayloadBuilderAttributes::default();
        attr.payload_attributes.parent = client.latest_header()?.unwrap().hash();

        {
            let job = generator.new_payload_job(attr.clone())?;
            let _ = job.await;

            // you need to give one second for the job to be dropped and cancelled the internal job
            tokio::time::sleep(Duration::from_secs(1)).await;

            let events = builder.get_events();
            assert_eq!(events, vec![BlockEvent::Started, BlockEvent::Cancelled]);
        }

        {
            // job resolve triggers cancellations from the build task
            let mut job = generator.new_payload_job(attr.clone())?;
            let _ = job.resolve();
            let _ = job.await;

            tokio::time::sleep(Duration::from_secs(1)).await;

            let events = builder.get_events();
            assert_eq!(events, vec![BlockEvent::Started, BlockEvent::Cancelled]);
        }

        Ok(())
    }
}
