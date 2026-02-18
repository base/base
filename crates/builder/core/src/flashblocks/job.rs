use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures_util::Future;
use parking_lot::Mutex;
use reth_basic_payload_builder::{HeaderForPayload, PayloadConfig};
use reth_node_api::PayloadKind;
use reth_payload_builder::{KeepPayloadJobAlive, PayloadBuilderError, PayloadJob};
use reth_revm::cached::CachedReads;
use reth_tasks::TaskSpawner;
use tokio::{sync::oneshot, time::Sleep};
use tokio_util::sync::CancellationToken;

use crate::{BlockCell, BuildArguments, PayloadBuilder, ResolvePayload};

/// A [`PayloadJob`] that builds empty blocks.
pub struct BlockPayloadJob<Tasks, Builder>
where
    Builder: PayloadBuilder,
{
    /// The configuration for how the payload will be created.
    pub config: PayloadConfig<Builder::Attributes, HeaderForPayload<Builder::BuiltPayload>>,
    /// How to spawn building tasks
    pub executor: Tasks,
    /// The type responsible for building payloads.
    ///
    /// See [`PayloadBuilder`]
    pub builder: Builder,
    /// The cell that holds the built payload (intermediate flashblocks, may not have state root).
    pub cell: BlockCell<Builder::BuiltPayload>,
    /// The cell that holds the finalized payload with state root computed.
    pub finalized_cell: BlockCell<Builder::BuiltPayload>,
    /// Whether to compute state root only on finalization (when `get_payload` is called).
    pub compute_state_root_on_finalize: bool,
    /// Cancellation token for the running job
    pub cancel: CancellationToken,
    /// Mutex to synchronize cancellation with payload publishing.
    pub publish_guard: Arc<Mutex<()>>,
    pub deadline: Pin<Box<Sleep>>, // Add deadline
    pub build_complete: Option<oneshot::Receiver<Result<(), PayloadBuilderError>>>,
    /// Caches all disk reads for the state the new payloads build on
    ///
    /// This is used to avoid reading the same state over and over again when new attempts are
    /// triggered, because during the building process we'll repeatedly execute the transactions.
    pub cached_reads: Option<CachedReads>,
}

impl<Tasks, Builder> std::fmt::Debug for BlockPayloadJob<Tasks, Builder>
where
    Builder: PayloadBuilder,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockPayloadJob")
            .field("compute_state_root_on_finalize", &self.compute_state_root_on_finalize)
            .finish_non_exhaustive()
    }
}

impl<Tasks, Builder> PayloadJob for BlockPayloadJob<Tasks, Builder>
where
    Tasks: TaskSpawner + Clone + 'static,
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
        tracing::info!("Resolve kind {:?}", kind);

        // Acquire mutex before cancelling to synchronize with payload publishing.
        {
            let _guard = self.publish_guard.lock();
            self.cancel.cancel();
        }

        let resolve_future = if self.compute_state_root_on_finalize {
            ResolvePayload::new(self.finalized_cell.wait_for_value())
        } else {
            ResolvePayload::new(self.cell.wait_for_value())
        };

        (resolve_future, KeepPayloadJobAlive::No)
    }
}

/// A [`PayloadJob`] is a future that's being polled by the `PayloadBuilderService`
impl<Tasks, Builder> BlockPayloadJob<Tasks, Builder>
where
    Tasks: TaskSpawner + Clone + 'static,
    Builder: PayloadBuilder + Unpin + 'static,
    Builder::Attributes: Unpin + Clone,
    Builder::BuiltPayload: Unpin + Clone,
{
    pub fn spawn_build_job(&mut self) {
        let builder = self.builder.clone();
        let payload_config = self.config.clone();
        let cell = self.cell.clone();
        let cancel = self.cancel.clone();
        let publish_guard = Arc::clone(&self.publish_guard);
        let finalized_cell = self.finalized_cell.clone();
        let compute_state_root_on_finalize = self.compute_state_root_on_finalize;

        let (tx, rx) = oneshot::channel();
        self.build_complete = Some(rx);
        let cached_reads = self.cached_reads.take().unwrap_or_default();
        self.executor.spawn_blocking(Box::pin(async move {
            let args = BuildArguments {
                cached_reads,
                config: payload_config,
                cancel,
                publish_guard,
                finalized_cell,
                compute_state_root_on_finalize,
            };

            let result = builder.try_build(args, cell).await;
            let _ = tx.send(result);
        }));
    }
}

/// A [`PayloadJob`] is a future that's being polled by the `PayloadBuilderService`
impl<Tasks, Builder> Future for BlockPayloadJob<Tasks, Builder>
where
    Tasks: TaskSpawner + Clone + 'static,
    Builder: PayloadBuilder + Unpin + 'static,
    Builder::Attributes: Unpin + Clone,
    Builder::BuiltPayload: Unpin + Clone,
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
