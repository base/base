//! Support for building payloads.
//!
//! The payload builder is responsible for building payloads.
//! Once a new payload is created, it is continuously updated.

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use alloy_primitives::{B256, BlockTimestamp};
use base_common_types_chain::BlockHeader;
use base_common_types_payload::PayloadId;
use base_execution_payload_types::{
    BaseBuiltPayload, BasePayloadBuilderAttributes, Events, PayloadBuilderError, PayloadEvents,
    PayloadKind,
};
use base_execution_state_tasks::PayloadStateRootHandle;
use futures_util::{Stream, StreamExt, future::FutureExt};
use reth_chain_state::CanonStateNotification;
use base_execution_state_tasks::SavedCache;
use reth_primitives_traits::FastInstant as Instant;
use tokio::sync::{
    broadcast, mpsc,
    oneshot::{self, Receiver},
    watch,
};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{Span, debug, debug_span, info, trace, warn};

use crate::{
    BasicPayloadJob, BasicPayloadJobGenerator, KeepPayloadJobAlive, PayloadBuilderServiceMetrics,
    PayloadJob,
};

pub type PayloadFuture =
    Pin<Box<dyn Future<Output = Result<BaseBuiltPayload, PayloadBuilderError>> + Send>>;
type ResolvePayloadResult<Job> = (Option<PayloadFuture>, Option<PayloadJobEntry<Job>>);

/// A communication channel to the [`PayloadBuilderService`] that can retrieve payloads.
///
/// This type is intended to be used to retrieve payloads from the service (e.g. from the engine
/// API).
#[derive(Debug)]
pub struct PayloadStore {
    inner: Arc<PayloadBuilderHandle>,
}

impl PayloadStore {
    /// Resolves the payload job and returns the best payload that has been built so far.
    ///
    /// Note: depending on the installed [`BasicPayloadJobGenerator`], this may or may not terminate the
    /// job, See [`PayloadJob::resolve`].
    pub fn resolve_kind(
        &self,
        id: PayloadId,
        kind: PayloadKind,
    ) -> impl Future<Output = Option<Result<BaseBuiltPayload, PayloadBuilderError>>> {
        self.inner.resolve_kind(id, kind)
    }

    /// Resolves the payload job and returns the best payload that has been built so far.
    pub async fn resolve(
        &self,
        id: PayloadId,
    ) -> Option<Result<BaseBuiltPayload, PayloadBuilderError>> {
        self.resolve_kind(id, PayloadKind::Earliest).await
    }

    /// Returns the best payload for the given identifier.
    ///
    /// Note: this merely returns the best payload so far and does not resolve the job.
    pub async fn best_payload(
        &self,
        id: PayloadId,
    ) -> Option<Result<BaseBuiltPayload, PayloadBuilderError>> {
        self.inner.best_payload(id).await
    }

    /// Returns the payload timestamp associated with the given identifier.
    ///
    /// Note: this returns the timestamp of the payload and does not resolve the job.
    pub async fn payload_timestamp(
        &self,
        id: PayloadId,
    ) -> Option<Result<u64, PayloadBuilderError>> {
        self.inner.payload_timestamp(id).await
    }

    /// Create a new instance
    pub fn new(inner: PayloadBuilderHandle) -> Self {
        Self { inner: Arc::new(inner) }
    }
}

impl From<PayloadBuilderHandle> for PayloadStore {
    fn from(inner: PayloadBuilderHandle) -> Self {
        Self::new(inner)
    }
}

/// A communication channel to the [`PayloadBuilderService`].
///
/// This is the API used to create new payloads and to get the current state of existing ones.
#[derive(Debug)]
pub struct PayloadBuilderHandle {
    /// Sender half of the message channel to the [`PayloadBuilderService`].
    to_service: mpsc::UnboundedSender<PayloadServiceCommand>,
}

