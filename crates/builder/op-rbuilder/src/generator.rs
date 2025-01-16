use alloy_eips::BlockNumberOrTag;
use alloy_primitives::Bytes;
use futures_util::Future;
use futures_util::FutureExt;
use reth::providers::{BlockReaderIdExt, BlockSource};
use reth::{
    providers::StateProviderFactory, tasks::TaskSpawner, transaction_pool::TransactionPool,
};
use reth_basic_payload_builder::{BasicPayloadJobGeneratorConfig, PayloadConfig};
use reth_basic_payload_builder::{BuildArguments, Cancelled};
use reth_node_api::PayloadBuilderAttributes;
use reth_node_api::PayloadKind;
use reth_payload_builder::PayloadJobGenerator;
use reth_payload_builder::{KeepPayloadJobAlive, PayloadBuilderError, PayloadJob};
use reth_payload_primitives::BuiltPayload;
use reth_primitives::SealedHeader;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tokio::sync::Notify;
use tokio::time::Duration;
use tokio::time::Sleep;
use tracing::info;

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
        args: BuildArguments<Pool, Client, Self::Attributes, Self::BuiltPayload>,
        best_payload: BlockCell<Self::BuiltPayload>,
    ) -> Result<(), PayloadBuilderError>;
}

/// The generator type that creates new jobs that builds empty blocks.
#[derive(Debug)]
pub struct EmptyBlockPayloadJobGenerator<Client, Pool, Tasks, Builder> {
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
}

// === impl EmptyBlockPayloadJobGenerator ===

impl<Client, Pool, Tasks, Builder> EmptyBlockPayloadJobGenerator<Client, Pool, Tasks, Builder> {
    /// Creates a new [EmptyBlockPayloadJobGenerator] with the given config and custom
    /// [PayloadBuilder]
    pub fn with_builder(
        client: Client,
        pool: Pool,
        executor: Tasks,
        config: BasicPayloadJobGeneratorConfig,
        builder: Builder,
    ) -> Self {
        Self {
            client,
            pool,
            executor,
            _config: config,
            builder,
        }
    }
}

impl<Client, Pool, Tasks, Builder> PayloadJobGenerator
    for EmptyBlockPayloadJobGenerator<Client, Pool, Tasks, Builder>
where
    Client: StateProviderFactory + BlockReaderIdExt + Clone + Unpin + 'static,
    Pool: TransactionPool + Unpin + 'static,
    Tasks: TaskSpawner + Clone + Unpin + 'static,
    Builder: PayloadBuilder<Pool, Client> + Unpin + 'static,
    <Builder as PayloadBuilder<Pool, Client>>::Attributes: Unpin + Clone,
    <Builder as PayloadBuilder<Pool, Client>>::BuiltPayload: Unpin + Clone,
{
    type Job = EmptyBlockPayloadJob<Client, Pool, Tasks, Builder>;

    /// This is invoked when the node receives payload attributes from the beacon node via
    /// `engine_forkchoiceUpdatedV1`
    fn new_payload_job(
        &self,
        attributes: <Builder as PayloadBuilder<Pool, Client>>::Attributes,
    ) -> Result<Self::Job, PayloadBuilderError> {
        let parent_block = if attributes.parent().is_zero() {
            // use latest block if parent is zero: genesis block
            self.client
                .block_by_number_or_tag(BlockNumberOrTag::Latest)?
                .ok_or_else(|| PayloadBuilderError::MissingParentBlock(attributes.parent()))?
                .seal_slow()
        } else {
            let block = self
                .client
                .find_block_by_hash(attributes.parent(), BlockSource::Any)?
                .ok_or_else(|| PayloadBuilderError::MissingParentBlock(attributes.parent()))?;

            // we already know the hash, so we can seal it
            block.seal(attributes.parent())
        };
        let hash = parent_block.hash();
        let header = SealedHeader::new(parent_block.header().clone(), hash);

        info!("Spawn block building job");

        let deadline = Box::pin(tokio::time::sleep(Duration::from_secs(2))); // Or another appropriate timeout
        let config = PayloadConfig::new(Arc::new(header), Bytes::default(), attributes);

        let mut job = EmptyBlockPayloadJob {
            client: self.client.clone(),
            pool: self.pool.clone(),
            executor: self.executor.clone(),
            builder: self.builder.clone(),
            config,
            cell: BlockCell::new(),
            cancel: None,
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
pub struct EmptyBlockPayloadJob<Client, Pool, Tasks, Builder>
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
    pub(crate) cancel: Option<Cancelled>,
    pub(crate) deadline: Pin<Box<Sleep>>, // Add deadline
    pub(crate) build_complete: Option<oneshot::Receiver<Result<(), PayloadBuilderError>>>,
}

impl<Client, Pool, Tasks, Builder> PayloadJob for EmptyBlockPayloadJob<Client, Pool, Tasks, Builder>
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
        tracing::debug!("Resolve kind {:?} {:?}", kind, self.cell.is_some());

        // check if self.cell has a payload
        self.cancel.take();

        let resolve_future = ResolvePayload::new(self.cell.wait_for_value());
        (resolve_future, KeepPayloadJobAlive::No)
    }
}

/// A [PayloadJob] is a a future that's being polled by the `PayloadBuilderService`
impl<Client, Pool, Tasks, Builder> EmptyBlockPayloadJob<Client, Pool, Tasks, Builder>
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
        let cancel = Cancelled::default();
        let _cancel = cancel.clone(); // Clone for the task
        let payload_config = self.config.clone();
        let cell = self.cell.clone();

        let (tx, rx) = oneshot::channel();
        self.build_complete = Some(rx);

        self.cancel = Some(cancel);
        self.executor.spawn_blocking(Box::pin(async move {
            let args = BuildArguments {
                client,
                pool,
                cached_reads: Default::default(),
                config: payload_config,
                cancel: _cancel,
                best_payload: None,
            };

            let result = builder.try_build(args, cell);
            let _ = tx.send(result);
        }));
    }
}

/// A [PayloadJob] is a a future that's being polled by the `PayloadBuilderService`
impl<Client, Pool, Tasks, Builder> Future for EmptyBlockPayloadJob<Client, Pool, Tasks, Builder>
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
            tracing::debug!("Deadline reached");
            return Poll::Ready(Ok(()));
        }

        // If cancelled via resolve_kind()
        if this.cancel.is_none() {
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
