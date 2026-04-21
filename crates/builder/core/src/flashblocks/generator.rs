use std::{
    sync::Arc,
    task::Waker,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::B256;
use futures::{Future, FutureExt};
use parking_lot::Mutex;
use reth_basic_payload_builder::{HeaderForPayload, PayloadConfig, PrecachedState};
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
use tokio::{
    sync::oneshot,
    time::{Duration, Sleep},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

use crate::PayloadBuilder;

/// The generator type that creates new jobs that build empty blocks.
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
    /// The extra block deadline in seconds
    extra_block_deadline: std::time::Duration,
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
            extra_block_deadline,
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

        // The deadline is critical for payload availability. If we reach the deadline,
        // the payload job stops and cannot be queried again. With tight deadlines close
        // to the block number, we risk reaching the deadline before the node queries the payload.
        //
        // Adding 0.5 seconds as wiggle room since block times are shorter here.
        // TODO: A better long-term solution would be to implement cancellation logic
        // that cancels existing jobs when receiving new block building requests.
        //
        // When batcher's max channel duration is big enough (e.g. 10m), the
        // sequencer would send an avalanche of FCUs/getBlockByNumber on
        // each batcher update (with 10m channel it's ~800 FCUs at once).
        // At such moment it can happen that the time b/w FCU and ensuing
        // getPayload would be on the scale of ~2.5s. Therefore we should
        // "remember" the payloads long enough to accommodate this corner-case
        // (without it we are losing blocks). Postponing the deadline for 5s
        // (not just 0.5s) because of that.
        let deadline = job_deadline(input.attributes.timestamp()) + self.extra_block_deadline;

        let deadline = Box::pin(tokio::time::sleep(deadline));

        // Extract hash before moving parent_header into Arc to avoid cloning
        let parent_hash = parent_header.hash();
        let config = PayloadConfig::new(Arc::new(parent_header), input.attributes, id);

        // Create shared mutex for synchronizing cancellation with payload publishing
        let publish_guard = Arc::new(Mutex::new(()));

        let mut job = BlockPayloadJob {
            executor: self.executor.clone(),
            builder: self.builder.clone(),
            config,
            cell: BlockCell::new(),
            finalized_cell: BlockCell::new(),
            cancel: cancel_token,
            publish_guard,
            deadline,
            build_complete: None,
            cached_reads: self.maybe_pre_cached(parent_hash),
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

/// A [`PayloadJob`] that builds empty blocks.
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
    /// The cell that holds the built payload (intermediate flashblocks, may not have state root).
    pub(crate) cell: BlockCell<Builder::BuiltPayload>,
    /// The cell that holds the finalized payload with state root computed.
    pub(crate) finalized_cell: BlockCell<Builder::BuiltPayload>,
    /// Cancellation token for the running job
    pub(crate) cancel: CancellationToken,
    /// Mutex to synchronize cancellation with payload publishing.
    pub(crate) publish_guard: Arc<Mutex<()>>,
    pub(crate) deadline: Pin<Box<Sleep>>, // Add deadline
    pub(crate) build_complete: Option<oneshot::Receiver<Result<(), PayloadBuilderError>>>,
    /// Caches all disk reads for the state the new payloads build on
    ///
    /// This is used to avoid reading the same state over and over again when new attempts are
    /// triggered, because during the building process we'll repeatedly execute the transactions.
    pub(crate) cached_reads: Option<CachedReads>,
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
        self.cell.get().ok_or_else(|| PayloadBuilderError::MissingPayload)
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

        let resolve_future = ResolvePayload::new(self.finalized_cell.wait_for_value());

        (resolve_future, KeepPayloadJobAlive::No)
    }
}

/// Build arguments
#[derive(Debug)]
pub struct BuildArguments<Attributes, Payload: BuiltPayload> {
    /// Previously cached disk reads
    pub cached_reads: CachedReads,
    /// How to configure the payload.
    pub config: PayloadConfig<Attributes, HeaderTy<Payload::Primitives>>,
    /// A marker that can be used to cancel the job.
    pub cancel: CancellationToken,
    /// Mutex to synchronize cancellation with payload publishing.
    pub publish_guard: Arc<Mutex<()>>,
    /// Cell to store the finalized payload with state root.
    pub finalized_cell: BlockCell<Payload>,
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
        let cell = self.cell.clone();
        let cancel = self.cancel.clone();
        let publish_guard = Arc::clone(&self.publish_guard);
        let finalized_cell = self.finalized_cell.clone();

        let (tx, rx) = oneshot::channel();
        self.build_complete = Some(rx);
        let cached_reads = self.cached_reads.take().unwrap_or_default();
        self.executor.spawn_blocking_task(Box::pin(async move {
            let args = BuildArguments {
                cached_reads,
                config: payload_config,
                cancel,
                publish_guard,
                finalized_cell,
            };

            let result = builder.try_build(args, cell).await;
            let _ = tx.send(result);
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

/// A future that resolves when a payload becomes available in the [`BlockCell`].
pub struct ResolvePayload<T> {
    future: WaitForValue<T>,
}

impl<T> std::fmt::Debug for ResolvePayload<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvePayload").finish_non_exhaustive()
    }
}

impl<T> ResolvePayload<T> {
    /// Creates a new [`ResolvePayload`] from the given [`WaitForValue`] future.
    pub const fn new(future: WaitForValue<T>) -> Self {
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

/// A cell that holds a value and allows waiting for it to be set.
///
/// Values can be overwritten by calling [`BlockCell::set`] multiple times.
/// Waiters registered via [`BlockCell::wait_for_value`] are woken when a new
/// value is stored.
#[derive(Clone, Debug)]
pub struct BlockCell<T> {
    state: Arc<Mutex<BlockCellState<T>>>,
}

/// Internal state shared between the cell and its waiters.
///
/// Each waiter owns a slot (index) in the `wakers` vector. Slots are reused
/// when a waiter completes or is dropped, keeping the vector compact.
#[derive(Debug)]
struct BlockCellState<T> {
    value: Option<T>,
    wakers: Vec<Option<Waker>>,
}

impl<T: Clone> BlockCell<T> {
    /// Creates an empty [`BlockCell`].
    pub fn new() -> Self {
        Self { state: Arc::new(Mutex::new(BlockCellState { value: None, wakers: Vec::new() })) }
    }

    /// Stores `value` in the cell, overwriting any previous value, and wakes one waiter.
    pub fn set(&self, value: T) {
        let wakers: Vec<Waker> = {
            let mut state = self.state.lock();
            state.value = Some(value);
            state.wakers.iter_mut().filter_map(Option::take).collect()
        };
        // Wake outside the lock to avoid holding it during waker execution.
        for waker in wakers {
            waker.wake();
        }
    }

    /// Returns a clone of the stored value, or `None` if the cell is empty.
    pub fn get(&self) -> Option<T> {
        self.state.lock().value.clone()
    }

    /// Return a future that resolves when a value is set.
    ///
    /// The returned future cleans up its waker slot on drop, so it is safe to
    /// use inside `tokio::select!` or with timeouts.
    pub fn wait_for_value(&self) -> WaitForValue<T> {
        WaitForValue { cell: self.clone(), slot: None }
    }
}

/// Future that resolves when a value is set in [`BlockCell`].
///
/// Cleans up its waker slot on drop, so cancelled futures do not leave stale
/// entries in the waker list.
pub struct WaitForValue<T> {
    cell: BlockCell<T>,
    slot: Option<usize>,
}

impl<T> std::fmt::Debug for WaitForValue<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaitForValue").finish_non_exhaustive()
    }
}

impl<T: Clone> Future for WaitForValue<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut state = this.cell.state.lock();
        if let Some(value) = state.value.clone() {
            // Clear our slot since we're resolving.
            if let Some(idx) = this.slot.take() {
                state.wakers[idx] = None;
            }
            Poll::Ready(value)
        } else {
            let waker = cx.waker().clone();
            match this.slot {
                Some(idx) => {
                    // Update existing slot with the current waker.
                    state.wakers[idx] = Some(waker);
                }
                None => {
                    // Reuse an empty slot or allocate a new one.
                    let idx = state.wakers.iter().position(Option::is_none).unwrap_or_else(|| {
                        state.wakers.push(None);
                        state.wakers.len() - 1
                    });
                    state.wakers[idx] = Some(waker);
                    this.slot = Some(idx);
                }
            }
            Poll::Pending
        }
    }
}

impl<T> Drop for WaitForValue<T> {
    fn drop(&mut self) {
        if let Some(idx) = self.slot.take() {
            self.cell.state.lock().wakers[idx] = None;
        }
    }
}

impl<T: Clone> Default for BlockCell<T> {
    fn default() -> Self {
        Self::new()
    }
}

fn job_deadline(unix_timestamp_secs: u64) -> std::time::Duration {
    let unix_now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(e) => {
            warn!(error = %e, "System clock went backward, returning zero deadline");
            return Duration::ZERO;
        }
    };

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
    use alloy_eips::eip7685::Requests;
    use alloy_primitives::U256;
    use base_common_consensus::BasePrimitives;
    use base_execution_payload_builder::{OpPayloadBuilderAttributes, PayloadPrimitives};
    use rand::rng;
    use reth_node_api::{BuiltPayloadExecutedBlock, NodePrimitives};
    use reth_primitives_traits::SealedBlock;
    use reth_provider::test_utils::MockEthProvider;
    use reth_tasks::Runtime;
    use reth_testing_utils::generators::{BlockRangeParams, random_block_range};
    use tokio::{
        task,
        time::{Duration, sleep},
    };

    use super::*;

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
        Started,
        Cancelled,
    }

    #[async_trait::async_trait]
    impl<N> PayloadBuilder for MockBuilder<N>
    where
        N: PayloadPrimitives,
    {
        type Attributes = OpPayloadBuilderAttributes<N::SignedTx>;
        type BuiltPayload = MockPayload;

        async fn try_build(
            &self,
            args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
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
        let mut attr = OpPayloadBuilderAttributes::default();
        attr.payload_attributes.parent = client.latest_header()?.unwrap().hash();

        {
            let parent_hash = attr.payload_attributes.parent;
            let input = BuildNewPayload {
                attributes: attr.clone(),
                parent_hash,
                cache: None,
                trie_handle: None,
            };
            let job = generator.new_payload_job(input, attr.payload_id(&parent_hash))?;
            let _ = job.await;

            // you need to give one second for the job to be dropped and cancelled the internal job
            tokio::time::sleep(Duration::from_secs(1)).await;

            let events = builder.get_events();
            assert_eq!(events, vec![BlockEvent::Started, BlockEvent::Cancelled]);
        }

        {
            // job resolve triggers cancellations from the build task
            let parent_hash = attr.payload_attributes.parent;
            let input = BuildNewPayload {
                attributes: attr.clone(),
                parent_hash,
                cache: None,
                trie_handle: None,
            };
            let mut job = generator.new_payload_job(input, attr.payload_id(&parent_hash))?;
            let _ = job.resolve();
            let _ = job.await;

            tokio::time::sleep(Duration::from_secs(1)).await;

            let events = builder.get_events();
            assert_eq!(events, vec![BlockEvent::Started, BlockEvent::Cancelled]);
        }

        Ok(())
    }
}
