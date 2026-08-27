//! Coordinates pending transaction state and background workers.

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
    pending::{PendingLedger, PendingWork, ReplacementReason, StagedSubmission, VersionId},
    publisher::{PublisherEvent, PublisherGroup},
    sweep::{ChainSweeper, SweepOutcome, SweepResolution},
};
use crate::{
    SubmissionHandle, SubmissionId, TxCandidate, TxManagerConfig, TxManagerError, TxManagerResult,
    TxMetrics,
};

/// Commands sent by [`crate::SimpleTxManager`] to the coordinator.
#[derive(Debug)]
pub enum CoordinatorCommand {
    /// Hands a staged caller submission to the coordinator.
    Submit(StagedSubmission),
    /// Requests cancellation of the oldest committed nonce.
    Cancel(oneshot::Sender<TxManagerResult<()>>),
}

/// Results sent back to the coordinator by background workers.
#[derive(Debug)]
pub enum WorkerEvent {
    /// A staged submission's first transaction was built and signed.
    TxPrepared {
        /// Submission that owned the prepare worker.
        submission_id: SubmissionId,
        /// Signed bytes or the terminal construction error.
        result: TxManagerResult<PreparedTx>,
    },
    /// A successor transaction for an existing nonce slot was built and signed.
    ReplacementTxPrepared {
        /// Submission whose slot requested the successor.
        submission_id: SubmissionId,
        /// Version that was current when construction started.
        base_version: VersionId,
        /// Reason used to interpret the resulting version.
        reason: ReplacementReason,
        /// Signed successor or construction failure.
        result: TxManagerResult<PreparedTx>,
    },
    /// The latest account nonce was fetched after `NonceTooLow`.
    AccountNonceFetched {
        /// Provisional submission requesting recovery.
        submission_id: SubmissionId,
        /// Version that observed the rejection.
        version: VersionId,
        /// Latest canonical nonce or chain-read failure.
        result: TxManagerResult<u64>,
    },
    /// A chain sweep completed.
    SweepCompleted(TxManagerResult<Vec<SweepResolution>>),
    /// A supervised worker panicked.
    WorkerPanicked(&'static str),
}

/// Cloneable handle for sending work to the coordinator.
#[derive(Debug, Clone)]
pub struct CoordinatorHandle {
    /// Sends caller commands to the coordinator event loop.
    commands: mpsc::UnboundedSender<CoordinatorCommand>,
    /// Allocates a unique ID for each submission.
    next_submission_id: Arc<AtomicU64>,
    /// Prevents new work after shutdown starts.
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
            Err(_) => return SubmissionHandle::resolved(Err(TxManagerError::SubmissionIdOverflow)),
        };

        let (staged, handle) = StagedSubmission::new(id, candidate);

        if self.closed.load(Ordering::Acquire) {
            staged.reject(TxManagerError::ChannelClosed);
            return handle;
        }

