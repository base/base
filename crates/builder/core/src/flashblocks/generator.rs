use std::sync::Arc;

use alloy_primitives::B256;
use base_payload_builder::{BlockCell, PayloadBuilder};
use parking_lot::Mutex;
use reth_basic_payload_builder::{
    BasicPayloadJobGeneratorConfig, HeaderForPayload, PayloadConfig, PrecachedState,
};
use reth_node_api::{NodePrimitives, PayloadBuilderAttributes};
use reth_payload_builder::{PayloadBuilderError, PayloadJobGenerator};
use reth_provider::{BlockReaderIdExt, CanonStateNotification, StateProviderFactory};
use reth_revm::cached::CachedReads;
use reth_tasks::TaskSpawner;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::BlockPayloadJob;

/// The generator type that creates new jobs that build empty blocks.
#[derive(Debug)]
pub struct BlockPayloadJobGenerator<Client, Tasks, Builder> {
    /// The client that can interact with the chain.
    pub client: Client,
    /// How to spawn building tasks
    pub executor: Tasks,
    /// The configuration for the job generator.
    pub _config: BasicPayloadJobGeneratorConfig,
    /// The type responsible for building payloads.
    ///
    /// See [`PayloadBuilder`]
    pub builder: Builder,
    /// Whether to ensure only one payload is being processed at a time
    pub ensure_only_one_payload: bool,
    /// The last payload being processed
    pub last_payload: Arc<Mutex<CancellationToken>>,
    /// The extra block deadline in seconds
    pub extra_block_deadline: std::time::Duration,
    /// Stored `cached_reads` for new payload jobs.
    pub pre_cached: Option<PrecachedState>,
    /// Whether to compute state root only on finalization (when `get_payload` is called).
    pub compute_state_root_on_finalize: bool,
}

// === impl BlockPayloadJobGenerator ===

impl<Client, Tasks, Builder> BlockPayloadJobGenerator<Client, Tasks, Builder> {
    /// Creates a new [`BlockPayloadJobGenerator`] with the given config and custom
    /// [`PayloadBuilder`]
    pub fn with_builder(
        client: Client,
        executor: Tasks,
        config: BasicPayloadJobGeneratorConfig,
        builder: Builder,
        ensure_only_one_payload: bool,
        extra_block_deadline: std::time::Duration,
        compute_state_root_on_finalize: bool,
    ) -> Self {
        Self {
            client,
            executor,
            _config: config,
            builder,
            ensure_only_one_payload,
            last_payload: Arc::new(Mutex::new(CancellationToken::new())),
            extra_block_deadline,
            pre_cached: None,
            compute_state_root_on_finalize,
        }
    }

    /// Returns the pre-cached reads for the given parent header if it matches the cached state's
    /// block.
    fn maybe_pre_cached(&self, parent: B256) -> Option<CachedReads> {
        self.pre_cached.as_ref().filter(|pc| pc.block == parent).map(|pc| pc.cached.clone())
    }

    /// Computes the job deadline from a unix timestamp.
    ///
    /// Returns a [`Duration`] representing how long until the given timestamp.
    /// If the timestamp is in the past or current, returns a minimum of 1 second.
    pub fn deadline(unix_timestamp_secs: u64) -> Duration {
        let unix_now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs(),
            Err(e) => {
                warn!(error = %e, "System clock went backward, returning zero deadline");
                return Duration::ZERO;
            }
        };

        // Safe subtraction that handles the case where timestamp is in the past
        let duration_until = unix_timestamp_secs.saturating_sub(unix_now);

        if duration_until == 0 {
            // Enforce a minimum block time of 1 second by rounding up any duration less than 1
            // second
            Duration::from_secs(1)
        } else {
            Duration::from_secs(duration_until)
        }
    }
}

impl<Client, Tasks, Builder> PayloadJobGenerator
    for BlockPayloadJobGenerator<Client, Tasks, Builder>
