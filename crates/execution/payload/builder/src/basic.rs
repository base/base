use std::{
    future::Future,
    ops::Deref,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_eips::merge::SLOT_DURATION;
use alloy_primitives::{B256, U256};
use base_common_chain_config::ChainSpecProvider;
use base_common_runtime_tasks::Runtime;
use base_execution_evm_blocks::CancelOnDrop;
use base_execution_payload_types::{
    BaseBuiltPayload, BasePayloadBuilderAttributes, PayloadBuilderError, PayloadKind,
};
use base_execution_state_memory::CachedReads;
use base_execution_trie::PayloadStateRootHandle;
use base_execution_txpool::TransactionPool;
use futures_core::ready;
use futures_util::FutureExt;
use reth_chain_state::CanonStateNotification;
use reth_execution_cache::SavedCache;
use reth_primitives_traits::SealedHeader;
use reth_storage_api::{BlockReaderIdExt, StateProviderFactory};
use tokio::{
    sync::{Semaphore, oneshot},
    time::{Interval, Sleep},
};
use tracing::{debug, trace, warn};

use crate::{
    BasePayloadBuilder, BuildNewPayload, KeepPayloadJobAlive, PayloadBuilderLease, PayloadId,
    PayloadJob, job_metrics::PayloadBuilderMetrics,
};

const PAYLOAD_BUILDER_THREAD_NAME: &str = "payload-builder";

/// Header used by payload builders.
pub type HeaderForPayload = base_common_types_chain::Header;

/// Creates and schedules Base payload construction jobs.
#[derive(Debug)]
pub struct BasicPayloadJobGenerator<Client, Pool> {
    /// The client that can interact with the chain.
    client: Client,
    /// The task executor to spawn payload building tasks on.
    executor: Runtime,
    /// The configuration for the job generator.
    config: BasicPayloadJobGeneratorConfig,
    /// Restricts how many generator tasks can be executed at once.
    payload_task_guard: PayloadTaskGuard,
    /// The type responsible for building payloads.
    ///
    /// See [`BasePayloadBuilder`]
    builder: BasePayloadBuilder<Pool, Client>,
    /// Stored `cached_reads` for new payload jobs.
    pre_cached: Option<PrecachedState>,
    /// Stored parent block information for new payload jobs.
    pre_cached_parent_block_info: Option<PrecachedParentBlockInfo>,
}

// === impl BasicPayloadJobGenerator ===

impl<Client, Pool> BasicPayloadJobGenerator<Client, Pool> {
    /// Creates a new [`BasicPayloadJobGenerator`] with the given config and custom
    /// [`BasePayloadBuilder`]
    pub fn with_builder(
        client: Client,
        executor: Runtime,
        config: BasicPayloadJobGeneratorConfig,
        builder: BasePayloadBuilder<Pool, Client>,
    ) -> Self {
        Self {
            client,
            executor,
            payload_task_guard: PayloadTaskGuard::new(config.max_payload_tasks),
            config,
            builder,
            pre_cached: None,
            pre_cached_parent_block_info: None,
        }
    }

    /// Returns the maximum duration a job should be allowed to run.
    ///
    /// This adheres to the following specification:
    /// > Client software SHOULD stop the updating process when either a call to engine_getPayload
    /// > with the build process's payloadId is made or SECONDS_PER_SLOT (12s in the Mainnet
    /// > configuration) have passed since the point in time identified by the timestamp parameter.
    ///
    /// See also <https://github.com/ethereum/execution-apis/blob/431cf72fd3403d946ca3e3afc36b973fc87e0e89/src/engine/paris.md?plain=1#L137>
    #[inline]
    fn max_job_duration(&self, unix_timestamp: u64) -> Duration {
        let duration_until_timestamp = duration_until(unix_timestamp);

        // safety in case clocks are bad
        let duration_until_timestamp = duration_until_timestamp.min(self.config.deadline * 3);

        self.config.deadline + duration_until_timestamp
    }

    /// Returns the [Instant](tokio::time::Instant) at which the job should be terminated because it
    /// is considered timed out.
    #[inline]
    fn job_deadline(&self, unix_timestamp: u64) -> tokio::time::Instant {
        tokio::time::Instant::now() + self.max_job_duration(unix_timestamp)
    }

    /// Returns a reference to the tasks type
    pub const fn tasks(&self) -> &Runtime {
        &self.executor
    }

    /// Returns the pre-cached reads for the given parent header if it matches the cached state's
    /// block.
    fn maybe_pre_cached(&self, parent: B256) -> Option<CachedReads> {
        if !self.config.pre_cache_state {
            return None;
        }

        self.pre_cached.as_ref().filter(|pc| pc.block == parent).map(|pc| pc.cached.clone())
    }

    /// Returns the cached parent block information if it matches the requested parent.
    fn maybe_parent_block_info(&self, parent: B256) -> Option<PayloadParentBlockInfo> {
        self.pre_cached_parent_block_info
            .as_ref()
            .filter(|info| info.block == parent)
            .map(|info| info.parent_block_info)
    }
}

// === impl BasicPayloadJobGenerator ===

impl<Client, Pool> BasicPayloadJobGenerator<Client, Pool>
where
    Client: StateProviderFactory + BlockReaderIdExt + ChainSpecProvider + Clone + Unpin + 'static,
    Pool: TransactionPool + Unpin + 'static,
    Pool: base_execution_txpool::ParkableTransactionPool,
{
    /// Starts building a payload against its requested parent.
    pub fn new_payload_job(
        &self,
        input: BuildNewPayload,
        id: PayloadId,
    ) -> Result<BasicPayloadJob<Pool, Client>, PayloadBuilderError> {
        let BuildNewPayload { attributes, parent_hash, mut resources } = input;
        let parent_header = if parent_hash.is_zero() {
            // Use latest header for genesis block case
            self.client
                .latest_header()
                .map_err(PayloadBuilderError::from)?
                .ok_or_else(|| PayloadBuilderError::MissingParentHeader(B256::ZERO))?
        } else {
            // Fetch specific header by hash
            self.client
                .sealed_header_by_hash(parent_hash)
                .map_err(PayloadBuilderError::from)?
                .ok_or_else(|| PayloadBuilderError::MissingParentHeader(parent_hash))?
        };

        let parent_hash = parent_header.hash();
        let cached_reads = self.maybe_pre_cached(parent_hash);
        let parent_block_info = self.maybe_parent_block_info(parent_hash);

        let config = PayloadConfig::new(Arc::new(parent_header), attributes, id)
            .with_parent_block_info(parent_block_info);

        let until = self.job_deadline(config.attributes.timestamp());
        let deadline = Box::pin(tokio::time::sleep_until(until));

        let mut job = BasicPayloadJob {
            config,
            executor: self.executor.clone(),
            deadline,
            // ticks immediately
            interval: tokio::time::interval(self.config.interval),
            best_payload: PayloadState::Missing,
            pending_block: None,
            cached_reads,
            execution_cache: resources.take_execution_cache(),
            state_root_handle: resources.take_state_root_handle(),
            leases: resources.take_leases(),
            payload_task_guard: self.payload_task_guard.clone(),
            metrics: Default::default(),
            builder: self.builder.clone(),
        };

        // start the first job right away
        job.spawn_build_job();

        Ok(job)
    }

    /// Updates cached state after a canonical chain change.
    pub fn on_new_state(&mut self, new_state: CanonStateNotification) {
        if !self.config.pre_cache_state {
            self.pre_cached = None;
            return;
        }

        let mut cached = CachedReads::default();

        // extract the state from the notification and put it into the cache
        let committed = new_state.committed();
        let new_execution_outcome = committed.execution_outcome();
        for (addr, acc) in new_execution_outcome.bundle_accounts_iter() {
            if let Some(info) = acc.info.clone() {
                // we want pre cache existing accounts and their storage
                // this only includes changed accounts and storage but is better than nothing
                let storage =
                    acc.storage.iter().map(|(key, slot)| (*key, slot.present_value)).collect();
                cached.insert_account(addr, info, storage);
            }
        }

        let tip = committed.tip();
        let block = tip.hash();
        let parent_block_info =
            PayloadParentBlockInfo { transaction_count: tip.transaction_count() };

        self.pre_cached = Some(PrecachedState { block, cached });
        self.pre_cached_parent_block_info =
            Some(PrecachedParentBlockInfo { block, parent_block_info });
    }
}

/// Pre-filled [`CachedReads`] for a specific block.
///
/// This is extracted from the [`CanonStateNotification`] for the tip block.
#[derive(Debug, Clone)]
pub struct PrecachedState {
    /// The block for which the state is pre-cached.
    pub block: B256,
    /// Cached state for the block.
    pub cached: CachedReads,
}

/// Pre-filled parent block information for a specific block.
#[derive(Debug, Clone, Copy)]
struct PrecachedParentBlockInfo {
    /// The block for which the parent block information is cached.
    block: B256,
    /// Cached parent block information.
    parent_block_info: PayloadParentBlockInfo,
}

/// Restricts how many generator tasks can be executed at once.
#[derive(Debug, Clone)]
pub struct PayloadTaskGuard(Arc<Semaphore>);

impl Deref for PayloadTaskGuard {
    type Target = Semaphore;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

// === impl PayloadTaskGuard ===

impl PayloadTaskGuard {
    /// Constructs `Self` with a maximum task count of `max_payload_tasks`.
    pub fn new(max_payload_tasks: usize) -> Self {
        Self(Arc::new(Semaphore::new(max_payload_tasks)))
    }

    /// Acquires an owned permit for a payload build task.
    async fn acquire_owned(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.0.clone().acquire_owned().await.expect("payload task semaphore closed")
    }
}

/// Settings for the [`BasicPayloadJobGenerator`].
#[derive(Debug, Clone)]
pub struct BasicPayloadJobGeneratorConfig {
    /// The interval at which the job should build a new payload after the last.
    interval: Duration,
    /// The deadline for when the payload builder job should resolve.
    ///
    /// By default this is [`SLOT_DURATION`]: 12s
    deadline: Duration,
    /// Maximum number of tasks to spawn for building a payload.
    max_payload_tasks: usize,
    /// Whether to pre-cache changed state from canonical state notifications.
    pre_cache_state: bool,
}

// === impl BasicPayloadJobGeneratorConfig ===

impl BasicPayloadJobGeneratorConfig {
    /// Sets the interval at which the job should build a new payload after the last.
    pub const fn interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Sets the deadline when this job should resolve.
    pub const fn deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// Sets the maximum number of tasks to spawn for building a payload(s).
    ///
    /// # Panics
    ///
    /// If `max_payload_tasks` is 0.
    pub fn max_payload_tasks(mut self, max_payload_tasks: usize) -> Self {
        assert!(max_payload_tasks > 0, "max_payload_tasks must be greater than 0");
        self.max_payload_tasks = max_payload_tasks;
        self
    }

    /// Sets whether to pre-cache changed state from canonical state notifications.
    ///
    /// This keeps the parent block's state changes in memory so payload jobs building on top of it
    /// can reuse those reads.
    pub const fn pre_cache_state(mut self, pre_cache_state: bool) -> Self {
        self.pre_cache_state = pre_cache_state;
        self
    }
}

impl Default for BasicPayloadJobGeneratorConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            // 12s slot time
            deadline: SLOT_DURATION,
            max_payload_tasks: 3,
            pre_cache_state: true,
        }
    }
}