impl PayloadBuilderHandle {
    /// Creates a new payload builder handle for the given channel.
    ///
    /// Note: this is only used internally by the [`PayloadBuilderService`] to manage the payload
    /// building flow See [`PayloadBuilderService::poll`] for implementation details.
    pub const fn new(to_service: mpsc::UnboundedSender<PayloadServiceCommand>) -> Self {
        Self { to_service }
    }

    /// Sends a message to the service to start building a new payload for the given payload.
    ///
    /// Returns a receiver that will receive the payload id.
    pub fn send_new_payload(
        &self,
        input: BuildNewPayload,
    ) -> Receiver<Result<PayloadId, PayloadBuilderError>> {
        let (tx, rx) = oneshot::channel();
        let span = debug_span!(parent: Span::current(), "payload_job");
        let _ =
            self.to_service.send(PayloadServiceCommand::BuildNewPayload(input.into(), span, tx));
        rx
    }

    /// Returns the best payload for the given identifier.
    /// Note: this does not resolve the job if it's still in progress.
    pub async fn best_payload(
        &self,
        id: PayloadId,
    ) -> Option<Result<BaseBuiltPayload, PayloadBuilderError>> {
        let (tx, rx) = oneshot::channel();
        self.to_service.send(PayloadServiceCommand::BestPayload(id, tx)).ok()?;
        rx.await.ok()?
    }

    /// Resolves the payload job and returns the best payload that has been built so far.
    ///
    /// # Cancellation safety
    ///
    /// The future returned by this method is not cancellation-safe. This method sends the resolve
    /// command before returning the future, so dropping the returned future drops the response
    /// receiver and cancels the job identified by `id`.
    pub fn resolve_kind(
        &self,
        id: PayloadId,
        kind: PayloadKind,
    ) -> impl Future<Output = Option<Result<BaseBuiltPayload, PayloadBuilderError>>> {
        let (tx, rx) = oneshot::channel();
        let sent = self.to_service.send(PayloadServiceCommand::Resolve(id, kind, tx)).is_ok();
        async move {
            if !sent {
                return None;
            }

            match rx.await.transpose()? {
                Ok(fut) => Some(fut.await),
                Err(e) => Some(Err(e.into())),
            }
        }
    }

    /// Sends a message to the service to subscribe to payload events.
    /// Returns a receiver that will receive them.
    pub async fn subscribe(&self) -> Result<PayloadEvents, PayloadBuilderError> {
        let (tx, rx) = oneshot::channel();
        let _ = self.to_service.send(PayloadServiceCommand::Subscribe(tx));
        Ok(PayloadEvents { receiver: rx.await? })
    }

    /// Returns the payload timestamp associated with the given identifier.
    ///
    /// Note: this returns the timestamp of the payload and does not resolve the job.
    pub async fn payload_timestamp(
        &self,
        id: PayloadId,
    ) -> Option<Result<u64, PayloadBuilderError>> {
        let (tx, rx) = oneshot::channel();
        self.to_service.send(PayloadServiceCommand::PayloadTimestamp(id, tx)).ok()?;
        rx.await.ok()?
    }
}

impl Clone for PayloadBuilderHandle {
    fn clone(&self) -> Self {
        Self { to_service: self.to_service.clone() }
    }
}

