//! Driver loop for the proposer.
//!
//! The driver coordinates between RPC clients, the enclave, and contract
//! interactions to generate and submit output proposals as dispute games.
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
    /// Number of L2 blocks between intermediate output root checkpoints.
    pub intermediate_block_interval: u64,
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
            intermediate_block_interval: 512,
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
    /// Cached intermediate roots for the current proposal (survives retry after failed submission).
    cached_intermediate_roots: Vec<B256>,
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
            cached_intermediate_roots: Vec::new(),
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
    pub const fn set_parent_game_state(
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
        if let Err(e) = self.generate_outputs(starting_block_number).await {
            warn!(error = %e, "Error generating outputs");
            return Err(e);
        }

        // Extract intermediate roots from the per-block pending queue. Once aggregation
        // drains the queue, we rely on the cached copy for retries.
        let fresh_roots = self.extract_intermediate_roots(starting_block_number);
        if !fresh_roots.is_empty() {
            self.cached_intermediate_roots = fresh_roots;
        }
        let intermediate_roots = self.cached_intermediate_roots.clone();

        // Check if we have enough proofs to aggregate and propose.
        match self
            .next_output(starting_block_number, starting_root, &intermediate_roots)
            .await
        {
            Ok(Some(proposal)) => {
                self.propose_output(&proposal, parent_index, &intermediate_roots)
                    .await;
            }
            Ok(None) => {}
            Err(e) => {
                warn!(error = %e, "Error getting next output");
                return Err(e);
            }
        }

        Ok(())
    }

    /// Extracts intermediate output roots from the pending queue.
    ///
    /// The intermediate roots are the output roots at every `intermediate_block_interval`
    /// within the current proposal range. Must be called before aggregation drains the queue.
    fn extract_intermediate_roots(&self, starting_block_number: u64) -> Vec<B256> {
        let interval = self.config.intermediate_block_interval;
        let count = self.config.block_interval / interval;
        let mut roots = Vec::with_capacity(count as usize);
        for i in 1..=count {
            let target_block = starting_block_number + i * interval;
            if let Some(p) = self.pending.iter().find(|p| p.to.number == target_block) {
                roots.push(p.output.output_root);
            }
        }
        roots
    }

    /// Generates single-block proofs, filling the pending queue.
    async fn generate_outputs(&mut self, starting_block_number: u64) -> Result<(), ProposerError> {
        // Clear pending if not contiguous with starting point.
        if let Some(front) = self.pending.front() {
            if front.from.number.saturating_sub(1) != starting_block_number {
                warn!(
                    starting = starting_block_number,
                    pending_from = front.from.number.saturating_sub(1),
                    "Pending outputs not contiguous with starting point, clearing"
                );
                self.pending.clear();
            }
        }

        // Determine what block to generate next.
        let next_number = self
            .pending
            .back()
            .map_or(starting_block_number, |back| back.to.number);

        // Generate proofs up to the target block for this interval.
        let target_block = starting_block_number + self.config.block_interval;

        for i in 0..self.config.block_interval {
            if self.cancel.is_cancelled() {
                info!("Shutting down proof generation");
                break;
            }

            let number = next_number + 1 + i;

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
        intermediate_roots: &[B256],
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

            // Only include intermediate roots in the final aggregation round.
            let is_final_batch = count == batch_length;
            let batch_roots = if is_final_batch {
                intermediate_roots.to_vec()
            } else {
                vec![]
            };

            match self
                .prover
                .aggregate(starting_root, prev_block_number, batch, batch_roots)
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

        // Reorg detection: verify the proposal's block hash matches the canonical chain.
        match self
            .l2_client
            .header_by_number(Some(proposal.to.number))
            .await
        {
            Ok(canonical_header) => {
                if proposal.to.hash != canonical_header.hash {
                    warn!(
                        proposal_hash = ?proposal.to.hash,
                        canonical_hash = ?canonical_header.hash,
                        block_number = proposal.to.number,
                        "Proposal block hash does not match canonical chain, possible reorg"
                    );
                    self.pending.clear();
                    return Ok(None);
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to fetch canonical header for reorg check");
                return Ok(None);
            }
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
    async fn propose_output(
        &mut self,
        proposal: &ProverProposal,
        parent_index: u32,
        intermediate_roots: &[B256],
    ) {
        info!(
            l2_block_number = proposal.to.number,
            l1_origin_number = proposal.to.l1origin.number,
            l1_origin_hash = ?proposal.to.l1origin.hash,
            output_root = ?proposal.output.output_root,
            has_withdrawals = proposal.has_withdrawals,
            from = proposal.from.number,
            to = proposal.to.number,
            parent_index,
            intermediate_roots_count = intermediate_roots.len(),
            "Proposing output (creating dispute game)"
        );

        match tokio::time::timeout(
            PROPOSAL_TIMEOUT,
            self.output_proposer
                .propose_output(proposal, parent_index, intermediate_roots),
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
                        self.cached_intermediate_roots.clear();

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
                    self.cached_intermediate_roots.clear();
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

        // Create a fresh session token (child of global, so SIGTERM still propagates).
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

        // Cancel the current session (does not cancel the global token).
        self.session_cancel.lock().await.cancel();

        // Await the spawned task so the driver mutex is released cleanly.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use alloy_primitives::{B256, Bytes};
    use async_trait::async_trait;
    use op_enclave_client::{ClientError, ExecuteStatelessRequest};
    use op_enclave_core::Proposal;
    use op_enclave_core::types::config::RollupConfig;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::enclave::EnclaveClientTrait;
    use crate::prover::Prover;
    use crate::prover::types::test_helpers::test_proposal;
    use crate::rpc::SyncStatus;
    use crate::test_utils::{
        MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2, MockOutputProposer,
        MockRollupClient, test_anchor_root, test_per_chain_config, test_sync_status,
    };

    // ---- Mock infrastructure ----

    /// Mock enclave whose methods are never called in most tests.
    struct MockEnclave;

    #[async_trait]
    impl EnclaveClientTrait for MockEnclave {
        async fn execute_stateless(
            &self,
            _: ExecuteStatelessRequest,
        ) -> Result<Proposal, ClientError> {
            unimplemented!()
        }
        async fn aggregate(
            &self,
            _: B256,
            _: B256,
            _: u64,
            _: Vec<Proposal>,
            _: alloy_primitives::Address,
            _: B256,
            _: Vec<B256>,
        ) -> Result<Proposal, ClientError> {
            unimplemented!()
        }
    }

    /// Mock enclave that returns a valid `Proposal` from `aggregate()`.
    /// Used for testing the aggregation code path in `next_output()`.
    struct MockEnclaveForAggregation;

    #[async_trait]
    impl EnclaveClientTrait for MockEnclaveForAggregation {
        async fn execute_stateless(
            &self,
            _: ExecuteStatelessRequest,
        ) -> Result<Proposal, ClientError> {
            unimplemented!()
        }
        async fn aggregate(
            &self,
            _: B256,
            _: B256,
            _: u64,
            proposals: Vec<Proposal>,
            _: alloy_primitives::Address,
            _: B256,
            _: Vec<B256>,
        ) -> Result<Proposal, ClientError> {
            let last = proposals.last().unwrap();
            Ok(Proposal {
                output_root: last.output_root,
                signature: Bytes::from(vec![0xab; 65]),
                l1_origin_hash: last.l1_origin_hash,
                l1_origin_number: last.l1_origin_number,
                l2_block_number: last.l2_block_number,
                prev_output_root: last.prev_output_root,
                config_hash: last.config_hash,
            })
        }
    }

    // ---- Helpers ----

    /// Generic driver constructor that accepts a custom enclave, config, and mocks.
    fn test_driver_custom<E: EnclaveClientTrait + 'static>(
        enclave: E,
        driver_config: DriverConfig,
        l1_block_number: u64,
        sync_status: SyncStatus,
        canonical_hash: Option<B256>,
        output_proposer: Arc<dyn OutputProposer>,
        cancel: CancellationToken,
    ) -> Driver<MockL1, MockL2, E, MockRollupClient, MockAnchorStateRegistry, MockDisputeGameFactory>
    {
        let l1 = Arc::new(MockL1 {
            latest_block_number: l1_block_number,
        });
        let l2 = Arc::new(MockL2 {
            block_not_found: true,
            canonical_hash,
        });
        let prover = Arc::new(Prover::new(
            test_per_chain_config(),
            RollupConfig::default(),
            Arc::clone(&l1),
            Arc::clone(&l2),
            enclave,
            alloy_primitives::Address::ZERO,
            B256::ZERO,
        ));
        let rollup = Arc::new(MockRollupClient { sync_status });
        let anchor_registry = Arc::new(MockAnchorStateRegistry {
            anchor_root: test_anchor_root(0),
        });
        let factory = Arc::new(MockDisputeGameFactory { game_count: 1 });

        Driver::new(
            driver_config,
            prover,
            l1,
            l2,
            rollup,
            anchor_registry,
            factory,
            output_proposer,
            cancel,
        )
    }

    /// Convenience wrapper with sensible defaults.
    fn test_driver(
        l1_block_number: u64,
        sync_status: SyncStatus,
        canonical_hash: Option<B256>,
    ) -> Driver<
        MockL1,
        MockL2,
        MockEnclave,
        MockRollupClient,
        MockAnchorStateRegistry,
        MockDisputeGameFactory,
    > {
        test_driver_custom(
            MockEnclave,
            DriverConfig {
                block_interval: 10,
                ..Default::default()
            },
            l1_block_number,
            sync_status,
            canonical_hash,
            Arc::new(MockOutputProposer),
            CancellationToken::new(),
        )
    }

    // ---- Tests ----

    #[tokio::test]
    async fn test_generate_outputs_clears_on_gap() {
        let sync_status = test_sync_status(200, B256::ZERO);
        let mut driver = test_driver(1000, sync_status, None);

        // Pre-populate with blocks 15..=17 (gap: starting=10 -> front.from=15)
        for n in 15..=17 {
            driver.pending.push_back(test_proposal(n, n, false));
        }
        assert_eq!(driver.pending.len(), 3);

        // generate_outputs(10): front.from(15) - 1 = 14 != 10, so queue is cleared
        let result = driver.generate_outputs(10).await;
        assert!(result.is_ok());
        assert_eq!(driver.pending.len(), 0);
    }

    #[tokio::test]
    async fn test_generate_outputs_preserves_contiguous() {
        let sync_status = test_sync_status(200, B256::ZERO);
        let mut driver = test_driver(1000, sync_status, None);

        // Pre-populate: starting=10, front.from=11 => 11-1=10 == starting, contiguous
        driver.pending.push_back(test_proposal(11, 11, false));
        driver.pending.push_back(test_proposal(12, 12, false));
        assert_eq!(driver.pending.len(), 2);

        let result = driver.generate_outputs(10).await;
        assert!(result.is_ok());
        // Queue preserved (contiguous), no new blocks generated (MockL2 returns BlockNotFound)
        assert_eq!(driver.pending.len(), 2);
    }

    #[tokio::test]
    async fn test_next_output_returns_none_when_empty() {
        let sync_status = test_sync_status(200, B256::ZERO);
        let mut driver = test_driver(1000, sync_status, None);

        // Empty pending queue
        let result = driver.next_output(100, B256::ZERO, &[]).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_next_output_returns_none_when_target_not_reached() {
        // target = 100 + 10 = 110, safe=200, but pending only goes up to 105
        let canonical_hash = B256::repeat_byte(0x30);
        let sync_status = test_sync_status(200, canonical_hash);
        let mut driver = test_driver(1000, sync_status, Some(canonical_hash));

        driver.pending.push_back(test_proposal(101, 105, false));

        let result = driver.next_output(100, B256::ZERO, &[]).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_next_output_returns_none_when_target_not_safe() {
        // target = 100 + 10 = 110, but safe_number = 105 (not safe yet)
        let canonical_hash = B256::repeat_byte(0x30);
        let sync_status = test_sync_status(105, canonical_hash);
        let mut driver = test_driver(1000, sync_status, Some(canonical_hash));

        driver.pending.push_back(test_proposal(101, 110, false));

        let result = driver.next_output(100, B256::ZERO, &[]).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_next_output_reorg_detection() {
        // Proposal has to.hash = 0x30, but canonical hash is 0xBB => mismatch
        let canonical_hash = B256::repeat_byte(0xBB);
        let sync_status = test_sync_status(200, B256::ZERO);
        let mut driver = test_driver(1000, sync_status, Some(canonical_hash));

        // test_proposal sets to.hash = B256::repeat_byte(0x30) which != 0xBB
        driver.pending.push_back(test_proposal(101, 110, false));
        assert_eq!(driver.pending.len(), 1);

        let result = driver.next_output(100, B256::ZERO, &[]).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
        // Queue should be cleared due to reorg detection
        assert_eq!(driver.pending.len(), 0);
    }

    #[tokio::test]
    async fn test_next_output_blockhash_window() {
        // l1_latest=10000, proposal l1origin.number = 1000
        // Threshold = 10000 - (8191-100) = 10000 - 8091 = 1909
        // 1000 <= 1909 → rejected (L1 origin too old)
        let canonical_hash = B256::repeat_byte(0x30);
        let sync_status = test_sync_status(200, canonical_hash);
        let mut driver = test_driver(10000, sync_status, Some(canonical_hash));

        let mut proposal = test_proposal(101, 110, false);
        proposal.to.l1origin.number = 1000;
        driver.pending.push_back(proposal);

        let result = driver.next_output(100, B256::ZERO, &[]).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
        // Queue NOT cleared — blockhash window gating doesn't clear the queue
        assert_eq!(driver.pending.len(), 1);
    }

    #[tokio::test]
    async fn test_next_output_already_proposed() {
        // Proposal for block 110, but we already proposed for that block
        let canonical_hash = B256::repeat_byte(0x30);
        let sync_status = test_sync_status(200, canonical_hash);
        let mut driver = test_driver(1000, sync_status, Some(canonical_hash));

        driver.pending.push_back(test_proposal(101, 110, false));
        driver.last_proposed_block = 110;

        let result = driver.next_output(100, B256::ZERO, &[]).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_next_output_happy_path() {
        // target = 100 + 10 = 110, safe = 200, l1_latest = 1000
        // test_proposal sets l1origin.number = 100 + to_number = 210, well within BLOCKHASH window
        let canonical_hash = B256::repeat_byte(0x30);
        let sync_status = test_sync_status(200, canonical_hash);
        let mut driver = test_driver(1000, sync_status, Some(canonical_hash));

        driver.pending.push_back(test_proposal(101, 110, false));
        assert_eq!(driver.pending.len(), 1);

        let result = driver.next_output(100, B256::ZERO, &[]).await;
        assert!(result.is_ok());
        let proposal = result.unwrap();
        assert!(proposal.is_some(), "expected Some(proposal)");
        let proposal = proposal.unwrap();
        assert_eq!(proposal.from.number, 101);
        assert_eq!(proposal.to.number, 110);
        // Pending still has the item (next_output clones, doesn't pop)
        assert_eq!(driver.pending.len(), 1);
    }

    #[tokio::test]
    async fn test_next_output_with_aggregation() {
        let canonical_hash = B256::repeat_byte(0x30);
        let sync_status = test_sync_status(103, canonical_hash);
        let cancel = CancellationToken::new();

        let mut driver = test_driver_custom(
            MockEnclaveForAggregation,
            DriverConfig {
                block_interval: 3,
                ..Default::default()
            },
            400,
            sync_status,
            Some(canonical_hash),
            Arc::new(MockOutputProposer),
            cancel,
        );

        // Pre-populate 3 single-block proposals: blocks 101, 102, 103
        driver.pending.push_back(test_proposal(101, 101, false));
        driver.pending.push_back(test_proposal(102, 102, false));
        driver.pending.push_back(test_proposal(103, 103, true));
        assert_eq!(driver.pending.len(), 3);

        // target = 100 + 3 = 103, safe = 103, all gates pass
        let result = driver.next_output(100, B256::ZERO, &[]).await;
        assert!(result.is_ok());
        let proposal = result.unwrap();
        assert!(
            proposal.is_some(),
            "expected Some(proposal) after aggregation"
        );
        let proposal = proposal.unwrap();
        assert_eq!(proposal.from.number, 101);
        assert_eq!(proposal.to.number, 103);
        // Pending should have 1 aggregated item
        assert_eq!(driver.pending.len(), 1);
    }

    #[tokio::test]
    async fn test_step_calls_output_proposer() {
        struct TrackingOutputProposer {
            called: AtomicBool,
        }

        #[async_trait]
        impl OutputProposer for TrackingOutputProposer {
            async fn propose_output(
                &self,
                _proposal: &ProverProposal,
                _parent_index: u32,
                _intermediate_roots: &[B256],
            ) -> Result<(), ProposerError> {
                self.called.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        let canonical_hash = B256::repeat_byte(0x30);
        let sync_status = test_sync_status(200, canonical_hash);
        let tracking = Arc::new(TrackingOutputProposer {
            called: AtomicBool::new(false),
        });

        let mut driver = test_driver_custom(
            MockEnclave,
            DriverConfig {
                block_interval: 10,
                ..Default::default()
            },
            1000,
            sync_status,
            Some(canonical_hash),
            Arc::clone(&tracking) as Arc<dyn OutputProposer>,
            CancellationToken::new(),
        );

        // Set parent game state so step() uses it as starting point
        driver.set_parent_game_state(0, B256::ZERO, 100);
        driver.pending.push_back(test_proposal(101, 110, false));

        let result = driver.step().await;
        assert!(result.is_ok(), "step() should succeed, got: {result:?}");
        assert!(
            tracking.called.load(Ordering::SeqCst),
            "OutputProposer::propose_output should have been called"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_run_cancellation() {
        let sync_status = test_sync_status(200, B256::ZERO);
        let cancel = CancellationToken::new();

        let mut driver = test_driver_custom(
            MockEnclave,
            DriverConfig {
                poll_interval: Duration::from_secs(3600),
                block_interval: 10,
                ..Default::default()
            },
            1000,
            sync_status,
            None,
            Arc::new(MockOutputProposer),
            cancel.clone(),
        );

        let handle = tokio::spawn(async move { driver.run().await });

        // Cancel immediately — with start_paused, tokio time is virtual
        cancel.cancel();

        let result = handle.await.expect("task should not panic");
        assert!(result.is_ok(), "run() should return Ok on cancellation");
    }

    // ---- DriverHandle tests ----

    /// Helper that builds a `DriverHandle` using mock components.
    fn test_driver_handle(
        global_cancel: CancellationToken,
    ) -> DriverHandle<
        MockL1,
        MockL2,
        MockEnclave,
        MockRollupClient,
        MockAnchorStateRegistry,
        MockDisputeGameFactory,
    > {
        let sync_status = test_sync_status(200, B256::ZERO);
        let driver = test_driver_custom(
            MockEnclave,
            DriverConfig {
                poll_interval: Duration::from_secs(3600), // long interval; won't tick in tests
                block_interval: 10,
                ..Default::default()
            },
            1000,
            sync_status,
            None,
            Arc::new(MockOutputProposer),
            global_cancel.child_token(),
        );
        DriverHandle::new(driver, global_cancel)
    }

    #[tokio::test]
    async fn test_driver_handle_start_stop() {
        let cancel = CancellationToken::new();
        let handle = test_driver_handle(cancel);

        assert!(!handle.is_running());

        // Start
        let result = handle.start_proposer().await;
        assert!(result.is_ok());
        assert!(handle.is_running());

        // Stop
        let result = handle.stop_proposer().await;
        assert!(result.is_ok());
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_driver_handle_double_start_errors() {
        let cancel = CancellationToken::new();
        let handle = test_driver_handle(cancel);

        handle.start_proposer().await.unwrap();
        assert!(handle.is_running());

        // Second start should fail
        let result = handle.start_proposer().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already running"));

        handle.stop_proposer().await.unwrap();
    }

    #[tokio::test]
    async fn test_driver_handle_stop_when_not_running() {
        let cancel = CancellationToken::new();
        let handle = test_driver_handle(cancel);

        let result = handle.stop_proposer().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not running"));
    }

    #[tokio::test]
    async fn test_driver_handle_restart() {
        let cancel = CancellationToken::new();
        let handle = test_driver_handle(cancel);

        // Start, stop, start again
        handle.start_proposer().await.unwrap();
        assert!(handle.is_running());

        handle.stop_proposer().await.unwrap();
        assert!(!handle.is_running());

        // Should be able to restart
        handle.start_proposer().await.unwrap();
        assert!(handle.is_running());

        handle.stop_proposer().await.unwrap();
        assert!(!handle.is_running());
    }

    #[tokio::test]
    async fn test_driver_handle_global_cancel_stops_driver() {
        let cancel = CancellationToken::new();
        let handle = test_driver_handle(cancel.clone());

        handle.start_proposer().await.unwrap();
        assert!(handle.is_running());

        // Global cancel (simulating SIGTERM) should stop the driver
        cancel.cancel();

        // Give the spawned task a moment to observe the cancellation
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(!handle.is_running(), "driver should stop on global cancel");
    }

    #[tokio::test]
    async fn test_driver_handle_debug() {
        let cancel = CancellationToken::new();
        let handle = test_driver_handle(cancel);

        let debug = format!("{handle:?}");
        assert!(debug.contains("DriverHandle"));
        assert!(debug.contains("running"));
    }
}