/// A basic payload job that continuously builds a payload with the best transactions from the pool.
///
/// This type is a [`PayloadJob`] and [`Future`] that terminates when the deadline is reached or
/// when the job is resolved: [`PayloadJob::resolve`].
///
/// This basic job implementation will trigger new payload build task continuously until the job is
/// resolved or the deadline is reached, or until the built payload is marked as frozen:
/// [`BuildOutcome::Freeze`]. Once a frozen payload is returned, no additional payloads will be
/// built and this future will wait to be resolved: [`PayloadJob::resolve`] or terminated if the
/// deadline is reached.
#[derive(Debug)]
pub struct BasicPayloadJob<Pool, Client> {
    /// The configuration for how the payload will be created.
    config: PayloadConfig,
    /// How to spawn building tasks
    executor: Runtime,
    /// The deadline when this job should resolve.
    deadline: Pin<Box<Sleep>>,
    /// The interval at which the job should build a new payload after the last.
    interval: Interval,
    /// The best payload so far and its state.
    best_payload: PayloadState,
    /// Receiver for the block that is currently being built.
    pending_block: Option<PendingPayload>,
    /// Restricts how many generator tasks can be executed at once.
    payload_task_guard: PayloadTaskGuard,
    /// Caches all disk reads for the state the new payloads builds on
    ///
    /// This is used to avoid reading the same state over and over again when new attempts are
    /// triggered, because during the building process we'll repeatedly execute the transactions.
    cached_reads: Option<CachedReads>,
    /// Optional execution cache shared with the engine.
    execution_cache: Option<SavedCache>,
    /// Optional state-root task handle, shared with the engine.
    state_root_handle: Option<PayloadStateRootHandle>,
    /// Lifecycle leases shared with the payload-builder service.
    ///
    /// Every detached build task clones these so that the loaned resources remain available until
    /// `try_build` completes, even if the payload job is resolved first.
    leases: Vec<PayloadBuilderLease>,
    /// metrics for this type
    metrics: PayloadBuilderMetrics,
    /// The type responsible for building payloads.
    ///
    /// See [`BasePayloadBuilder`]
    builder: BasePayloadBuilder<Pool, Client>,
}