/// A service that manages payload building tasks.
///
/// This type is an endless future that manages the building of payloads.
///
/// It tracks active payloads and their build jobs that run in a worker pool.
///
/// By design, this type relies entirely on the [`BasicPayloadJobGenerator`] to create new payloads and
/// does know nothing about how to build them, it just drives their jobs to completion.
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct PayloadBuilderService<Client, Pool, St>
where
    Client: base_execution_state_api::StateProviderFactory
        + base_execution_state_api::BlockReaderIdExt
        + base_common_chain_config::ChainSpecProvider
        + Clone
        + Unpin
        + 'static,
    Pool: base_execution_txpool::TransactionPool + Unpin + 'static,
    Pool: base_execution_txpool::ParkableTransactionPool,
{
    /// The type that knows how to create new payloads.
    generator: BasicPayloadJobGenerator<Client, Pool>,
    /// All active payload jobs, each accompanied by its id and the caller's tracing span
    /// propagated across the channel so that poll and resolve work appears as children of the
    /// original Engine API request.
    payload_jobs: Vec<PayloadJobEntry<BasicPayloadJob<Pool, Client>>>,
    /// Copy of the sender half, so new [`PayloadBuilderHandle`] can be created on demand.
    service_tx: mpsc::UnboundedSender<PayloadServiceCommand>,
    /// Receiver half of the command channel.
    command_rx: UnboundedReceiverStream<PayloadServiceCommand>,
    /// Metrics for the payload builder service
    metrics: PayloadBuilderServiceMetrics,
    /// Chain events notification stream
    chain_events: St,
    /// Payload events handler, used to broadcast and subscribe to payload events.
    payload_events: broadcast::Sender<Events>,
    /// We retain latest resolved payload just to make sure that we can handle repeating
    /// requests for it gracefully.
    cached_payload_rx: watch::Receiver<Option<(PayloadId, BlockTimestamp, BaseBuiltPayload)>>,
    /// Sender half of the cached payload channel.
    cached_payload_tx: watch::Sender<Option<(PayloadId, BlockTimestamp, BaseBuiltPayload)>>,
}

const PAYLOAD_EVENTS_BUFFER_SIZE: usize = 20;

// === impl PayloadBuilderService ===

