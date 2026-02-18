//! Driver loop for the proposer.
//!
//! The driver coordinates between RPC clients, the enclave, and contract
//! interactions to generate and submit output proposals as dispute games.
//!
//! TODO: Add unit tests for the driver (step, generate_outputs, next_output,
//! propose_output, retry-on-failure, and graceful shutdown).
//!
//! # Lifecycle control
//!
//! The [`Driver`] itself runs a single polling loop via [`Driver::run`].
//! [`DriverHandle`] wraps a `Driver` and exposes start/stop/is-running
//! semantics through the [`ProposerDriverControl`] trait, which is consumed
//! by the admin JSON-RPC server.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use alloy_primitives::{B256, U256};
use async_trait::async_trait;
use eyre::Result;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::contracts::anchor_state_registry::AnchorStateRegistryClient;
use crate::contracts::dispute_game_factory::DisputeGameFactoryClient;
use crate::contracts::output_proposer::{OutputProposer, is_game_already_exists};
use crate::enclave::EnclaveClientTrait;
use crate::metrics as proposer_metrics;
use crate::prover::{Prover, ProverProposal};
use crate::rpc::{L1Client, L2Client, RollupClient};
use crate::{
    AGGREGATE_BATCH_SIZE, BLOCKHASH_SAFETY_MARGIN, BLOCKHASH_WINDOW, NO_PARENT_INDEX,
    PROPOSAL_TIMEOUT, ProposerError,
};

/// Driver configuration.
#[derive(Debug, Clone)]
pub struct DriverConfig {
    /// Polling interval for new blocks.
    pub poll_interval: Duration,
    /// Number of L2 blocks between proposals (read from `AggregateVerifier` at startup).
    pub block_interval: u64,
    /// ETH bond required to create a dispute game.
    pub init_bond: U256,
    /// Game type ID for `AggregateVerifier` dispute games.
    pub game_type: u32,
    /// If true, use `safe_l2` (derived from L1 but L1 not yet finalized).
    /// If false (default), use `finalized_l2` (derived from finalized L1).
    pub allow_non_finalized: bool,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(12),
            block_interval: 512,
            init_bond: U256::ZERO,
            game_type: 0,
            allow_non_finalized: false,
        }
    }
}

/// Tracks the most recent dispute game created by this proposer.
///
/// Used to chain games: each new game references its parent via `game_index`.
/// On restart, this state is recovered from on-chain data.
#[derive(Debug, Clone, Default)]
struct ParentGameState {
    /// Whether we've created at least one game since the process started.
    initialized: bool,
    /// Factory index of the last game we created.
    game_index: u32,
    /// Output root claimed by the last game.
    output_root: B256,
    /// L2 block number of the last game's claim.
    l2_block_number: u64,
}

/// The main driver that coordinates proposal generation.
pub struct Driver<L1, L2, E, R, ASR, F>
where
    L1: L1Client,
    L2: L2Client,
    E: EnclaveClientTrait,
    R: RollupClient,
    ASR: AnchorStateRegistryClient,
    F: DisputeGameFactoryClient,
{
    config: DriverConfig,
    prover: Arc<Prover<L1, L2, E>>,
    l1_client: Arc<L1>,
    l2_client: Arc<L2>,
    rollup_client: Arc<R>,
    anchor_registry: Arc<ASR>,
    factory_client: Arc<F>,
    output_proposer: Arc<dyn OutputProposer>,
    cancel: CancellationToken,
    /// Pending single-block proposals awaiting aggregation and submission.
    pending: VecDeque<ProverProposal>,
    /// Tracks the most recent dispute game for parent chaining.
    parent_game_state: ParentGameState,
    /// Prevents re-proposing the same block.
    last_proposed_block: u64,
}

impl<L1, L2, E, R, ASR, F> std::fmt::Debug for Driver<L1, L2, E, R, ASR, F>
where
    L1: L1Client,
    L2: L2Client,
    E: EnclaveClientTrait,
    R: RollupClient,
    ASR: AnchorStateRegistryClient,
    F: DisputeGameFactoryClient,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Driver")
            .field("config", &self.config)
            .field("pending_count", &self.pending.len())
            .field("parent_game_state", &self.parent_game_state)
            .finish_non_exhaustive()
    }
}