impl<Pool, Client> BasicPayloadJob<Pool, Client>
where
    Client: StateProviderFactory + BlockReaderIdExt + ChainSpecProvider + Clone + Unpin + 'static,
    Pool: TransactionPool + Unpin + 'static,
    Pool: base_execution_txpool::ParkableTransactionPool,
{
    /// Spawns a new payload build task.
    fn spawn_build_job(&mut self) {
        trace!(target: "payload_builder", id = %self.config.payload_id(), "spawn new payload build task");
        let (tx, rx) = oneshot::channel();
        let cancel = CancelOnDrop::default();
        let pending_cancel = cancel.clone();
        let guard = self.payload_task_guard.clone();
        let payload_config = self.config.clone();
        let best_payload = self.best_payload.payload().cloned();
        self.metrics.inc_initiated_payload_builds();
        let cached_reads = self.cached_reads.take().unwrap_or_default();
        let execution_cache = self.execution_cache.clone();
        let state_root_handle = self.state_root_handle.take();
        let leases = self.leases.clone();
        let builder = self.builder.clone();
        let executor = self.executor.clone();
        self.executor.spawn_task(async move {
            // acquire the permit for executing the task
            let permit = guard.acquire_owned().await;
            executor.spawn_blocking_named_or_tokio(PAYLOAD_BUILDER_THREAD_NAME, move || {
                let _permit = permit;
                let args = BuildArguments {
                    cached_reads,
                    execution_cache,
                    state_root_handle,
                    config: payload_config,
                    cancel,
                    best_payload,
                };
                let result = builder.try_build(args);
                drop(leases);
                let _ = tx.send(result);
            });
        });

        self.pending_block = Some(PendingPayload { cancel: pending_cancel, payload: rx });
    }
}