impl<Client, Pool, St> PayloadBuilderService<Client, Pool, St>
where
    Client: base_execution_state_api::StateProviderFactory
        + base_execution_state_api::BlockReaderIdExt
        + base_common_chain_config::ChainSpecProvider
        + Clone
        + Unpin
        + 'static,
    Pool: base_execution_txpool::TransactionPool + Unpin + 'static,
    Pool: base_execution_txpool::ParkableTransactionPool,
{
    /// Creates a new payload builder service and returns the [`PayloadBuilderHandle`] to interact
    /// with it.
    ///
    /// This also takes a stream of chain events that will be forwarded to the generator to apply
    /// additional logic when new state is committed. See also
    /// [`BasicPayloadJobGenerator::on_new_state`].
    pub fn new(
        generator: BasicPayloadJobGenerator<Client, Pool>,
        chain_events: St,
    ) -> (Self, PayloadBuilderHandle) {
        let (service_tx, command_rx) = mpsc::unbounded_channel();
        let (payload_events, _) = broadcast::channel(PAYLOAD_EVENTS_BUFFER_SIZE);

        let (cached_payload_tx, cached_payload_rx) = watch::channel(None);

        let service = Self {
            generator,
            payload_jobs: Vec::new(),
            service_tx,
            command_rx: UnboundedReceiverStream::new(command_rx),
            metrics: Default::default(),
            chain_events,
            payload_events,
            cached_payload_rx,
            cached_payload_tx,
        };

        let handle = service.handle();
        (service, handle)
    }

    /// Returns a handle to the service.
    pub fn handle(&self) -> PayloadBuilderHandle {
        PayloadBuilderHandle::new(self.service_tx.clone())
    }

    /// Create clone on `payload_events` sending handle that could be used by builder to produce
    /// additional events during block building
    pub fn payload_events_handle(&self) -> broadcast::Sender<Events> {
        self.payload_events.clone()
    }

    /// Returns true if the given payload is currently being built.
    fn contains_payload(&self, id: PayloadId) -> bool {
        self.payload_jobs.iter().any(|entry| entry.id == id)
    }

    /// Returns the best payload for the given identifier that has been built so far.
    fn best_payload(&self, id: PayloadId) -> Option<Result<BaseBuiltPayload, PayloadBuilderError>> {
        let res = self
            .payload_jobs
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.job.best_payload());
        if let Some(Ok(ref best)) = res {
            self.metrics.set_best_revenue(best.block().number(), f64::from(best.fees()));
        }

        res
    }

    /// Returns the best payload for the given identifier that has been built so far.
    ///
    /// If the job should be terminated, this removes it from active polling and returns it so the
    /// caller can drop it after the response is sent.
    fn resolve(
        &mut self,
        id: PayloadId,
        kind: PayloadKind,
    ) -> ResolvePayloadResult<BasicPayloadJob<Pool, Client>> {
        let start = Instant::now();
        debug!(target: "payload_builder", %id, "resolving payload job");

        if let Some((cached, _, payload)) = &*self.cached_payload_rx.borrow()
            && *cached == id
        {
            self.metrics.resolve_duration_seconds.record(start.elapsed());
            return (Some(Box::pin(core::future::ready(Ok(payload.clone())))), None);
        }

        let Some(job) = self.payload_jobs.iter().position(|entry| entry.id == id) else {
            return (None, None);
        };
        let (fut, keep_alive) = self.payload_jobs[job].job.resolve_kind(kind);
        let payload_timestamp = self.payload_jobs[job].job.payload_timestamp();

        let mut resolved_job =
            (keep_alive == KeepPayloadJobAlive::No).then(|| self.payload_jobs.swap_remove(job));
        let leases = resolved_job
            .as_mut()
            .map(|entry| std::mem::take(&mut entry.leases))
            .unwrap_or_default();

        // Since the fees will not be known until the payload future is resolved / awaited, we wrap
        // the future in a new future that will update the metrics.
        let resolved_metrics = self.metrics.clone();
        let payload_events = self.payload_events.clone();
        let cached_payload_tx = self.cached_payload_tx.clone();

        let fut = async move {
            let _leases = leases;
            let res = fut.await;
            resolved_metrics.resolve_duration_seconds.record(start.elapsed());
            if let Ok(payload) = &res {
                if payload_events.receiver_count() > 0 {
                    payload_events.send(Events::BuiltPayload(payload.clone())).ok();
                }

                if let Ok(timestamp) = payload_timestamp {
                    let _ = cached_payload_tx.send(Some((id, timestamp, payload.clone())));
                }

                resolved_metrics
                    .set_resolved_revenue(payload.block().number(), f64::from(payload.fees()));
            }
            res
        };

        (Some(Box::pin(fut)), resolved_job)
    }

    /// Returns the payload timestamp for the given payload.
    fn payload_timestamp(&self, id: PayloadId) -> Option<Result<u64, PayloadBuilderError>> {
        if let Some((cached_id, timestamp, _)) = *self.cached_payload_rx.borrow()
            && cached_id == id
        {
            return Some(Ok(timestamp));
        }

        let timestamp = self
            .payload_jobs
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.job.payload_timestamp());

        if timestamp.is_none() {
            trace!(target: "payload_builder", %id, "no matching payload job found to get timestamp for");
        }

        timestamp
    }
}