where
    Client: StateProviderFactory
        + BlockReaderIdExt<Header = HeaderForPayload<Builder::BuiltPayload>>
        + Clone
        + Unpin
        + 'static,
    Tasks: TaskSpawner + Clone + Unpin + 'static,
    Builder: PayloadBuilder + Unpin + 'static,
    Builder::Attributes: Unpin + Clone,
    Builder::BuiltPayload: Unpin + Clone,
{
    type Job = BlockPayloadJob<Tasks, Builder>;

    /// This is invoked when the node receives payload attributes from the beacon node via
    /// `engine_forkchoiceUpdatedVX`
    fn new_payload_job(
        &self,
        attributes: <Builder as PayloadBuilder>::Attributes,
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
        //
        // When batcher's max channel duration is big enough (e.g. 10m), the
        // sequencer would send an avalanche of FCUs/getBlockByNumber on
        // each batcher update (with 10m channel it's ~800 FCUs at once).
        // At such moment it can happen that the time b/w FCU and ensuing
        // getPayload would be on the scale of ~2.5s. Therefore we should
        // "remember" the payloads long enough to accommodate this corner-case
        // (without it we are losing blocks). Postponing the deadline for 5s
        // (not just 0.5s) because of that.
        let deadline = Self::deadline(attributes.timestamp()) + self.extra_block_deadline;

        let deadline = Box::pin(tokio::time::sleep(deadline));

        // Extract hash before moving parent_header into Arc to avoid cloning
        let parent_hash = parent_header.hash();
        let config = PayloadConfig::new(Arc::new(parent_header), attributes);

        // Create shared mutex for synchronizing cancellation with payload publishing
        let publish_guard = Arc::new(Mutex::new(()));

        let mut job = BlockPayloadJob {
            executor: self.executor.clone(),
            builder: self.builder.clone(),
            config,
            cell: BlockCell::new(),
            finalized_cell: BlockCell::new(),
            compute_state_root_on_finalize: self.compute_state_root_on_finalize,
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

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use alloy_eips::eip7685::Requests;
    use alloy_primitives::U256;
    use base_payload_builder::BuildArguments;
    use rand::rng;
    use reth_node_api::{BuiltPayloadExecutedBlock, NodePrimitives};
    use reth_optimism_payload_builder::{OpPayloadPrimitives, payload::OpPayloadBuilderAttributes};
    use reth_optimism_primitives::OpPrimitives;
    use reth_payload_builder::PayloadJob;
    use reth_payload_primitives::BuiltPayload;
    use reth_primitives::SealedBlock;
    use reth_provider::test_utils::MockEthProvider;
    use reth_tasks::TokioTaskExecutor;
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
        block: SealedBlock<<OpPrimitives as NodePrimitives>::Block>,
        fees: U256,
        requests: Option<Requests>,
    }

    impl BuiltPayload for MockPayload {
        type Primitives = OpPrimitives;

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
        N: OpPayloadPrimitives,
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
    async fn test_deadline() {
        // Test future deadline
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let future_timestamp = now + Duration::from_secs(2);
        // 2 seconds from now
        let deadline = BlockPayloadJobGenerator::<(), (), ()>::deadline(future_timestamp.as_secs());
        assert!(deadline <= Duration::from_secs(2));
        assert!(deadline > Duration::from_secs(0));

        // Test past deadline
        let past_timestamp = now - Duration::from_secs(10);
        let deadline = BlockPayloadJobGenerator::<(), (), ()>::deadline(past_timestamp.as_secs());
        // Should default to 1 second when timestamp is in the past
        assert_eq!(deadline, Duration::from_secs(1));

        // Test current timestamp
        let deadline = BlockPayloadJobGenerator::<(), (), ()>::deadline(now.as_secs());
        // Should use 1 second when timestamp is current
        assert_eq!(deadline, Duration::from_secs(1));
    }

    #[tokio::test]
    async fn test_payload_generator() -> eyre::Result<()> {
        let mut rng = rng();

        let client = MockEthProvider::default();
        let executor = TokioTaskExecutor::default();
        let config = BasicPayloadJobGeneratorConfig::default();
        let builder = MockBuilder::<OpPrimitives>::new();

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
            config,
            builder.clone(),
            false,
            std::time::Duration::from_secs(1),
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