impl<Pool, Client> Future for BasicPayloadJob<Pool, Client>
where
    Client: StateProviderFactory + BlockReaderIdExt + ChainSpecProvider + Clone + Unpin + 'static,
    Pool: TransactionPool + Unpin + 'static,
    Pool: base_execution_txpool::ParkableTransactionPool,
{
    type Output = Result<(), PayloadBuilderError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // check if the deadline is reached
        if this.deadline.as_mut().poll(cx).is_ready() {
            trace!(target: "payload_builder", "payload building deadline reached");
            return Poll::Ready(Ok(()));
        }

        loop {
            // Wait for any pending build to complete before polling the next tick.
            //
            // This avoids consuming interval ticks while a build is still in-flight,
            // which would delay the follow-up build by a full interval even though
            // the current attempt has already finished.
            if let Some(mut fut) = this.pending_block.take() {
                match fut.poll_unpin(cx) {
                    Poll::Ready(Ok(outcome)) => match outcome {
                        BuildOutcome::Better { payload, cached_reads } => {
                            this.cached_reads = Some(cached_reads);
                            debug!(target: "payload_builder", value = %payload.fees(), "built better payload");
                            this.best_payload = PayloadState::Best(payload);
                        }
                        BuildOutcome::Freeze(payload) => {
                            debug!(target: "payload_builder", "payload frozen, no further building will occur");
                            this.best_payload = PayloadState::Frozen(payload);
                        }
                        BuildOutcome::Aborted { fees, cached_reads } => {
                            this.cached_reads = Some(cached_reads);
                            trace!(target: "payload_builder", worse_fees = %fees, "skipped payload build of worse block");
                        }
                        BuildOutcome::Cancelled => {
                            unreachable!("the cancel signal never fired")
                        }
                    },
                    Poll::Ready(Err(error)) => {
                        // job failed, but we simply try again next interval
                        debug!(target: "payload_builder", %error, "payload build attempt failed");
                        this.metrics.inc_failed_payload_builds();
                    }
                    Poll::Pending => {
                        this.pending_block = Some(fut);
                        return Poll::Pending;
                    }
                }
            }

            if this.best_payload.is_frozen() {
                return Poll::Pending;
            }

            // Wait for the next build interval tick.
            //
            // The loop is needed because `poll_tick` does not register a waker
            // when it returns `Ready`, so we must loop back after spawning a job
            // to reach a point that *does* register one (the pending block poll above).
            ready!(this.interval.poll_tick(cx));
            this.spawn_build_job()
        }
    }
}