impl<Client, Pool, St> Future for PayloadBuilderService<Client, Pool, St>
where
    Client: base_execution_state_api::StateProviderFactory
        + base_execution_state_api::BlockReaderIdExt
        + base_common_chain_config::ChainSpecProvider
        + Clone
        + Unpin
        + 'static,
    Pool: base_execution_txpool::TransactionPool + Unpin + 'static,
    Pool: base_execution_txpool::ParkableTransactionPool,
    St: Stream<Item = CanonStateNotification> + Send + Unpin + 'static,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            // notify the generator of new chain events
            while let Poll::Ready(Some(new_head)) = this.chain_events.poll_next_unpin(cx) {
                this.generator.on_new_state(new_head);
            }

            // we poll all jobs first, so we always have the latest payload that we can report if
            // requests
            // we don't care about the order of the jobs, so we can just swap_remove them
            for idx in (0..this.payload_jobs.len()).rev() {
                let PayloadJobEntry { mut job, id, span, leases } =
                    this.payload_jobs.swap_remove(idx);

                let poll_result = {
                    let _entered = span.enter();
                    job.poll_unpin(cx)
                };

                match poll_result {
                    Poll::Ready(Ok(_)) => {
                        this.metrics.set_active_jobs(this.payload_jobs.len());
                        trace!(target: "payload_builder", %id, "payload job finished");
                    }
                    Poll::Ready(Err(err)) => {
                        warn!(target: "payload_builder",%err, ?id, "Payload builder job failed; resolving payload");
                        this.metrics.inc_failed_jobs();
                        this.metrics.set_active_jobs(this.payload_jobs.len());
                    }
                    Poll::Pending => {
                        this.payload_jobs.push(PayloadJobEntry { job, id, span, leases });
                    }
                }
            }

            // marker for exit condition
            let mut new_job = false;

            // drain all requests
            while let Poll::Ready(Some(cmd)) = this.command_rx.poll_next_unpin(cx) {
                match cmd {
                    PayloadServiceCommand::BuildNewPayload(input, job_span, tx) => {
                        let id = input.payload_id();
                        let mut res = Ok(id);
                        let parent = input.parent_hash;

                        if this.contains_payload(id) {
                            debug!(target: "payload_builder", %id, %parent, "Payload job already in progress, ignoring.");
                        } else {
                            let start = Instant::now();
                            let attributes = input.attributes.clone();
                            // Keep a service-owned reference for generic payload jobs. Builders
                            // that move work into detached tasks can retain their own references
                            // from `resources` until those tasks exit.
                            let leases = input.resources.clone_leases();
                            let job_result = {
                                let _entered = job_span.enter();
                                this.generator.new_payload_job(*input, id)
                            };

                            match job_result {
                                Ok(job) => {
                                    this.metrics.new_job_duration_seconds.record(start.elapsed());
                                    info!(target: "payload_builder", %id, %parent, "New payload job created");
                                    this.metrics.inc_initiated_jobs();
                                    new_job = true;
                                    this.payload_jobs.push(PayloadJobEntry {
                                        job,
                                        id,
                                        span: job_span,
                                        leases,
                                    });
                                    this.payload_events.send(Events::Attributes(attributes)).ok();

                                    // Clear stale cached payload for this id so
                                    // resolve() never returns an outdated result
                                    // from a previous job with the same id.
                                    if this
                                        .cached_payload_rx
                                        .borrow()
                                        .as_ref()
                                        .is_some_and(|(cached_id, _, _)| *cached_id == id)
                                    {
                                        trace!(target: "payload_builder", %id, "clearing stale cached payload for reused payload id");
                                        let _ = this.cached_payload_tx.send(None);
                                    }
                                }
                                Err(err) => {
                                    this.metrics.new_job_duration_seconds.record(start.elapsed());
                                    this.metrics.inc_failed_jobs();
                                    warn!(target: "payload_builder", %err, %id, "Failed to create payload builder job");
                                    res = Err(err);
                                }
                            }
                        }

                        let _ = tx.send(res);
                    }
                    PayloadServiceCommand::BestPayload(id, tx) => {
                        let _ = tx.send(this.best_payload(id));
                    }
                    PayloadServiceCommand::PayloadTimestamp(id, tx) => {
                        let timestamp = this.payload_timestamp(id);
                        let _ = tx.send(timestamp);
                    }
                    PayloadServiceCommand::Resolve(id, strategy, tx) => {
                        let (payload_fut, resolved_job) = this.resolve(id, strategy);
                        let _ = tx.send(payload_fut);

                        if let Some(entry) = resolved_job {
                            debug!(target: "payload_builder", id = %entry.id, "terminated resolved job");
                        }
                    }
                    PayloadServiceCommand::Subscribe(tx) => {
                        let new_rx = this.payload_events.subscribe();
                        let _ = tx.send(new_rx);
                    }
                }
            }

            if !new_job {
                return Poll::Pending;
            }
        }
    }
}

