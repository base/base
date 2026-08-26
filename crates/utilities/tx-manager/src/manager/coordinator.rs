//! Single-owner coordinator for pending-slot transitions and worker scheduling.

use std::{
    fmt::Debug,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use alloy_primitives::Address;
use alloy_provider::Provider;
use base_runtime::Runtime;
use futures::{FutureExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, warn};

use super::{
    build::{PreparedTx, TxBuilder},
    pending::{
        PendingAdmission, PendingLedger, PendingWork, ReplacementReason, SweepResolution, VersionId,
    },
    publisher::{PublisherEvent, PublisherGroup},
    sweep::ChainSweeper,
};
use crate::{
    SubmissionHandle, SubmissionId, TxCandidate, TxManagerConfig, TxManagerError, TxManagerResult,
    TxMetrics,
};

/// Public-boundary actions serialized by the lifecycle coordinator.
#[derive(Debug)]
pub enum CoordinatorCommand {
    /// Transfers a fully tracked caller admission into coordinator ownership.
    Submit(PendingAdmission),
    /// Requests cancellation of the oldest committed nonce.
    Cancel(oneshot::Sender<TxManagerResult<()>>),
}

/// Results returned from network and signing workers to the coordinator.
#[derive(Debug)]
pub enum WorkerEvent {
    /// Completion of initial construction for a staged submission.
    InitialBuilt {
        /// Submission that owned the initial-build worker.
        submission_id: SubmissionId,
        /// Signed bytes or the terminal construction error.
        result: TxManagerResult<PreparedTx>,
    },
    /// Completion of successor construction for an existing nonce slot.
    ReplacementBuilt {
        /// Submission whose slot requested the successor.
        submission_id: SubmissionId,
        /// Version that was current when construction started.
        base_version: VersionId,
        /// Reason used to interpret the resulting version.
        reason: ReplacementReason,
        /// Signed successor or construction failure.
        result: TxManagerResult<PreparedTx>,
    },
    /// Latest account nonce read after `NonceTooLow`.
    NonceSynced {
        /// Provisional submission requesting recovery.
        submission_id: SubmissionId,
        /// Version that observed the rejection.
        version: VersionId,
        /// Latest canonical nonce or chain-read failure.
        result: TxManagerResult<u64>,
    },
    /// Canonical resolution of the oldest committed prefix.
    Swept(TxManagerResult<Vec<SweepResolution>>),
    /// Panic from a supervised worker that cannot safely be ignored.
    Fatal(&'static str),
}

/// Cloneable command boundary shared by all [`crate::SimpleTxManager`] handles.
#[derive(Debug, Clone)]
pub struct CoordinatorHandle {
    /// Non-blocking command path into the single-owner event loop.
    commands: mpsc::UnboundedSender<CoordinatorCommand>,
    /// Shared monotonic submission-ID allocator.
    next_submission_id: Arc<AtomicU64>,
    /// Admission gate shared with every cloned manager handle.
    closed: Arc<AtomicBool>,
}

impl CoordinatorHandle {
    /// Enqueues a submission without performing RPC or signing work.
    pub fn submit(&self, candidate: TxCandidate) -> SubmissionHandle {
        // Allocate IDs at the API boundary so the caller receives a stable
        // handle even if enqueueing or coordinator startup has failed.
        let id =
            self.next_submission_id.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            });
        let id = match id {
            Ok(id) => SubmissionId::new(id),
            Err(_) => {
                let (admission, handle) =
                    PendingAdmission::new(SubmissionId::new(u64::MAX), candidate);
                admission.reject(TxManagerError::SubmissionIdOverflow);
                return handle;
            }
        };
        let (admission, handle) = PendingAdmission::new(id, candidate);
        if self.closed.load(Ordering::Acquire) {
            admission.reject(TxManagerError::ChannelClosed);
            return handle;
        }
        if let Err(error) = self.commands.send(CoordinatorCommand::Submit(admission)) {
            let CoordinatorCommand::Submit(admission) = error.0 else {
                unreachable!("submit send failure returns the submitted command")
            };
            admission.reject(TxManagerError::ChannelClosed);
            return handle;
        }

        handle
    }

    /// Requests cancellation and waits until cancellation bytes may be live.
    ///
    /// Canonical confirmation continues in the coordinator after this method
    /// returns. A definitive build or publication rejection is returned.
    pub async fn cancel(&self) -> TxManagerResult<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(TxManagerError::ChannelClosed);
        }
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(CoordinatorCommand::Cancel(tx))
            .map_err(|_| TxManagerError::ChannelClosed)?;
        rx.await.unwrap_or(Err(TxManagerError::ChannelClosed))
    }
}