impl<Pool, Client> PayloadJob for BasicPayloadJob<Pool, Client>
where
    Client: StateProviderFactory + BlockReaderIdExt + ChainSpecProvider + Clone + Unpin + 'static,
    Pool: TransactionPool + Unpin + 'static,
    Pool: base_execution_txpool::ParkableTransactionPool,
{
    type ResolvePayloadFuture = ResolveBestPayload;

    fn best_payload(&self) -> Result<BaseBuiltPayload, PayloadBuilderError> {
        if let Some(payload) = self.best_payload.payload() {
            Ok(payload.clone())
        } else {
            // No payload has been built yet, but we need to return something that the CL then
            // can deliver, so we need to return an empty payload.
            //
            // Note: it is assumed that this is unlikely to happen, as the payload job is
            // started right away and the first full block should have been
            // built by the time CL is requesting the payload.
            self.metrics.inc_requested_empty_payload();
            self.builder.build_empty_payload(self.config.clone())
        }
    }

    fn payload_attributes(&self) -> Result<BasePayloadBuilderAttributes, PayloadBuilderError> {
        Ok(self.config.attributes.clone())
    }

    fn payload_timestamp(&self) -> Result<u64, PayloadBuilderError> {
        Ok(self.config.attributes.timestamp())
    }

    fn resolve_kind(
        &mut self,
        _kind: PayloadKind,
    ) -> (Self::ResolvePayloadFuture, KeepPayloadJobAlive) {
        let best_payload = self.best_payload.payload().cloned();
        if best_payload.is_none() && self.pending_block.is_none() {
            // ensure we have a job scheduled if we don't have a best payload yet and none is active
            self.spawn_build_job();
        }

        let maybe_better = self.pending_block.take();
        if best_payload.is_none() {
            if let Some(pending) = maybe_better.as_ref() {
                pending.cancel.request_finalization();
            }
            debug!(target: "payload_builder", id=%self.config.payload_id(), "awaiting in progress Base payload build job");
        }
        let fut = ResolveBestPayload { best_payload, maybe_better };

        (fut, KeepPayloadJobAlive::No)
    }
}