/// Message type for the [`PayloadBuilderService`].
#[derive(derive_more::Debug)]
pub enum PayloadServiceCommand {
    /// Start building a new payload.
    ///
    /// Carries the caller's [`Span`] so the service can parent payload-building work under the
    /// originating Engine API trace.
    BuildNewPayload(
        Box<BuildNewPayload>,
        Span,
        oneshot::Sender<Result<PayloadId, PayloadBuilderError>>,
    ),
    /// Get the best payload so far
    BestPayload(PayloadId, oneshot::Sender<Option<Result<BaseBuiltPayload, PayloadBuilderError>>>),
    /// Get the payload timestamp for the given payload
    PayloadTimestamp(PayloadId, oneshot::Sender<Option<Result<u64, PayloadBuilderError>>>),
    /// Resolve the payload and return the payload
    Resolve(
        PayloadId,
        /* kind: */ PayloadKind,
        #[debug(skip)] oneshot::Sender<Option<PayloadFuture>>,
    ),
    /// Payload service events
    Subscribe(oneshot::Sender<broadcast::Receiver<Events>>),
}

/// A request to build a new payload.
#[derive(Debug)]
pub struct BuildNewPayload {
    /// The attributes for the new payload
    pub attributes: BasePayloadBuilderAttributes,
    /// The parent hash of the new payload
    pub parent_hash: B256,
    /// Resources loaned to the payload builder for this job.
    pub resources: PayloadBuilderResources,
}

impl BuildNewPayload {
    /// Returns the payload id for the new payload.
    pub fn payload_id(&self) -> PayloadId {
        self.attributes.payload_id(&self.parent_hash)
    }
}

/// Resources loaned to a payload builder job by the engine.
#[derive(Debug, Default)]
pub struct PayloadBuilderResources {
    /// Optional execution cache to use for the payload.
    ///
    /// Only provided if `--engine.share-execution-cache-with-payload-builder` is enabled.
    execution_cache: Option<SavedCache>,
    /// Optional handle to a background state-root task.
    state_root_handle: Option<PayloadStateRootHandle>,
    /// Lifecycle leases retained by the service or by detached payload build tasks.
    leases: Vec<PayloadBuilderLease>,
}

impl PayloadBuilderResources {
    /// Creates a new payload builder resource bundle.
    pub const fn new(
        execution_cache: Option<SavedCache>,
        state_root_handle: Option<PayloadStateRootHandle>,
    ) -> Self {
        Self { execution_cache, state_root_handle, leases: Vec::new() }
    }

    /// Adds a lease for this payload build.
    pub fn with_lease(mut self, lease: PayloadBuilderLease) -> Self {
        self.leases.push(lease);
        self
    }

    /// Returns the loaned execution cache, if any.
    pub const fn execution_cache(&self) -> Option<&SavedCache> {
        self.execution_cache.as_ref()
    }

    /// Takes the loaned execution cache, if any.
    pub const fn take_execution_cache(&mut self) -> Option<SavedCache> {
        self.execution_cache.take()
    }

    /// Returns the loaned state-root task handle, if any.
    pub const fn state_root_handle(&self) -> Option<&PayloadStateRootHandle> {
        self.state_root_handle.as_ref()
    }

    /// Takes the loaned state-root task handle, if any.
    pub const fn take_state_root_handle(&mut self) -> Option<PayloadStateRootHandle> {
        self.state_root_handle.take()
    }

    /// Takes lifecycle leases for a payload job that owns detached work.
    pub fn take_leases(&mut self) -> Vec<PayloadBuilderLease> {
        std::mem::take(&mut self.leases)
    }

    /// Clones lifecycle leases for the payload builder service to retain.
    fn clone_leases(&self) -> Vec<PayloadBuilderLease> {
        self.leases.clone()
    }
}