/// Stateless services used by coordinator-owned worker tasks.
#[derive(Debug)]
pub struct CoordinatorWorkers<P, R> {
    /// Transaction construction and signing service.
    pub builder: TxBuilder<P, R>,
    /// Canonical chain-resolution service.
    pub sweeper: ChainSweeper<P, R>,
    /// Symmetric nonce-ordered publication workers.
    pub publishers: PublisherGroup,
}

/// Owns the complete mutable transaction lifecycle state.
#[derive(Debug)]
pub struct TxCoordinator<P, R> {
    /// Sole mutable owner of nonce and submission lifecycle state.
    ledger: PendingLedger,
    /// Services delegated to supervised workers.
    workers: CoordinatorWorkers<P, R>,
    /// Address used by explicit self-transfer cancellation.
    sender: Address,
    /// Runtime used for clocks, workers, and shutdown.
    runtime: R,
    /// Scheduling and timeout policy shared by coordinator workers.
    config: TxManagerConfig,
    /// Transaction-manager metrics sink.
    metrics: Arc<dyn TxMetrics>,
    /// Public commands received from manager handles.
    commands: mpsc::UnboundedReceiver<CoordinatorCommand>,
    /// Results returned by supervised workers.
    events: mpsc::UnboundedReceiver<WorkerEvent>,
    /// Cloneable sender captured by supervised workers.
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
    /// Publication outcomes returned independently by backend workers.
    publisher_events: mpsc::UnboundedReceiver<PublisherEvent>,
    /// Shared admission state visible at the public boundary.
    closed: Arc<AtomicBool>,
    /// Guards against overlapping canonical snapshots.
    sweep_in_progress: bool,
    /// Whether graceful close is waiting for all owned work to resolve.
    closing: bool,
}