/// Represents the current state of a payload being built.
#[derive(Debug, Clone)]
pub enum PayloadState {
    /// No payload has been built yet.
    Missing,
    /// The best payload built so far, which may still be improved upon.
    Best(BaseBuiltPayload),
    /// The payload is frozen and no further building should occur.
    ///
    /// Contains the final payload `BaseBuiltPayload` that should be used.
    Frozen(BaseBuiltPayload),
}

impl PayloadState {
    /// Checks if the payload is frozen.
    pub const fn is_frozen(&self) -> bool {
        matches!(self, Self::Frozen(_))
    }

    /// Returns the payload if it exists (either Best or Frozen).
    pub const fn payload(&self) -> Option<&BaseBuiltPayload> {
        match self {
            Self::Missing => None,
            Self::Best(p) | Self::Frozen(p) => Some(p),
        }
    }
}

/// The future that returns the best payload to be served to the consensus layer.
///
/// This returns the payload that's supposed to be sent to the CL.
///
/// If payload has been built so far, it will return that, but it will check if there's a better
/// payload available from an in progress build job. If so it will return that.
///
/// If no payload has been built so far, it will either return an empty payload or the result of the
/// in progress build job, whatever finishes first.
#[derive(Debug)]
pub struct ResolveBestPayload {
    /// Best payload so far.
    pub best_payload: Option<BaseBuiltPayload>,
    /// Regular payload job that's currently running that might produce a better payload.
    pub maybe_better: Option<PendingPayload>,
}

impl ResolveBestPayload {
    const fn is_empty(&self) -> bool {
        self.best_payload.is_none() && self.maybe_better.is_none()
    }
}

impl Future for ResolveBestPayload {
    type Output = Result<BaseBuiltPayload, PayloadBuilderError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // check if there is a better payload before returning the best payload
        if let Some(fut) = Pin::new(&mut this.maybe_better).as_pin_mut()
            && let Poll::Ready(res) = fut.poll(cx)
        {
            this.maybe_better = None;
            if let Ok(Some(payload)) = res.map(|out| out.into_payload()).inspect_err(
                |err| warn!(target: "payload_builder", %err, "failed to resolve pending payload"),
            ) {
                debug!(target: "payload_builder", "resolving better payload");
                return Poll::Ready(Ok(payload));
            }
        }

        if let Some(best) = this.best_payload.take() {
            debug!(target: "payload_builder", "resolving best payload");
            return Poll::Ready(Ok(best));
        }

        if this.is_empty() {
            return Poll::Ready(Err(PayloadBuilderError::MissingPayload));
        }

        Poll::Pending
    }
}

/// A future that resolves to the result of the block building job.
#[derive(Debug)]
pub struct PendingPayload {
    /// Cancels the job on drop and carries cooperative control signals.
    cancel: CancelOnDrop,
    /// The channel to send the result to.
    payload: oneshot::Receiver<Result<BuildOutcome, PayloadBuilderError>>,
}

impl PendingPayload {
    /// Constructs a `PendingPayload` future.
    pub const fn new(
        cancel: CancelOnDrop,
        payload: oneshot::Receiver<Result<BuildOutcome, PayloadBuilderError>>,
    ) -> Self {
        Self { cancel, payload }
    }
}

impl Future for PendingPayload {
    type Output = Result<BuildOutcome, PayloadBuilderError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = ready!(self.payload.poll_unpin(cx));
        Poll::Ready(res.map_err(Into::into).and_then(|res| res))
    }
}

/// Static config for how to build a payload.
#[derive(Clone, Debug)]
pub struct PayloadConfig {
    /// The parent header.
    pub parent_header: Arc<SealedHeader>,
    /// Additional parent block information, if available.
    pub parent_block_info: Option<PayloadParentBlockInfo>,
    /// Requested attributes for the payload.
    pub attributes: BasePayloadBuilderAttributes,
    /// The payload id.
    pub payload_id: PayloadId,
}