        if let Err(error) = self.commands.send(CoordinatorCommand::Submit(staged)) {
            let CoordinatorCommand::Submit(staged) = error.0 else {
                unreachable!("submit send failure returns the submitted command")
            };
            staged.reject(TxManagerError::ChannelClosed);
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

/// Services the coordinator uses for preparation, publication, and confirmation.
#[derive(Debug)]
pub struct CoordinatorWorkers<P, R> {
    /// Transaction construction and signing service.
    pub builder: TxBuilder<P, R>,
    /// Canonical chain-resolution service.
    pub sweeper: ChainSweeper<P, R>,
    /// Symmetric nonce-ordered publication workers.
    pub publishers: PublisherGroup,
}

/// Runs the transaction lifecycle event loop and owns the pending ledger.
#[derive(Debug)]
pub struct TxCoordinator<P, R> {
    /// Stores staged submissions and pending nonce slots.
    ledger: PendingLedger,
    /// Services used by background workers.
    workers: CoordinatorWorkers<P, R>,
    /// Address used by explicit self-transfer cancellation.
    sender: Address,
    /// Runtime used for clocks, workers, and shutdown.
    runtime: R,
    /// Scheduling and timeout configuration.
    config: TxManagerConfig,
    /// Transaction-manager metrics sink.
    metrics: Arc<dyn TxMetrics>,
    /// Caller commands received from manager handles.
    commands: mpsc::UnboundedReceiver<CoordinatorCommand>,
    /// Preparation and chain-query results returned by workers.
    events: mpsc::UnboundedReceiver<WorkerEvent>,
    /// Sender used by workers to return results.
    event_tx: mpsc::UnboundedSender<WorkerEvent>,
    /// Publication results returned by backend workers.
    publisher_events: mpsc::UnboundedReceiver<PublisherEvent>,
    /// Tells manager handles whether new work is accepted.
    closed: Arc<AtomicBool>,
    /// Prevents concurrent chain sweeps.
    sweep_in_progress: bool,
    /// Whether shutdown is waiting for pending work to finish.
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

    /// Processes commands and worker results until shutdown completes.
    pub async fn run(mut self) {
        // Retry publishing and check confirmations on independent schedules.
        let mut publish_ticks = self.runtime.interval(self.config.publish_retry_delay);
        let mut sweep_ticks = self.runtime.interval(self.config.receipt_query_interval);
        let mut commands_open = true;

        loop {
            // Handle caller commands before worker results when both are ready.
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
                _ = publish_ticks.next() => {
                    // Wake the loop so `plan()` can start publication retries that are due.
                }
                _ = sweep_ticks.next() => {
                    self.start_sweep();
                }
                _ = self.runtime.cancelled() => {
                    self.closed.store(true, Ordering::Release);
                    self.ledger.abort();
                    break;
                }
            }

            // Apply the latest result, then schedule the next required work.
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

    /// Applies one caller command to the pending ledger.
    pub fn handle_command(&mut self, command: CoordinatorCommand) {
        match command {
            CoordinatorCommand::Submit(staged) if !self.closing => {
                self.ledger.submit(staged);
            }
            CoordinatorCommand::Submit(staged) => {
                staged.reject(TxManagerError::ChannelClosed);
            }
            CoordinatorCommand::Cancel(result) if !self.closing => {
                self.ledger.cancel(self.sender, result);
            }
            CoordinatorCommand::Cancel(result) => {
                let _ = result.send(Err(TxManagerError::ChannelClosed));
            }
        }
    }

    /// Applies one background worker result to the pending ledger.
    pub fn handle_event(&mut self, event: WorkerEvent) {
        let now = self.runtime.now();

        match event {
            WorkerEvent::TxPrepared { submission_id, result } => {
                self.ledger.tx_prepared(submission_id, result, now);
            }
            WorkerEvent::ReplacementTxPrepared { submission_id, base_version, reason, result } => {
                if result.is_ok() && matches!(reason, ReplacementReason::FeeBump) {
                    self.metrics.record_gas_bump();
                }
                self.ledger.replacement_tx_prepared(
                    submission_id,
                    base_version,
                    reason,
                    result,
                    now,
                );
            }
            WorkerEvent::AccountNonceFetched { submission_id, version, result } => {
                self.ledger.account_nonce_fetched(submission_id, version, result, now);
            }
            WorkerEvent::SweepCompleted(result) => {
                self.sweep_in_progress = false;

                match result {
                    Ok(resolutions) => {
                        for resolution in &resolutions {
                            if matches!(resolution.outcome, SweepOutcome::Confirmed { .. }) {
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
            WorkerEvent::WorkerPanicked(worker) => {
                // The ledger cannot finish work whose worker panicked.
                error!(worker, "transaction manager worker panicked; stopping coordinator");
                self.closing = true;
                self.closed.store(true, Ordering::Release);
                self.ledger.abort();
            }
        }
    }

    /// Starts each action returned by [`PendingLedger::plan`].
    pub fn start_planned_work(&mut self) {
        for work in self.ledger.plan(self.runtime.now()) {
            match work {
                PendingWork::PrepareTx { submission_id, nonce, candidate, .. } => {
                    let builder = self.workers.builder.clone();
                    self.spawn_worker(
                        "prepare tx",
                        async move { builder.prepare_tx(&candidate, nonce).await },
                        move |result| WorkerEvent::TxPrepared { submission_id, result },
                    );
                }
                PendingWork::PrepareReplacementTx {
                    submission_id,
                    base_version,
                    nonce,
                    candidate,
                    base,
                    reason,
                } => {
                    let builder = self.workers.builder.clone();
                    self.spawn_worker(
                        "prepare replacement tx",
                        async move {
                            builder.prepare_replacement_tx(&candidate, &base, nonce, reason).await
                        },
                        move |result| WorkerEvent::ReplacementTxPrepared {
                            submission_id,
                            base_version,
                            reason,
                            result,
                        },
                    );
                }
                PendingWork::FetchAccountNonce { submission_id, version } => {
                    let sweeper = self.workers.sweeper.clone();
                    self.spawn_worker(
                        "fetch account nonce",
                        async move { sweeper.latest_nonce().await },
                        move |result| WorkerEvent::AccountNonceFetched {
                            submission_id,
                            version,
                            result,
                        },
                    );
                }
            }
        }
    }

    /// Starts a chain sweep unless one is already running.
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
            WorkerEvent::SweepCompleted,
        );
    }

    /// Runs a worker and reports either its result or a panic.
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
                    let event = outcome.map_or(WorkerEvent::WorkerPanicked(name), event);
                    let _ = events.send(event);
                }
            }
        });
    }

    /// Stops accepting new work and lets pending transactions finish.
    pub fn begin_close(&mut self) {
        if self.closing {
            return;
        }

        self.closing = true;
        self.closed.store(true, Ordering::Release);
        self.ledger.close();
    }
}