impl<P, R> TxCoordinator<P, R>
where
    P: Provider + Clone + Debug + Send + Sync + 'static,
    R: Runtime,
{
    /// Creates a coordinator and its cloneable command handle.
    pub fn new(
        ledger: PendingLedger,
        workers: CoordinatorWorkers<P, R>,
        publisher_events: mpsc::UnboundedReceiver<PublisherEvent>,
        sender: Address,
        runtime: R,
        config: TxManagerConfig,
        metrics: Arc<dyn TxMetrics>,
    ) -> (Self, CoordinatorHandle) {
        let (command_tx, commands) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let closed = Arc::new(AtomicBool::new(false));
        let handle = CoordinatorHandle {
            commands: command_tx,
            next_submission_id: Arc::new(AtomicU64::new(0)),
            closed: Arc::clone(&closed),
        };
        (
            Self {
                ledger,
                workers,
                sender,
                runtime,
                config,
                metrics,
                commands,
                events,
                event_tx,
                publisher_events,
                closed,
                sweep_in_progress: false,
                closing: false,
            },
            handle,
        )
    }

    /// Runs until an explicit close drains or runtime cancellation aborts all waiters.
    pub async fn run(mut self) {
        // Independent clocks wake publication scheduling and canonical
        // resolution without coupling those cadences to command traffic.
        let mut publish_ticks = self.runtime.interval(self.config.publish_retry_delay);
        let mut sweep_ticks = self.runtime.interval(self.config.receipt_query_interval);
        let mut commands_open = true;
        loop {
            // Commands are biased so admission and close decisions are applied
            // before scheduling another network operation.
            tokio::select! {
                biased;
                command = self.commands.recv(), if commands_open => {
                    match command {
                        Some(command) => self.handle_command(command),
                        None => {
                            commands_open = false;
                            self.begin_close();
                        }
                    }
                }
                event = self.events.recv() => {
                    if let Some(event) = event {
                        self.handle_event(event);
                    }
                }
                event = self.publisher_events.recv() => {
                    match event {
                        Some(event) => self.ledger.published(event, self.runtime.now()),
                        None => {
                            if !self.runtime.is_cancelled() {
                                error!("all transaction publisher workers stopped");
                            }
                            self.closed.store(true, Ordering::Release);
                            self.ledger.abort();
                            break;
                        }
                    }
                }
                _ = publish_ticks.next() => {}
                _ = sweep_ticks.next() => {
                    self.start_sweep();
                }
                _ = self.runtime.cancelled() => {
                    self.closed.store(true, Ordering::Release);
                    self.ledger.abort();
                    break;
                }
            }

            // Every external observation is reduced into pure ledger state
            // before another work plan is produced.
            self.start_planned_work();
            self.workers.publishers.update(self.ledger.publisher_snapshot());
            if self.ledger.sweep_requested() {
                self.start_sweep();
            }
            if self.closing && self.ledger.is_empty() {
                break;
            }
        }
        debug!("transaction coordinator stopped");
    }

    /// Applies one public command to the single-owner pending state.
    pub fn handle_command(&mut self, command: CoordinatorCommand) {
        match command {
            CoordinatorCommand::Submit(admission) if !self.closing => {
                self.ledger.submit(admission);
            }
            CoordinatorCommand::Submit(admission) => {
                admission.reject(TxManagerError::ChannelClosed);
            }
            CoordinatorCommand::Cancel(result) if !self.closing => {
                self.ledger.cancel(self.sender, result);
            }
            CoordinatorCommand::Cancel(result) => {
                let _ = result.send(Err(TxManagerError::ChannelClosed));
            }
        }
    }

    /// Applies one supervised worker result and discards stale versions safely.
    pub fn handle_event(&mut self, event: WorkerEvent) {
        let now = self.runtime.now();
        match event {
            WorkerEvent::InitialBuilt { submission_id, result } => {
                self.ledger.initial_built(submission_id, result, now);
            }
            WorkerEvent::ReplacementBuilt { submission_id, base_version, reason, result } => {
                if result.is_ok() && matches!(reason, ReplacementReason::FeeBump) {
                    self.metrics.record_gas_bump();
                }
                self.ledger.replacement_built(submission_id, base_version, reason, result, now);
            }
            WorkerEvent::NonceSynced { submission_id, version, result } => {
                self.ledger.nonce_synced(submission_id, version, result, now);
            }
            WorkerEvent::Swept(result) => {
                self.sweep_in_progress = false;
                match result {
                    Ok(resolutions) => {
                        for resolution in &resolutions {
                            if matches!(
                                resolution.outcome,
                                super::pending::SweepOutcome::Confirmed { .. }
                            ) {
                                self.metrics.record_tx_confirmed();
                            }
                        }
                        self.ledger.apply_sweep(resolutions);
                    }
                    Err(error) => {
                        warn!(error_kind = error.kind(), "pending transaction sweep failed");
                        self.ledger.apply_sweep(Vec::new());
                    }
                }
            }
            WorkerEvent::Fatal(worker) => {
                // A lost build or sweep result would leave coordinator-owned
                // in-progress state wedged, so the whole manager must stop.
                error!(worker, "transaction manager worker panicked; stopping coordinator");
                self.closing = true;
                self.closed.store(true, Ordering::Release);
                self.ledger.abort();
            }
        }
    }

    /// Spawns every action selected by the pure pending-state planner.
    pub fn start_planned_work(&mut self) {
        for work in self.ledger.plan(self.runtime.now()) {
            match work {
                PendingWork::BuildInitial { submission_id, nonce, candidate, .. } => {
                    let builder = self.workers.builder.clone();
                    self.spawn_worker(
                        "initial builder",
                        async move { builder.prepare_initial(&candidate, nonce).await },
                        move |result| WorkerEvent::InitialBuilt { submission_id, result },
                    );
                }
                PendingWork::BuildReplacement {
                    submission_id,
                    base_version,
                    nonce,
                    candidate,
                    base,
                    reason,
                } => {
                    let builder = self.workers.builder.clone();
                    self.spawn_worker(
                        "replacement builder",
                        async move {
                            builder.prepare_replacement(&candidate, &base, nonce, reason).await
                        },
                        move |result| WorkerEvent::ReplacementBuilt {
                            submission_id,
                            base_version,
                            reason,
                            result,
                        },
                    );
                }
                PendingWork::SyncNonce { submission_id, version } => {
                    let sweeper = self.workers.sweeper.clone();
                    self.spawn_worker(
                        "nonce synchronizer",
                        async move { sweeper.latest_nonce().await },
                        move |result| WorkerEvent::NonceSynced { submission_id, version, result },
                    );
                }
            }
        }
    }

    /// Starts one canonical sweep unless another snapshot is already active.
    pub fn start_sweep(&mut self) {
        if self.sweep_in_progress {
            return;
        }
        let targets = self.ledger.sweep_targets();
        if targets.is_empty() {
            return;
        }
        self.sweep_in_progress = true;
        self.ledger.start_sweep();
        let sweeper = self.workers.sweeper.clone();
        self.spawn_worker(
            "chain sweeper",
            async move { sweeper.sweep(targets).await },
            WorkerEvent::Swept,
        );
    }

    /// Supervises one worker and converts panic or runtime cancellation explicitly.
    pub fn spawn_worker<F, T, M>(&self, name: &'static str, future: F, event: M)
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
        M: FnOnce(T) -> WorkerEvent + Send + 'static,
    {
        let runtime = self.runtime.clone();
        let events = self.event_tx.clone();
        self.runtime.spawn(async move {
            tokio::select! {
                _ = runtime.cancelled() => {}
                outcome = AssertUnwindSafe(future).catch_unwind() => {
                    let event = outcome.map_or(WorkerEvent::Fatal(name), event);
                    let _ = events.send(event);
                }
            }
        });
    }

    /// Rejects new work while allowing potentially-live nonce slots to resolve.
    pub fn begin_close(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.closed.store(true, Ordering::Release);
        self.ledger.close();
    }
}