/// Additional information about the parent block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PayloadParentBlockInfo {
    /// Number of transactions in the parent block.
    pub transaction_count: usize,
}

impl PayloadConfig {
    /// Create new payload config.
    pub const fn new(
        parent_header: Arc<SealedHeader>,
        attributes: BasePayloadBuilderAttributes,
        payload_id: PayloadId,
    ) -> Self {
        Self { parent_header, parent_block_info: None, attributes, payload_id }
    }

    /// Attaches cached parent block information.
    pub const fn with_parent_block_info(
        mut self,
        parent_block_info: Option<PayloadParentBlockInfo>,
    ) -> Self {
        self.parent_block_info = parent_block_info;
        self
    }

    /// Returns the payload id.
    pub const fn payload_id(&self) -> PayloadId {
        self.payload_id
    }
}

/// The possible outcomes of a payload building attempt.
#[derive(Debug)]
pub enum BuildOutcome {
    /// Successfully built a better block.
    Better {
        /// The new payload that was built.
        payload: BaseBuiltPayload,
        /// The cached reads that were used to build the payload.
        cached_reads: CachedReads,
    },
    /// Aborted payload building because resulted in worse block wrt. fees.
    Aborted {
        /// The total fees associated with the attempted payload.
        fees: U256,
        /// The cached reads that were used to build the payload.
        cached_reads: CachedReads,
    },
    /// Build job was cancelled
    Cancelled,

    /// The payload is final and no further building should occur
    Freeze(BaseBuiltPayload),
}

impl BuildOutcome {
    /// Consumes the type and returns the payload if the outcome is `Better` or `Freeze`.
    pub fn into_payload(self) -> Option<BaseBuiltPayload> {
        match self {
            Self::Better { payload, .. } | Self::Freeze(payload) => Some(payload),
            _ => None,
        }
    }

    /// Consumes the type and returns the payload if the outcome is `Better` or `Freeze`.
    pub const fn payload(&self) -> Option<&BaseBuiltPayload> {
        match self {
            Self::Better { payload, .. } | Self::Freeze(payload) => Some(payload),
            _ => None,
        }
    }

    /// Returns true if the outcome is `Better`.
    pub const fn is_better(&self) -> bool {
        matches!(self, Self::Better { .. })
    }

    /// Returns true if the outcome is `Freeze`.
    pub const fn is_frozen(&self) -> bool {
        matches!(self, Self::Freeze { .. })
    }

    /// Returns true if the outcome is `Aborted`.
    pub const fn is_aborted(&self) -> bool {
        matches!(self, Self::Aborted { .. })
    }