/// Keeps a loaned resource active until the last lease clone is dropped.
#[derive(Clone)]
pub struct PayloadBuilderLease {
    _lease: Arc<dyn Send + Sync>,
}

impl PayloadBuilderLease {
    /// Wraps a lease that releases its resource when the last clone is dropped.
    pub fn new(lease: impl Send + Sync + 'static) -> Self {
        Self { _lease: Arc::new(lease) }
    }
}

impl std::fmt::Debug for PayloadBuilderLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PayloadBuilderLease").finish_non_exhaustive()
    }
}

/// An active payload job and its service metadata.
#[derive(Debug)]
struct PayloadJobEntry<Job> {
    job: Job,
    id: PayloadId,
    span: Span,
    leases: Vec<PayloadBuilderLease>,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use alloy_primitives::Address;
    use base_common_chain_config::BaseChainSpec;
    use base_common_runtime_tasks::Runtime;
    use base_common_types_chain::{BaseBlock, Header};
    use base_common_types_payload::PayloadAttributes as EthPayloadAttributes;
    use base_execution_evm_blocks::BaseEvmConfig;
    use base_execution_txpool::{
        BaseOrdering, BaseTransactionPool, BaseTransactionValidator,
        EthTransactionValidatorBuilder, InMemoryBlobStore, Pool,
    };
    use reth_provider::test_utils::MockEthProvider;

    use super::*;
    use crate::{BasePayloadBuilder, BasicPayloadJobGeneratorConfig};

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn payload_builder_lease_is_held_until_resolve_finishes() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(
            async {
                let chain_spec = Arc::new(BaseChainSpec::mainnet());
                let provider = MockEthProvider::default().with_chain_spec((*chain_spec).clone());
                let parent = Header { gas_limit: 30_000_000, ..Default::default() };
                provider.add_block(
                    parent.hash_slow(),
                    BaseBlock { header: parent, body: Default::default() },
                );
                let validator = EthTransactionValidatorBuilder::new(
                    provider.clone(),
                    BaseEvmConfig::new(chain_spec.clone()),
                )
                .build_with_tasks(Runtime::test())
                .map(BaseTransactionValidator::new);
                let pool = Pool::new(
                    validator,
                    BaseOrdering::default(),
                    InMemoryBlobStore::default(),
                    Default::default(),
                );
                let pool = BaseTransactionPool::new(pool, BaseOrdering::default());
                let builder =
                    BasePayloadBuilder::new(pool, provider.clone(), BaseEvmConfig::new(chain_spec));
                let generator = BasicPayloadJobGenerator::with_builder(
                    provider,
                    Runtime::test(),
                    BasicPayloadJobGeneratorConfig::default(),
                    builder,
                );
                let (service, handle) =
                    PayloadBuilderService::new(generator, futures_util::stream::empty());
                let service = tokio::spawn(service);
                let dropped = Arc::new(AtomicBool::new(false));
                let lease = PayloadBuilderLease::new(DropProbe(Arc::clone(&dropped)));
                let input = BuildNewPayload {
                    attributes: EthPayloadAttributes {
                        timestamp: 1,
                        prev_randao: B256::ZERO,
                        suggested_fee_recipient: Address::ZERO,
                        withdrawals: None,
                        parent_beacon_block_root: None,
                        slot_number: None,
                        target_gas_limit: None,
                    }
                    .into(),
                    parent_hash: B256::ZERO,
                    resources: PayloadBuilderResources::default().with_lease(lease),
                };

                let id = handle.send_new_payload(input).await.unwrap().unwrap();
                assert!(!dropped.load(Ordering::Acquire));

                handle.resolve_kind(id, PayloadKind::Earliest).await.unwrap().unwrap();
                assert!(dropped.load(Ordering::Acquire));
                service.abort();
            },
        );
    }
}