impl<L1, L2, E, R, ASR, F> Driver<L1, L2, E, R, ASR, F>
where
    L1: L1Client + 'static,
    L2: L2Client + 'static,
    E: EnclaveClientTrait + 'static,
    R: RollupClient + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    /// Creates a new driver with the given configuration.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: DriverConfig,
        prover: Arc<Prover<L1, L2, E>>,
        l1_client: Arc<L1>,
        l2_client: Arc<L2>,
        rollup_client: Arc<R>,
        anchor_registry: Arc<ASR>,
        factory_client: Arc<F>,
        output_proposer: Arc<dyn OutputProposer>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config,
            prover,
            l1_client,
            l2_client,
            rollup_client,
            anchor_registry,
            factory_client,
            output_proposer,
            cancel,
            pending: VecDeque::new(),
            parent_game_state: ParentGameState::default(),
            last_proposed_block: 0,
        }
    }

    /// Replaces the cancellation token.
    ///
    /// Used by [`DriverHandle`] to create fresh sessions when the driver is
    /// restarted via the admin RPC.
    pub(crate) fn set_cancel(&mut self, cancel: CancellationToken) {
        self.cancel = cancel;
    }

    /// Sets the parent game state directly (typically from recovery in `main.rs`).
    pub fn set_parent_game_state(
        &mut self,
        game_index: u32,
        output_root: B256,
        l2_block_number: u64,
    ) {
        self.parent_game_state = ParentGameState {
            initialized: true,
            game_index,
            output_root,
            l2_block_number,
        };
    }

    /// Starts the driver loop.
    pub async fn run(&mut self) -> Result<()> {
        info!("Starting driver loop");

        loop {
            tokio::select! {
                () = self.cancel.cancelled() => {
                    info!("Driver received shutdown signal");
                    break;
                }
                () = sleep(self.config.poll_interval) => {
                    if let Err(e) = self.step().await {
                        warn!(error = %e, "Driver step failed");
                    }
                }
            }
        }

        info!("Driver loop stopped");
        Ok(())
    }

    /// Performs a single driver step (one tick of the loop).
    async fn step(&mut self) -> Result<(), ProposerError> {
        // Determine starting point: parent game state or anchor registry.
        let (starting_block_number, starting_root, parent_index) =
            if self.parent_game_state.initialized {
                (
                    self.parent_game_state.l2_block_number,
                    self.parent_game_state.output_root,
                    self.parent_game_state.game_index,
                )
            } else {
                let anchor = self.anchor_registry.get_anchor_root().await?;
                debug!(
                    l2_block_number = anchor.l2_block_number,
                    root = ?anchor.root,
                    "Using anchor state registry as starting point"
                );
                (anchor.l2_block_number, anchor.root, NO_PARENT_INDEX)
            };

        // Generate proofs for blocks in the range.
        if let Err(e) = self
            .generate_outputs(starting_block_number, starting_root)
            .await
        {
            warn!(error = %e, "Error generating outputs");
            return Err(e);
        }

        // Check if we have enough proofs to aggregate and propose.
        match self.next_output(starting_block_number, starting_root).await {
            Ok(Some(proposal)) => {
                self.propose_output(&proposal, parent_index).await;
            }
            Ok(None) => {}
            Err(e) => {
                warn!(error = %e, "Error getting next output");
                return Err(e);
            }
        }

        Ok(())
    }

    /// Generates single-block proofs, filling the pending queue.
    async fn generate_outputs(
        &mut self,
        starting_block_number: u64,
        _starting_root: B256,
    ) -> Result<(), ProposerError> {
        // Clear pending if not contiguous with starting point.
        if let Some(front) = self.pending.front() {
            if front.from.number.saturating_sub(1) != starting_block_number
                && !self.pending.is_empty()
            {
                warn!(
                    starting = starting_block_number,
                    pending_from = front.from.number.saturating_sub(1),
                    "Pending outputs not contiguous with starting point, clearing"
                );
                self.pending.clear();
            }
        }

        // Determine what block to generate next.
        let next_number = if let Some(back) = self.pending.back() {
            back.to.number
        } else {
            starting_block_number
        };

        // Generate proofs up to the target block for this interval.
        let target_block = starting_block_number + self.config.block_interval;

        for i in 0..self.config.block_interval {
            if self.cancel.is_cancelled() {
                info!("Shutting down proof generation");
                break;
            }

            let number = next_number + 1 + i as u64;

            // Stop once we've reached the target block for this interval.
            if number > target_block {
                break;
            }

            let block = match self.l2_client.block_by_number(Some(number)).await {
                Ok(block) => block,
                Err(crate::rpc::RpcError::BlockNotFound(_)) => {
                    break;
                }
                Err(e) => {
                    return Err(ProposerError::Rpc(e));
                }
            };

            let proposal = self.prover.generate(&block).await?;

            let blocks_behind = match self.latest_safe_block_number().await {
                Ok(safe_number) if safe_number > proposal.to.number => {
                    safe_number - proposal.to.number
                }
                _ => 0,
            };

            info!(
                block_number = proposal.to.number,
                l1_origin_number = proposal.to.l1origin.number,
                l1_origin_hash = ?proposal.to.l1origin.hash,
                has_withdrawals = proposal.has_withdrawals,
                output_root = ?proposal.output.output_root,
                blocks_behind,
                "Generated proof for block"
            );
            self.pending.push_back(proposal);
        }

        debug!(queue_depth = self.pending.len(), "Proof queue depth");
        metrics::gauge!(proposer_metrics::PROOF_QUEUE_DEPTH).set(self.pending.len() as f64);
        Ok(())
    }

    /// Determines the next output to propose, aggregating if needed.
    async fn next_output(
        &mut self,
        starting_block_number: u64,
        starting_root: B256,
    ) -> Result<Option<ProverProposal>, ProposerError> {
        let latest_safe = self.latest_safe_block().await?;
        let latest_safe_number = latest_safe.number;

        // We need exactly block_interval proofs to propose.
        let target = starting_block_number + self.config.block_interval;

        // Count pending proposals up to the target block and safe head.
        let mut count = 0;
        for p in &self.pending {
            if p.to.number <= target && p.to.number <= latest_safe_number {
                count += 1;
            } else {
                break;
            }
        }

        if count == 0 {
            return Ok(None);
        }

        // Check that the last eligible proof actually reaches the target block.
        // An aggregated proof may count as 1 but already covers the full interval.
        let last_proof_to = self.pending[count - 1].to.number;
        if last_proof_to < target {
            return Ok(None);
        }

        // Target must be safe before we submit.
        if target > latest_safe_number {
            return Ok(None);
        }

        // Aggregate proposals in batches.
        let prev_block_number = starting_block_number;
        while count > 1 {
            let batch_length = count.min(AGGREGATE_BATCH_SIZE);
            let batch: Vec<ProverProposal> = self.pending.drain(..batch_length).collect();

            match self
                .prover
                .aggregate(starting_root, prev_block_number, batch)
                .await
            {
                Ok(aggregated) => {
                    info!(
                        output_root = ?aggregated.output.output_root,
                        blocks = batch_length,
                        remaining = count - batch_length,
                        has_withdrawals = aggregated.has_withdrawals,
                        from = aggregated.from.number,
                        to = aggregated.to.number,
                        "Aggregated proofs"
                    );
                    self.pending.push_front(aggregated);
                    count -= batch_length - 1;
                }
                Err(e) => {
                    if matches!(e, ProposerError::Enclave(_)) {
                        warn!(error = %e, "Non-recoverable error aggregating proofs");
                        self.pending.clear();
                    }
                    return Err(e);
                }
            }
        }

        let proposal = &self.pending[0];

        // Verify the aggregated proposal reaches the target block.
        if proposal.to.number < target {
            return Ok(None);
        }

        // Don't re-propose for a block we've already submitted.
        if proposal.to.number <= self.last_proposed_block {
            return Ok(None);
        }

        // Check BLOCKHASH window: L1 origin must be recent enough.
        let l1_latest = match self.l1_client.block_number().await {
            Ok(n) => n,
            Err(e) => {
                warn!(error = %e, "Failed to get latest L1 block number");
                return Ok(None);
            }
        };

        let l1_origin_number = proposal.to.l1origin.number;
        if l1_origin_number <= l1_latest.saturating_sub(BLOCKHASH_WINDOW - BLOCKHASH_SAFETY_MARGIN)
        {
            warn!(
                l1_origin = l1_origin_number,
                l1_latest, "Not submitting proposal, L1 origin block is too old"
            );
            return Ok(None);
        }

        Ok(Some(self.pending[0].clone()))
    }

    /// Returns the latest safe L2 block reference.
    async fn latest_safe_block(&self) -> Result<crate::rpc::L2BlockRef, ProposerError> {
        let sync_status = self.rollup_client.sync_status().await?;
        if self.config.allow_non_finalized {
            Ok(sync_status.safe_l2)
        } else {
            Ok(sync_status.finalized_l2)
        }
    }

    /// Returns the latest safe L2 block number.
    async fn latest_safe_block_number(&self) -> Result<u64, ProposerError> {
        self.latest_safe_block().await.map(|b| b.number)
    }

    /// Submits a proposal by creating a dispute game via the factory.
    async fn propose_output(&mut self, proposal: &ProverProposal, parent_index: u32) {
        info!(
            l2_block_number = proposal.to.number,
            l1_origin_number = proposal.to.l1origin.number,
            l1_origin_hash = ?proposal.to.l1origin.hash,
            output_root = ?proposal.output.output_root,
            has_withdrawals = proposal.has_withdrawals,
            from = proposal.from.number,
            to = proposal.to.number,
            parent_index,
            "Proposing output (creating dispute game)"
        );

        match tokio::time::timeout(
            PROPOSAL_TIMEOUT,
            self.output_proposer.propose_output(proposal, parent_index),
        )
        .await
        {
            Ok(Ok(())) => {
                info!(
                    l2_block_number = proposal.to.number,
                    "Dispute game created successfully"
                );
                metrics::counter!(proposer_metrics::L2_OUTPUT_PROPOSALS_TOTAL).increment(1);

                // TODO: This uses `game_count - 1` to infer the new game's index,
                // which is racy if another proposer creates a game between our
                // `create()` and this `game_count()` call. Consider parsing the
                // game index from the transaction receipt's event logs instead.
                // The current approach self-heals via `GameAlreadyExists` recovery.
                match self.factory_client.game_count().await {
                    Ok(count) if count > 0 => {
                        let new_index = (count - 1) as u32;
                        self.parent_game_state = ParentGameState {
                            initialized: true,
                            game_index: new_index,
                            output_root: proposal.output.output_root,
                            l2_block_number: proposal.to.number,
                        };
                        self.last_proposed_block = proposal.to.number;
                        self.pending.clear();

                        info!(
                            new_game_index = new_index,
                            output_root = ?proposal.output.output_root,
                            l2_block_number = proposal.to.number,
                            "Updated parent game state"
                        );
                    }
                    Ok(_) => {
                        warn!("gameCount returned 0 after successful create");
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            "Failed to get new game index, will recover on next tick"
                        );
                    }
                }
            }
            Ok(Err(e)) => {
                if is_game_already_exists(&e) {
                    info!(
                        l2_block_number = proposal.to.number,
                        "Game already exists, continuing"
                    );
                    // The game was created (likely by a previous run). Try to
                    // recover the parent state so we continue the chain.
                    self.pending.clear();
                    self.parent_game_state.initialized = false;
                } else {
                    warn!(
                        error = %e,
                        l2_block_number = proposal.to.number,
                        "Failed to create dispute game"
                    );
                }
            }
            Err(_) => {
                warn!(
                    l2_block_number = proposal.to.number,
                    timeout_secs = PROPOSAL_TIMEOUT.as_secs(),
                    "Dispute game creation timed out"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ProposerDriverControl trait and DriverHandle
// ---------------------------------------------------------------------------

/// Trait for controlling the proposer driver at runtime.
///
/// This is the type-erased interface consumed by the admin JSON-RPC server.
/// [`DriverHandle`] is the concrete implementation.
#[async_trait]
pub trait ProposerDriverControl: Send + Sync {
    /// Start the proposer driver loop.
    async fn start_proposer(&self) -> Result<(), String>;
    /// Stop the proposer driver loop.
    async fn stop_proposer(&self) -> Result<(), String>;
    /// Returns whether the proposer driver is currently running.
    fn is_running(&self) -> bool;
}

/// Manages the lifecycle of a [`Driver`], allowing it to be started and
/// stopped at runtime (e.g. via the admin RPC).
///
/// Internally the driver is placed behind a [`TokioMutex`] so it can be moved
/// into a spawned task for the duration of a session.
pub struct DriverHandle<L1, L2, E, R, ASR, F>
where
    L1: L1Client + 'static,
    L2: L2Client + 'static,
    E: EnclaveClientTrait + 'static,
    R: RollupClient + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    #[allow(clippy::type_complexity)]
    driver: Arc<TokioMutex<Driver<L1, L2, E, R, ASR, F>>>,
    /// Cancel token for the *current* driver session (child of global).
    session_cancel: TokioMutex<CancellationToken>,
    /// Top-level cancel token (SIGTERM / SIGINT).
    global_cancel: CancellationToken,
    /// Join handle for the currently running driver task.
    task: TokioMutex<Option<JoinHandle<Result<()>>>>,
    /// Shared flag indicating whether the driver loop is active.
    running: Arc<AtomicBool>,
}

impl<L1, L2, E, R, ASR, F> std::fmt::Debug for DriverHandle<L1, L2, E, R, ASR, F>
where
    L1: L1Client + 'static,
    L2: L2Client + 'static,
    E: EnclaveClientTrait + 'static,
    R: RollupClient + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverHandle")
            .field("running", &self.running.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl<L1, L2, E, R, ASR, F> DriverHandle<L1, L2, E, R, ASR, F>
where
    L1: L1Client + 'static,
    L2: L2Client + 'static,
    E: EnclaveClientTrait + 'static,
    R: RollupClient + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    /// Wraps a [`Driver`] in a lifecycle-managed handle.
    ///
    /// The driver is **not** started automatically — call
    /// [`start_proposer`](ProposerDriverControl::start_proposer) to begin the
    /// polling loop.
    pub fn new(driver: Driver<L1, L2, E, R, ASR, F>, global_cancel: CancellationToken) -> Self {
        let session_cancel = global_cancel.child_token();
        Self {
            driver: Arc::new(TokioMutex::new(driver)),
            session_cancel: TokioMutex::new(session_cancel),
            global_cancel,
            task: TokioMutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl<L1, L2, E, R, ASR, F> ProposerDriverControl for DriverHandle<L1, L2, E, R, ASR, F>
where
    L1: L1Client + 'static,
    L2: L2Client + 'static,
    E: EnclaveClientTrait + 'static,
    R: RollupClient + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    async fn start_proposer(&self) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("proposer is already running".into());
        }

        let cancel = self.global_cancel.child_token();
        {
            let mut driver = self.driver.lock().await;
            driver.set_cancel(cancel.clone());
        }
        *self.session_cancel.lock().await = cancel;

        let driver = Arc::clone(&self.driver);
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::SeqCst);

        let handle = tokio::spawn(async move {
            let mut guard = driver.lock().await;
            let result = guard.run().await;
            running.store(false, Ordering::SeqCst);
            result
        });

        *self.task.lock().await = Some(handle);
        info!("Proposer driver started");
        Ok(())
    }

    async fn stop_proposer(&self) -> Result<(), String> {
        if !self.running.load(Ordering::SeqCst) {
            return Err("proposer is not running".into());
        }

        self.session_cancel.lock().await.cancel();

        if let Some(task) = self.task.lock().await.take() {
            let _ = task.await;
        }

        info!("Proposer driver stopped");
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}