    /// Returns true if the outcome is `Cancelled`.
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

/// The possible outcomes of a payload building attempt without reused [`CachedReads`]
#[derive(Debug)]
pub enum BuildOutcomeKind {
    /// Successfully built a better block.
    Better {
        /// The new payload that was built.
        payload: BaseBuiltPayload,
    },
    /// Aborted payload building because resulted in worse block wrt. fees.
    Aborted {
        /// The total fees associated with the attempted payload.
        fees: U256,
    },
    /// Build job was cancelled
    Cancelled,
    /// The payload is final and no further building should occur
    Freeze(BaseBuiltPayload),
}

impl BuildOutcomeKind {
    /// Attaches the [`CachedReads`] to the outcome.
    pub fn with_cached_reads(self, cached_reads: CachedReads) -> BuildOutcome {
        match self {
            Self::Better { payload } => BuildOutcome::Better { payload, cached_reads },
            Self::Aborted { fees } => BuildOutcome::Aborted { fees, cached_reads },
            Self::Cancelled => BuildOutcome::Cancelled,
            Self::Freeze(payload) => BuildOutcome::Freeze(payload),
        }
    }
}

/// A collection of arguments used for building payloads.
///
/// This struct encapsulates the essential components and configuration required for the payload
/// building process. It holds references to the Ethereum client, transaction pool, cached reads,
/// payload configuration, cancellation status, and the best payload achieved so far.
#[derive(Debug)]
pub struct BuildArguments {
    /// Previously cached disk reads
    pub cached_reads: CachedReads,
    /// Optional execution cache shared with the engine.
    pub execution_cache: Option<SavedCache>,
    /// Optional state-root task handle, shared with the engine.
    ///
    /// The preserved trie is shared with the engine, so a concurrent `newPayload` will
    /// block until this task completes. The trie is anchored at the built block's state
    /// root, so if the next `newPayload` is not on top of that block, the trie cache is
    /// invalidated and cleared.
    pub state_root_handle: Option<PayloadStateRootHandle>,
    /// How to configure the payload.
    pub config: PayloadConfig,
    /// A marker that can be used to cancel the job.
    pub cancel: CancelOnDrop,
    /// The best payload achieved so far.
    pub best_payload: Option<BaseBuiltPayload>,
}

impl BuildArguments {
    /// Create new build arguments.
    pub const fn new(
        cached_reads: CachedReads,
        execution_cache: Option<SavedCache>,
        state_root_handle: Option<PayloadStateRootHandle>,
        config: PayloadConfig,
        cancel: CancelOnDrop,
        best_payload: Option<BaseBuiltPayload>,
    ) -> Self {
        Self { cached_reads, execution_cache, state_root_handle, config, cancel, best_payload }
    }
}

/// Checks if the new payload is better than the current best.
///
/// This compares the total fees of the blocks, higher is better.
#[inline(always)]
pub fn is_better_payload(best_payload: Option<&BaseBuiltPayload>, new_fees: U256) -> bool {
    if let Some(best_payload) = best_payload { new_fees > best_payload.fees() } else { true }
}

/// Returns the duration until the given unix timestamp in seconds.
///
/// Returns `Duration::ZERO` if the given timestamp is in the past.
fn duration_until(unix_timestamp_secs: u64) -> Duration {
    let unix_now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let timestamp = Duration::from_secs(unix_timestamp_secs);
    timestamp.saturating_sub(unix_now)
}

#[cfg(test)]
mod tests {
    use base_common_types_chain::BaseBlock;
    use reth_primitives_traits::Block as _;

    use super::*;

    #[tokio::test]
    async fn resolve_waits_for_pending_base_payload() {
        let (sender, receiver) = oneshot::channel();
        let mut resolving = Box::pin(ResolveBestPayload {
            best_payload: None,
            maybe_better: Some(PendingPayload::new(CancelOnDrop::default(), receiver)),
        });
        assert!(resolving.as_mut().now_or_never().is_none());

        let id = PayloadId::new([7; 8]);
        let payload = BaseBuiltPayload::new(
            id,
            Arc::new(BaseBlock::default().seal_slow()),
            U256::ZERO,
            None,
            None,
        );
        sender.send(Ok(BuildOutcome::Freeze(payload))).unwrap();
        assert_eq!(resolving.await.unwrap().id(), id);
    }

    #[tokio::test]
    async fn resolve_retains_best_when_pending_build_does_not_improve() {
        let id = PayloadId::new([9; 8]);
        let payload = BaseBuiltPayload::new(
            id,
            Arc::new(BaseBlock::default().seal_slow()),
            U256::from(100),
            None,
            None,
        );
        let (sender, receiver) = oneshot::channel();
        sender
            .send(Ok(BuildOutcome::Aborted {
                fees: U256::from(50),
                cached_reads: CachedReads::default(),
            }))
            .unwrap();
        let resolved = ResolveBestPayload {
            best_payload: Some(payload),
            maybe_better: Some(PendingPayload::new(CancelOnDrop::default(), receiver)),
        }
        .await
        .unwrap();
        assert_eq!(resolved.id(), id);
        assert_eq!(resolved.fees(), U256::from(100));
    }
}
