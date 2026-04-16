//! Parallel proving pipeline for the proposer.
//!
//! The [`ProvingPipeline`] is an event-driven coordinator that runs multiple
//! proofs concurrently while maintaining strictly sequential on-chain submission.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────┐     ┌──────────────┐     ┌──────────────┐
//! │  PLAN    │ ──▶ │  PROVE       │ ──▶ │  SUBMIT      │
//! │ (scan)   │     │ (parallel)   │     │ (at most 1)  │
//! └──────────┘     └──────────────┘     └──────────────┘
//! ```
//!
//! The coordinator loop uses `tokio::select!` over three event sources:
//!
//! - **Submit completion** — when the spawned L1 transaction resolves, the
//!   coordinator processes the outcome and (on success only) chains the next
//!   submission immediately.
//! - **Proof completion** — when any proof task finishes, its result is stored
//!   in `proved` and the coordinator attempts to start a submission if one is
//!   ready and no submission is in flight.
//! - **Poll-interval tick** — periodic recovery scan that discovers new safe
//!   head advances, refills proof slots, and retries failed submissions.
//!
//! Submission runs as a separate spawned task so the coordinator never blocks
//! on L1 transaction confirmation. Failed submissions defer retry to the next
//! tick rather than retrying immediately, preventing tight loops when L1 is
//! persistently failing.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

use alloy_primitives::{Address, B256, Signature, keccak256};
use alloy_sol_types::SolCall;
use base_proof_contracts::{
    AggregateVerifierClient, AnchorStateRegistryClient, DisputeGameFactoryClient,
    ITEEProverRegistry, encode_extra_data,
};
use base_proof_primitives::{ProofJournal, ProofRequest, ProofResult, ProverClient};
use base_proof_rpc::{L1Provider, L2Provider, RollupProvider, RpcError};
use eyre::Result;
use futures::{StreamExt, TryStreamExt, stream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};

use crate::{
    Metrics,
    constants::PROPOSAL_TIMEOUT,
    driver::{DriverConfig, RecoveredState},
    error::ProposerError,
    output_proposer::OutputProposer,
};

/// Configuration for the parallel proving pipeline.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Maximum number of concurrent proof tasks.
    pub max_parallel_proofs: usize,
    /// Maximum retries for a single proof range before full pipeline reset.
    pub max_retries: u32,
    /// Maximum number of concurrent RPC calls during the recovery scan.
    pub recovery_scan_concurrency: usize,
    /// Base driver configuration.
    pub driver: DriverConfig,
    /// Optional address of the `TEEProverRegistry` contract on L1.
    /// When set, the pipeline validates signers via `isValidSigner` before submission.
    pub tee_prover_registry_address: Option<Address>,
}

/// Cached result from the last successful recovery.
///
/// The cache is keyed by `game_count`. When `game_count` is unchanged
/// and the anchor has not advanced past the cached tip, the cached
/// `RecoveredState` is returned immediately (zero additional RPCs).
///
/// When `game_count` increases (and the anchor is still at or behind the
/// cached tip), the walk resumes from the cached tip (incremental —
/// typically 1–2 steps).
///
/// A full re-walk from the anchor is only needed when:
/// - No cache exists (cold start / pipeline reset).
/// - The anchor advanced past the cached tip (governance intervention).
/// - `game_count` decreased (L1 reorg removed games).
#[derive(Debug, Clone, Copy)]
struct CachedRecovery {
    /// Factory `game_count` at the time of the last walk.
    game_count: u64,
    /// The recovered on-chain state from the walk.
    state: RecoveredState,
}

/// Mutable state for the coordinator loop.
struct PipelineState {
    /// Running proof tasks, each yielding `(target_block, result)`.
    prove_tasks: JoinSet<(u64, Result<ProofResult, ProposerError>)>,
    /// At most one concurrent submission task.
    submit_tasks: JoinSet<SubmitOutcome>,
    /// Completed proofs waiting for sequential submission, keyed by target block.
    proved: BTreeMap<u64, ProofResult>,
    /// Target blocks currently being proved.
    inflight: BTreeSet<u64>,
    /// Target block currently being submitted (at most one).
    submitting: Option<u64>,
    /// Per-target-block retry counts; exceeding `max_retries` triggers a full reset.
    retry_counts: BTreeMap<u64, u32>,
    /// Cached result from the last successful recovery scan.
    cached_recovery: Option<CachedRecovery>,
}

impl PipelineState {
    fn new() -> Self {
        Self {
            prove_tasks: JoinSet::new(),
            submit_tasks: JoinSet::new(),
            proved: BTreeMap::new(),
            inflight: BTreeSet::new(),
            submitting: None,
            retry_counts: BTreeMap::new(),
            cached_recovery: None,
        }
    }

    fn reset(&mut self) {
        self.prove_tasks.abort_all();
        self.submit_tasks.abort_all();
        self.inflight.clear();
        self.proved.clear();
        self.submitting = None;
        self.retry_counts.clear();
        self.cached_recovery = None;
        self.record_gauges();
    }

    fn record_gauges(&self) {
        Metrics::inflight_proofs().set(self.inflight.len() as f64);
        Metrics::proved_queue_depth().set(self.proved.len() as f64);
        Metrics::pipeline_retries().set(self.retry_counts.values().sum::<u32>() as f64);
    }

    fn prune_stale(&mut self, recovered_block: u64) {
        self.proved.retain(|&target, _| target > recovered_block);
        self.inflight.retain(|&target| target > recovered_block);
        self.retry_counts.retain(|&target, _| target > recovered_block);
        // NOTE: we intentionally do NOT abort in-flight submit tasks here.
        // When the recovered block advances past the submitting block, it
        // means the transaction already landed on L1.  Aborting the task
        // would prevent `handle_submit_result` from recording the
        // `last_proposed_block` metric and performing proper state cleanup.
        // The task will finish with `Success` or `GameAlreadyExists`, and
        // `handle_submit_result` will clear `submitting` and update metrics.
    }
}

/// The parallel proving pipeline.
///
/// Orchestrates multiple concurrent proof tasks with a single-threaded
/// coordinator loop.
pub struct ProvingPipeline<L1, L2, R, ASR, F>
where
    L1: L1Provider,
    L2: L2Provider,
    R: RollupProvider,
    ASR: AnchorStateRegistryClient,
    F: DisputeGameFactoryClient,
{
    config: PipelineConfig,
    prover: Arc<dyn ProverClient>,
    l1_client: Arc<L1>,
    l2_client: Arc<L2>,
    rollup_client: Arc<R>,
    anchor_registry: Arc<ASR>,
    factory_client: Arc<F>,
    verifier_client: Arc<dyn AggregateVerifierClient>,
    output_proposer: Arc<dyn OutputProposer>,
    cancel: CancellationToken,
}

impl<L1, L2, R, ASR, F> Clone for ProvingPipeline<L1, L2, R, ASR, F>
where
    L1: L1Provider,
    L2: L2Provider,
    R: RollupProvider,
    ASR: AnchorStateRegistryClient,
    F: DisputeGameFactoryClient,
{
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            prover: Arc::clone(&self.prover),
            l1_client: Arc::clone(&self.l1_client),
            l2_client: Arc::clone(&self.l2_client),
            rollup_client: Arc::clone(&self.rollup_client),
            anchor_registry: Arc::clone(&self.anchor_registry),
            factory_client: Arc::clone(&self.factory_client),
            verifier_client: Arc::clone(&self.verifier_client),
            output_proposer: Arc::clone(&self.output_proposer),
            cancel: self.cancel.clone(),
        }
    }
}

impl<L1, L2, R, ASR, F> std::fmt::Debug for ProvingPipeline<L1, L2, R, ASR, F>
where
    L1: L1Provider,
    L2: L2Provider,
    R: RollupProvider,
    ASR: AnchorStateRegistryClient,
    F: DisputeGameFactoryClient,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProvingPipeline").field("config", &self.config).finish_non_exhaustive()
    }
}

impl<L1, L2, R, ASR, F> ProvingPipeline<L1, L2, R, ASR, F>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
{
    /// Creates a new parallel proving pipeline.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: PipelineConfig,
        prover: Arc<dyn ProverClient>,
        l1_client: Arc<L1>,
        l2_client: Arc<L2>,
        rollup_client: Arc<R>,
        anchor_registry: Arc<ASR>,
        factory_client: Arc<F>,
        verifier_client: Arc<dyn AggregateVerifierClient>,
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
            verifier_client,
            output_proposer,
            cancel,
        }
    }

    /// Replaces the cancellation token.
    ///
    /// Used by [`crate::PipelineHandle`] to create fresh sessions when the
    /// pipeline is restarted via the admin RPC.
    pub fn set_cancel(&mut self, cancel: CancellationToken) {
        self.cancel = cancel;
    }

    /// Runs the parallel proving pipeline until cancelled.
    ///
    /// The coordinator never blocks on L1 transaction confirmation. Submission
    /// runs as a separate spawned task while the coordinator continues to
    /// collect proof completions and refill proof slots immediately.
    pub async fn run(&self) -> Result<()> {
        info!(
            max_parallel_proofs = self.config.max_parallel_proofs,
            block_interval = self.config.driver.block_interval,
            "Starting parallel proving pipeline"
        );

        let mut state = PipelineState::new();
        let mut poll_interval = tokio::time::interval(self.config.driver.poll_interval);

        loop {
            tokio::select! {
                biased;

                () = self.cancel.cancelled() => {
                    state.prove_tasks.abort_all();
                    state.submit_tasks.abort_all();
                    break;
                }

                Some(result) = state.submit_tasks.join_next() => {
                    let chain_next = self.handle_submit_result(result, &mut state).await;
                    if chain_next {
                        self.try_submit(&mut state);
                    }
                }

                Some(result) = state.prove_tasks.join_next() => {
                    self.handle_proof_result(result, &mut state);
                    self.try_submit(&mut state);
                }

                _ = poll_interval.tick() => {
                    if let Err(e) = self.tick(&mut state).await {
                        error!(error = ?e, "Pipeline tick failed, retrying next interval");
                    }
                    self.try_submit(&mut state);
                }
            }
        }

        info!("Parallel proving pipeline stopped");
        Ok(())
    }

    #[instrument(skip_all)]
    async fn tick(&self, state: &mut PipelineState) -> Result<()> {
        let _tick_timer = base_metrics::timed!(Metrics::tick_duration_seconds());

        if let Some((recovered, safe_head)) =
            self.try_recover_and_plan(&mut state.cached_recovery).await
        {
            Metrics::safe_head().set(safe_head as f64);
            state.prune_stale(recovered.l2_block_number);
            self.dispatch_proofs(&recovered, safe_head, state).await?;
        }
        Ok(())
    }

    #[instrument(skip_all, fields(
        recovered_block = recovered.l2_block_number,
        safe_head = safe_head,
    ))]
    async fn dispatch_proofs(
        &self,
        recovered: &RecoveredState,
        safe_head: u64,
        state: &mut PipelineState,
    ) -> Result<()> {
        let mut cursor = recovered
            .l2_block_number
            .checked_add(self.config.driver.block_interval)
            .ok_or_else(|| {
            eyre::eyre!(
                "overflow: l2_block_number {} + block_interval {}",
                recovered.l2_block_number,
                self.config.driver.block_interval
            )
        })?;

        let mut start_block = recovered.l2_block_number;
        let mut start_output = recovered.output_root;

        while cursor <= safe_head && state.inflight.len() < self.config.max_parallel_proofs {
            // Skip blocks already being handled (in-flight, proved, or
            // submitting).  Track the last skipped block so we can fetch
            // its output root once — only when we actually find a block
            // to dispatch.
            let mut last_skipped = None;
            while cursor <= safe_head
                && (state.inflight.contains(&cursor)
                    || state.proved.contains_key(&cursor)
                    || state.submitting == Some(cursor))
            {
                last_skipped = Some(cursor);
                cursor = match cursor.checked_add(self.config.driver.block_interval) {
                    Some(c) => c,
                    // Overflow means there are no further blocks to dispatch.
                    None => return Ok(()),
                };
            }

            // Nothing left to dispatch after skipping.
            if cursor > safe_head {
                break;
            }

            // Still at max capacity after skipping.
            if state.inflight.len() >= self.config.max_parallel_proofs {
                break;
            }

            // Fetch the output root for the last skipped block so the
            // proof request chains correctly.
            if let Some(skipped) = last_skipped {
                match self.rollup_client.output_at_block(skipped).await {
                    Ok(output) => {
                        start_block = skipped;
                        start_output = output.output_root;
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            block = skipped,
                            "Failed to fetch output root while skipping, stopping dispatch"
                        );
                        break;
                    }
                }
            }

            match self.build_proof_request_for(start_block, start_output, cursor).await {
                Ok(request) => {
                    let claimed_output = request.claimed_l2_output_root;
                    let prover = Arc::clone(&self.prover);
                    let target = cursor;
                    let cancel = self.cancel.child_token();

                    info!(
                        from_block = start_block,
                        to_block = target,
                        blocks = target.saturating_sub(start_block),
                        "Dispatching proof task"
                    );
                    state.inflight.insert(target);
                    state.prove_tasks.spawn(async move {
                        let mut proof_timer =
                            base_metrics::timed!(Metrics::proof_duration_seconds());
                        tokio::select! {
                            () = cancel.cancelled() => {
                                proof_timer.disarm();
                                (target, Err(ProposerError::Internal("cancelled".into())))
                            }
                            result = prover.prove(request) => {
                                drop(proof_timer);
                                (target, result.map_err(|e| ProposerError::Prover(e.to_string())))
                            }
                        }
                    });

                    start_block = cursor;
                    start_output = claimed_output;
                }
                Err(e) => {
                    warn!(error = %e, target_block = cursor, "Failed to build proof request");
                    break;
                }
            }
            cursor = match cursor.checked_add(self.config.driver.block_interval) {
                Some(c) => c,
                None => break,
            };
        }
        state.record_gauges();
        Ok(())
    }

    fn try_submit(&self, state: &mut PipelineState) {
        if state.submitting.is_some() || !state.submit_tasks.is_empty() {
            return;
        }

        let recovered = match &state.cached_recovery {
            Some(cached) => cached.state,
            _ => return,
        };

        let next_to_submit =
            match recovered.l2_block_number.checked_add(self.config.driver.block_interval) {
                Some(n) => n,
                None => return,
            };

        let proof_result = match state.proved.remove(&next_to_submit) {
            Some(r) => r,
            None => return,
        };

        let parent_address = recovered.parent_address;
        state.submitting = Some(next_to_submit);
        state.record_gauges();

        info!(target_block = next_to_submit, parent_address = %parent_address, "Spawning submission task");

        let pipeline = self.clone();
        state.submit_tasks.spawn(async move {
            let mut submit_timer = base_metrics::timed!(Metrics::proposal_total_duration_seconds());
            let result =
                pipeline.validate_and_submit(&proof_result, next_to_submit, parent_address).await;
            match result {
                Ok(()) => {
                    drop(submit_timer);
                    SubmitOutcome::Success { target_block: next_to_submit }
                }
                Err(SubmitAction::RootMismatch) => {
                    submit_timer.disarm();
                    SubmitOutcome::RootMismatch { target_block: next_to_submit }
                }
                Err(SubmitAction::Failed(e)) => {
                    submit_timer.disarm();
                    SubmitOutcome::Failed {
                        target_block: next_to_submit,
                        proof: proof_result,
                        error: e,
                    }
                }
                Err(SubmitAction::GameAlreadyExists) => {
                    submit_timer.disarm();
                    SubmitOutcome::GameAlreadyExists { target_block: next_to_submit }
                }
                Err(SubmitAction::Discard(e)) => {
                    submit_timer.disarm();
                    SubmitOutcome::Discard { target_block: next_to_submit, error: e }
                }
            }
        });
    }

    /// Returns `true` when the caller should immediately attempt the next
    /// submission (i.e. on success). Returns `false` on failure/discard so
    /// that retry is deferred to the next poll-interval tick.
    async fn handle_submit_result(
        &self,
        join_result: Result<SubmitOutcome, tokio::task::JoinError>,
        state: &mut PipelineState,
    ) -> bool {
        let outcome = match join_result {
            Ok(outcome) => outcome,
            Err(join_err) if join_err.is_cancelled() => {
                debug!(error = %join_err, "Submit task cancelled");
                state.submitting = None;
                return false;
            }
            Err(join_err) => {
                warn!(error = %join_err, "Submit task panicked");
                state.reset();
                return false;
            }
        };

        match outcome {
            SubmitOutcome::Success { target_block } => {
                info!(target_block, "Submission successful");
                Metrics::last_proposed_block().set(target_block as f64);
                state.retry_counts.remove(&target_block);
                state.submitting = None;
                // Don't clear the cache — recover_latest_state will see the
                // new game_count and incrementally scan just the new entry.
                match self.recover_latest_state(&mut state.cached_recovery).await {
                    Ok(recovered) => {
                        state.prune_stale(recovered.l2_block_number);
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to recover state after submission");
                    }
                }
                state.record_gauges();
                true
            }
            SubmitOutcome::GameAlreadyExists { target_block } => {
                info!(target_block, "Game already exists on chain");
                Metrics::last_proposed_block().set(target_block as f64);
                state.retry_counts.remove(&target_block);
                state.submitting = None;
                // The game exists but the forward walk missed it — most
                // likely because `game_count` was read from a different L1
                // RPC replica than the one serving `factory.games()`.
                // Decrement the cached game_count so the next recovery sees
                // `actual_count > cached_count` and performs an incremental
                // forward walk from the cached tip (O(1): a single
                // `factory.games()` lookup at the next expected block).
                if let Some(ref mut cached) = state.cached_recovery {
                    cached.game_count = cached.game_count.saturating_sub(1);
                }
                match self.recover_latest_state(&mut state.cached_recovery).await {
                    Ok(recovered) => {
                        state.prune_stale(recovered.l2_block_number);
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to recover state after GameAlreadyExists");
                    }
                }
                state.record_gauges();
                true
            }
            SubmitOutcome::RootMismatch { target_block } => {
                warn!(target_block, "Output root mismatch at submit time, resetting pipeline");
                Metrics::root_mismatch_total().increment(1);
                state.reset();
                false
            }
            SubmitOutcome::Failed { target_block, proof, error } => {
                Metrics::errors_total(error.metric_label()).increment(1);
                warn!(
                    error = %error,
                    target_block,
                    "Submission failed, will retry"
                );
                state.proved.insert(target_block, proof);
                state.submitting = None;
                state.record_gauges();
                false
            }
            SubmitOutcome::Discard { target_block, error } => {
                Metrics::errors_total(error.metric_label()).increment(1);
                warn!(
                    error = %error,
                    target_block,
                    "Proof discarded, will re-prove"
                );
                state.submitting = None;
                state.record_gauges();
                false
            }
        }
    }

    fn handle_proof_result(
        &self,
        join_result: Result<(u64, Result<ProofResult, ProposerError>), tokio::task::JoinError>,
        state: &mut PipelineState,
    ) {
        match join_result {
            Ok((target, Ok(proof_result))) => {
                state.inflight.remove(&target);
                state.retry_counts.remove(&target);
                state.proved.insert(target, proof_result);
                state.record_gauges();
                info!(target_block = target, "Proof completed successfully");
            }
            Ok((target, Err(e))) => {
                Metrics::errors_total(e.metric_label()).increment(1);
                state.inflight.remove(&target);
                let count = state.retry_counts.entry(target).or_insert(0);
                *count += 1;
                if *count >= self.config.max_retries {
                    error!(
                        target_block = target,
                        attempts = *count,
                        error = %e,
                        "Proof failed after max retries, resetting pipeline"
                    );
                    state.reset();
                } else {
                    warn!(
                        target_block = target,
                        attempt = *count,
                        error = %e,
                        "Proof failed, will retry next tick"
                    );
                    state.record_gauges();
                }
            }
            Err(join_err) if join_err.is_cancelled() => {
                debug!(error = %join_err, "Proof task cancelled");
            }
            Err(join_err) => {
                warn!(error = %join_err, "Proof task panicked");
                state.reset();
            }
        }
    }

    /// Attempts to recover on-chain state and fetch the safe head.
    ///
    /// Returns `None` if either step fails (logged as warnings), allowing the
    /// caller to fall through to the poll-tick sleep.
    async fn try_recover_and_plan(
        &self,
        cache: &mut Option<CachedRecovery>,
    ) -> Option<(RecoveredState, u64)> {
        let (state_result, safe_head_result) =
            tokio::join!(self.recover_latest_state(cache), self.latest_safe_block_number(),);

        let state = match state_result {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "Failed to recover on-chain state, retrying next tick");
                return None;
            }
        };

        let safe_head = match safe_head_result {
            Ok(n) => n,
            Err(e) => {
                warn!(error = %e, "Failed to fetch safe head, retrying next tick");
                return None;
            }
        };

        Some((state, safe_head))
    }

    /// Recovers the latest on-chain state using a deterministic forward walk
    /// from the anchor root.
    ///
    /// # Strategy
    ///
    /// 1. Read `game_count` from the factory and anchor root from the registry
    ///    (2 RPC calls per tick — always needed for cache validation).
    /// 2. **Cache check — fast path.** If both `game_count` and `anchor_root`
    ///    match the cache, return the cached state immediately (zero RPCs).
    /// 3. **Forward walk.** Walk from the anchor block, stepping by
    ///    `block_interval`. At each step:
    ///    - Compute expected block number deterministically.
    ///    - Fetch the canonical output root and intermediate roots from the
    ///      rollup node.
    ///    - Build `extraData` from the block number, parent address, and
    ///      intermediate roots.
    ///    - Call `factory.games(gameType, rootClaim, extraData)` to look up
    ///      the game by its unique UUID.
    ///    - If `proxy == Address::ZERO`, no game exists — gap found, stop.
    ///    - Otherwise, advance to the returned proxy as the new parent.
    ///
    /// This approach is deterministic: the correct game for each step is
    /// uniquely identified by its `(gameType, rootClaim, extraData)` tuple.
    /// There is no ambiguity or filtering — the game either exists or it
    /// doesn't.
    ///
    /// # Bounding
    ///
    /// The walk is NOT bounded by the safe/finalized L2 head because it
    /// only verifies existing on-chain games (which were already submitted
    /// and included on L1). New proposal dispatch in [`Self::dispatch_proofs`]
    /// is separately bounded by the safe head.
    async fn recover_latest_state(
        &self,
        cache: &mut Option<CachedRecovery>,
    ) -> Result<RecoveredState, ProposerError> {
        let count = self
            .factory_client
            .game_count()
            .await
            .map_err(|e| ProposerError::Contract(format!("recovery game_count failed: {e}")))?;

        // Read the anchor root early so it can be included in the cache key.
        let anchor = self
            .anchor_registry
            .get_anchor_root()
            .await
            .map_err(|e| ProposerError::Contract(format!("get_anchor_root failed: {e}")))?;

        // The cached tip is valid as long as the anchor hasn't advanced past
        // it. The anchor advances when games resolve (~every 20 min after the
        // dispute window elapses), but it always stays behind the chain tip.
        let tip_still_valid =
            |cached: &CachedRecovery| anchor.l2_block_number <= cached.state.l2_block_number;

        // Fast path: game_count unchanged and anchor still behind tip →
        // return the cached state with zero additional RPCs.
        if let Some(cached) = cache.as_ref()
            && tip_still_valid(cached)
            && cached.game_count == count
        {
            debug!(game_count = count, "No changes since last recovery, returning cached state");
            return Ok(cached.state);
        }

        // ── Forward walk ────────────────────────────────────────────────
        //
        // When game_count increased and the anchor is still at or behind
        // the cached tip, resume from the tip instead of re-walking from
        // the anchor. This turns post-submission recovery from O(K) to
        // O(1).
        //
        // A full walk from the anchor is required when:
        // - No cache exists (cold start / pipeline reset).
        // - The anchor advanced past the cached tip (governance / anomaly).
        // - game_count decreased (L1 reorg removed games).
        let start = match cache.as_ref() {
            Some(cached) if tip_still_valid(cached) && count > cached.game_count => {
                debug!(
                    cached_block = cached.state.l2_block_number,
                    old_count = cached.game_count,
                    new_count = count,
                    "Resuming forward walk from cached tip"
                );
                cached.state
            }
            _ => RecoveredState {
                parent_address: self.config.driver.anchor_state_registry_address,
                output_root: anchor.root,
                l2_block_number: anchor.l2_block_number,
            },
        };

        let state = self.forward_walk(&start).await?;

        *cache = Some(CachedRecovery { game_count: count, state });
        Ok(state)
    }

    /// Performs a deterministic forward walk to find the latest verified game
    /// using UUID-based `games()` lookups.
    ///
    /// The walk starts from `start`, which is either the anchor state (full
    /// walk) or the cached tip from a previous walk (incremental).
    ///
    /// At each step:
    /// 1. Compute the expected block number: `parent_block + block_interval`.
    /// 2. Fetch all intermediate roots (including the target block's output
    ///    root) from the rollup node in a single batch.
    /// 3. Build `extraData` from the block number, parent address, and
    ///    intermediate roots.
    /// 4. Call `factory.games(gameType, rootClaim, extraData)` — the factory
    ///    returns the proxy address if a game with this exact UUID exists, or
    ///    `Address::ZERO` if not.
    /// 5. `Address::ZERO` → gap found, stop. Otherwise advance the parent.
    ///
    /// Because the game's UUID is computed from canonical data, there is no
    /// ambiguity: the correct game either exists or it doesn't. Invalid games
    /// (wrong root claim, wrong parent, wrong intermediate roots) simply have
    /// different UUIDs and are never matched.
    ///
    /// The walk is sequential (each step needs the previous proxy address for
    /// `extraData`), but each step requires only two RPCs: one
    /// `fetch_canonical_roots` batch and one `games()` lookup.
    async fn forward_walk(&self, start: &RecoveredState) -> Result<RecoveredState, ProposerError> {
        let block_interval = self.config.driver.block_interval;
        let game_type = self.config.driver.game_type;

        let log_interval = (block_interval / 5).max(1);

        let mut parent_address = start.parent_address;
        let mut parent_output_root = start.output_root;
        let mut parent_block = start.l2_block_number;
        let mut steps: u64 = 0;

        while let Some(expected_block) = parent_block.checked_add(block_interval) {
            // Fetch all intermediate roots (including the final root at
            // `expected_block`) from the rollup node in one batch. The last
            // element of `intermediate_blocks` is always `expected_block`,
            // so this also provides the canonical output root — no separate
            // `output_at_block` call needed.
            let intermediate_blocks = self.intermediate_block_numbers(parent_block)?;
            let intermediate_roots =
                match self.fetch_canonical_roots(intermediate_blocks.clone()).await {
                    Ok(roots) => roots,
                    Err(ProposerError::Rpc(RpcError::BlockNotFound(_))) => {
                        // The block doesn't exist yet (ahead of safe head).
                        // This is the natural termination point of the walk.
                        debug!(
                            block = expected_block,
                            "Block not available yet, treating as end of walk"
                        );
                        break;
                    }
                    Err(e) => {
                        // All other RPC errors (retryable or not) propagate so
                        // recovery retries on the next tick rather than caching
                        // a partial result.
                        return Err(e);
                    }
                };

            // Extract the canonical root for the target block (always the
            // last intermediate block).
            let canonical_root = *intermediate_roots.get(&expected_block).ok_or_else(|| {
                ProposerError::Internal(format!(
                    "missing canonical root for expected block {expected_block}"
                ))
            })?;

            // Build the ordered intermediate root vector matching extraData layout.
            let intermediate_root_vec: Vec<B256> = intermediate_blocks
                .iter()
                .map(|ib| {
                    intermediate_roots.get(ib).copied().ok_or_else(|| {
                        ProposerError::Internal(format!(
                            "missing canonical root for intermediate block {ib}"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            // Build extraData and look up the game by UUID.
            let extra_data =
                encode_extra_data(expected_block, parent_address, &intermediate_root_vec);

            let lookup =
                self.factory_client.games(game_type, canonical_root, extra_data).await.map_err(
                    |e| {
                        ProposerError::Contract(format!(
                            "games lookup failed at block {expected_block}: {e}"
                        ))
                    },
                )?;

            if lookup == Address::ZERO {
                info!(
                    gap_block = expected_block,
                    parent_block,
                    parent_address = %parent_address,
                    games_verified = steps,
                    "No game found at expected block, will propose from here"
                );
                break;
            }

            parent_address = lookup;
            parent_output_root = canonical_root;
            parent_block = expected_block;
            steps += 1;

            if steps.is_multiple_of(log_interval) {
                info!(
                    games_verified = steps,
                    latest_block = parent_block,
                    "Recovery forward walk in progress"
                );
            }
        }

        if steps > 0 {
            info!(
                latest_block = parent_block,
                parent_address = %parent_address,
                games_verified = steps,
                "Recovery forward walk complete"
            );
        }

        Ok(RecoveredState {
            parent_address,
            output_root: parent_output_root,
            l2_block_number: parent_block,
        })
    }

    /// Returns the latest safe L2 block number.
    async fn latest_safe_block_number(&self) -> Result<u64, ProposerError> {
        let sync_status = self.rollup_client.sync_status().await?;
        if self.config.driver.allow_non_finalized {
            Ok(sync_status.safe_l2.number)
        } else {
            Ok(sync_status.finalized_l2.number)
        }
    }

    /// Concurrently fetches canonical output roots for the given block numbers.
    async fn fetch_canonical_roots(
        &self,
        blocks: Vec<u64>,
    ) -> Result<HashMap<u64, B256>, ProposerError> {
        if blocks.is_empty() {
            return Ok(HashMap::new());
        }
        stream::iter(blocks)
            .map(|block_number| {
                let rollup = &self.rollup_client;
                async move {
                    rollup
                        .output_at_block(block_number)
                        .await
                        .map(|out| (block_number, out.output_root))
                        .map_err(ProposerError::Rpc)
                }
            })
            .buffered(self.config.recovery_scan_concurrency)
            .try_collect()
            .await
    }

    async fn build_proof_request_for(
        &self,
        starting_block_number: u64,
        agreed_output_root: B256,
        target_block: u64,
    ) -> Result<ProofRequest, ProposerError> {
        let (agreed_l2_head, claimed_output, l1_head) = tokio::try_join!(
            async {
                self.l2_client
                    .header_by_number(Some(starting_block_number))
                    .await
                    .map_err(ProposerError::Rpc)
            },
            async {
                self.rollup_client.output_at_block(target_block).await.map_err(ProposerError::Rpc)
            },
            async { self.l1_client.header_by_number(None).await.map_err(ProposerError::Rpc) },
        )?;

        let request = ProofRequest {
            l1_head: l1_head.hash,
            agreed_l2_head_hash: agreed_l2_head.hash,
            agreed_l2_output_root: agreed_output_root,
            claimed_l2_output_root: claimed_output.output_root,
            claimed_l2_block_number: target_block,
            proposer: self.config.driver.proposer_address,
            intermediate_block_interval: self.config.driver.intermediate_block_interval,
            l1_head_number: l1_head.number,
            image_hash: self.config.driver.tee_image_hash,
        };

        info!(request = ?request, "Built proof request for parallel proving");

        Ok(request)
    }

    /// Recovers the TEE signer from the aggregate proposal and checks
    /// `isValidSigner` on the `TEEProverRegistry`.
    ///
    /// Returns `Ok(true)` if the signer is valid, `Ok(false)` if not,
    /// or `Err` if the check itself failed (RPC error, parse failure, etc.).
    async fn check_signer_validity(
        &self,
        aggregate_proposal: &base_proof_primitives::Proposal,
        starting_block_number: u64,
        intermediate_roots: &[B256],
        registry_address: Address,
    ) -> Result<bool, ProposerError> {
        // Reconstruct the journal that the enclave signed over.
        let journal = ProofJournal {
            proposer: self.config.driver.proposer_address,
            l1_origin_hash: aggregate_proposal.l1_origin_hash,
            prev_output_root: aggregate_proposal.prev_output_root,
            starting_l2_block: starting_block_number,
            output_root: aggregate_proposal.output_root,
            ending_l2_block: aggregate_proposal.l2_block_number,
            intermediate_roots: intermediate_roots.to_vec(),
            config_hash: aggregate_proposal.config_hash,
            tee_image_hash: self.config.driver.tee_image_hash,
        };
        let digest = keccak256(journal.encode());

        // Parse the 65-byte ECDSA signature (r ‖ s ‖ v).
        let sig_bytes = aggregate_proposal.signature.as_ref();
        let sig = Signature::try_from(sig_bytes)
            .map_err(|e| ProposerError::Internal(format!("invalid proposal signature: {e}")))?;

        let signer = sig
            .recover_address_from_prehash(&digest)
            .map_err(|e| ProposerError::Internal(format!("signer recovery failed: {e}")))?;

        debug!(signer = %signer, "recovered TEE signer from aggregate proposal");

        // Call isValidSigner on the registry via the L1 provider.
        let calldata = ITEEProverRegistry::isValidSignerCall { signer }.abi_encode();
        let result = self
            .l1_client
            .call_contract(registry_address, calldata.into(), None)
            .await
            .map_err(ProposerError::Rpc)?;

        let is_valid =
            ITEEProverRegistry::isValidSignerCall::abi_decode_returns(&result).map_err(|e| {
                ProposerError::Internal(format!("failed to decode isValidSigner response: {e}"))
            })?;
        debug!(signer = %signer, is_valid, "isValidSigner check result");

        Ok(is_valid)
    }

    #[instrument(skip_all, fields(target_block = target_block, parent_address = %parent_address))]
    async fn validate_and_submit(
        &self,
        proof_result: &ProofResult,
        target_block: u64,
        parent_address: Address,
    ) -> Result<(), SubmitAction> {
        let (aggregate_proposal, proposals) = match proof_result {
            ProofResult::Tee { aggregate_proposal, proposals } => (aggregate_proposal, proposals),
            ProofResult::Zk { .. } => {
                return Err(SubmitAction::Failed(ProposerError::Prover(
                    "unexpected ZK proof result from TEE prover".into(),
                )));
            }
        };

        // JIT validation: check that the proved output root still matches canonical.
        let canonical_output = self
            .rollup_client
            .output_at_block(target_block)
            .await
            .map_err(|e| SubmitAction::Failed(ProposerError::Rpc(e)))?;

        if aggregate_proposal.output_root != canonical_output.output_root {
            warn!(
                proposal_root = ?aggregate_proposal.output_root,
                canonical_root = ?canonical_output.output_root,
                target_block,
                "Proposal output root does not match canonical chain at submit time"
            );
            return Err(SubmitAction::RootMismatch);
        }

        // Extract intermediate roots.
        let starting_block_number =
            target_block.checked_sub(self.config.driver.block_interval).ok_or_else(|| {
                SubmitAction::Failed(ProposerError::Internal(format!(
                    "target_block {target_block} < block_interval {}",
                    self.config.driver.block_interval
                )))
            })?;
        let intermediate_blocks =
            self.intermediate_block_numbers(starting_block_number).map_err(SubmitAction::Failed)?;
        let intermediate_roots = self
            .extract_intermediate_roots(starting_block_number, proposals, &intermediate_blocks)
            .map_err(SubmitAction::Failed)?;

        // Fetch canonical roots for non-target intermediate blocks only;
        // the target block was already fetched for the JIT check above.
        let non_target_blocks: Vec<u64> =
            intermediate_blocks.iter().copied().filter(|&b| b != target_block).collect();

        let mut canonical_map: HashMap<u64, B256> =
            self.fetch_canonical_roots(non_target_blocks).await.map_err(SubmitAction::Failed)?;
        canonical_map.insert(target_block, canonical_output.output_root);

        for (root, block) in intermediate_roots.iter().zip(intermediate_blocks.iter()) {
            let canonical = canonical_map.get(block).ok_or_else(|| {
                SubmitAction::Failed(ProposerError::Internal(format!(
                    "missing canonical root for intermediate block {block}"
                )))
            })?;
            if *root != *canonical {
                warn!(
                    intermediate_block = *block,
                    proposal_root = ?root,
                    canonical_root = ?canonical,
                    target_block,
                    "Intermediate output root does not match canonical chain at submit time"
                );
                return Err(SubmitAction::RootMismatch);
            }
        }

        // Pre-submission signer validation: if a TEE prover registry is
        // configured, recover the signer from the aggregate proposal signature
        // and check `isValidSigner` on-chain. If the signer is invalid, skip
        // submission to avoid wasting gas on a transaction that will revert.
        if let Some(registry_address) = self.config.tee_prover_registry_address {
            match self
                .check_signer_validity(
                    aggregate_proposal,
                    starting_block_number,
                    &intermediate_roots,
                    registry_address,
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    // The proof's signer is not registered on-chain. Discard
                    // this proof so the pipeline re-proves with a (potentially
                    // different, registered) enclave on the next attempt.
                    warn!(target_block, "TEE signer is not valid on-chain, discarding proof");
                    Metrics::tee_signer_invalid_total().increment(1);
                    return Err(SubmitAction::Discard(ProposerError::Internal(
                        "TEE signer not registered on-chain".into(),
                    )));
                }
                Err(e) => {
                    // Proceed on RPC failure: if L1 is unreachable, the
                    // subsequent propose_output call will also fail and be
                    // retried naturally. Blocking here would not save gas.
                    // This also handles the case where the registry contract
                    // is not yet deployed (rolling out the --tee-prover-registry-address
                    // config before the contract exists on-chain).
                    warn!(error = %e, target_block, "signer validity check failed, proceeding anyway");
                }
            }
        }

        info!(
            target_block,
            output_root = ?aggregate_proposal.output_root,
            parent_address = %parent_address,
            intermediate_roots_count = intermediate_roots.len(),
            proposals_count = proposals.len(),
            "Proposing output (creating dispute game)"
        );

        // Submit with timeout.
        let mut propose_timer = base_metrics::timed!(Metrics::proposal_l1_tx_duration_seconds());
        let propose_result = tokio::time::timeout(
            PROPOSAL_TIMEOUT,
            self.output_proposer.propose_output(
                aggregate_proposal,
                parent_address,
                &intermediate_roots,
            ),
        )
        .await;

        match propose_result {
            Ok(Ok(())) => {
                drop(propose_timer);
                info!(target_block, "Dispute game created successfully");
                Metrics::l2_output_proposals_total().increment(1);
                Ok(())
            }
            Ok(Err(e)) => {
                if e.is_game_already_exists() {
                    drop(propose_timer);
                    info!(
                        target_block,
                        "Game already exists, next tick will load fresh state from chain"
                    );
                    Err(SubmitAction::GameAlreadyExists)
                } else {
                    propose_timer.disarm();
                    Err(SubmitAction::Failed(e))
                }
            }
            Err(_) => {
                propose_timer.disarm();
                Err(SubmitAction::Failed(ProposerError::Internal(format!(
                    "dispute game creation timed out after {}s",
                    PROPOSAL_TIMEOUT.as_secs()
                ))))
            }
        }
    }

    /// Returns intermediate block numbers between `starting_block_number` and
    /// the next proposal target, stepping by `intermediate_block_interval`.
    fn intermediate_block_numbers(
        &self,
        starting_block_number: u64,
    ) -> Result<Vec<u64>, ProposerError> {
        let interval = self.config.driver.intermediate_block_interval;
        if interval == 0 {
            return Err(ProposerError::Config(
                "intermediate_block_interval must not be zero".into(),
            ));
        }
        let count = self.config.driver.block_interval / interval;
        (1..=count)
            .map(|i| {
                starting_block_number
                    .checked_add(i.checked_mul(interval).ok_or_else(|| {
                        ProposerError::Internal(
                            "overflow computing intermediate block number".into(),
                        )
                    })?)
                    .ok_or_else(|| {
                        ProposerError::Internal(
                            "overflow computing intermediate block number".into(),
                        )
                    })
            })
            .collect()
    }

    /// Extracts intermediate output roots from per-block proposals.
    ///
    /// Samples at every `intermediate_block_interval` within the range.
    fn extract_intermediate_roots(
        &self,
        starting_block_number: u64,
        proposals: &[base_proof_primitives::Proposal],
        blocks: &[u64],
    ) -> Result<Vec<B256>, ProposerError> {
        let mut roots = Vec::with_capacity(blocks.len());
        for &target_block in blocks {
            let idx = target_block.checked_sub(starting_block_number + 1).ok_or_else(|| {
                ProposerError::Internal(format!(
                    "underflow computing proposal index for block {target_block}"
                ))
            })?;
            if let Some(p) = proposals.get(idx as usize) {
                roots.push(p.output_root);
            } else {
                return Err(ProposerError::Internal(format!(
                    "intermediate root at block {target_block} not found in proposals (index {idx}, len {})",
                    proposals.len()
                )));
            }
        }
        Ok(roots)
    }
}

/// Internal action after a submission attempt.
#[derive(Debug)]
enum SubmitAction {
    /// Output root mismatch — proved root no longer matches canonical chain.
    RootMismatch,
    /// The dispute game already exists on-chain by a previous attempt whose
    /// result was lost to an RPC propagation delay. The pipeline must invalidate
    /// its recovery cache so the next forward walk discovers the existing game.
    GameAlreadyExists,
    /// Transient failure — retry later with the same proof.
    Failed(ProposerError),
    /// Proof is permanently invalid (e.g. signer not registered) — discard
    /// and re-prove on the next attempt.
    Discard(ProposerError),
}

impl std::fmt::Display for SubmitAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RootMismatch => write!(f, "output root mismatch"),
            Self::GameAlreadyExists => write!(f, "game already exists"),
            Self::Failed(e) | Self::Discard(e) => write!(f, "{e}"),
        }
    }
}

/// Result of a concurrent submission task, returned to the coordinator.
enum SubmitOutcome {
    Success { target_block: u64 },
    GameAlreadyExists { target_block: u64 },
    RootMismatch { target_block: u64 },
    Failed { target_block: u64, proof: ProofResult, error: ProposerError },
    Discard { target_block: u64, error: ProposerError },
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, time::Duration};

    use alloy_primitives::{Address, B256};
    use base_proof_primitives::{ProofResult, Proposal, ProverClient};
    use rstest::rstest;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
        MockOutputProposer, MockProver, MockRollupClient, test_anchor_root, test_proposal,
        test_sync_status,
    };

    // ---- Named constants for test data ----

    /// Game type used across recovery tests.
    const TEST_GAME_TYPE: u32 = 42;

    /// Default block interval for recovery tests (matches `DriverConfig` default).
    const TEST_BLOCK_INTERVAL: u64 = 512;

    /// Default anchor block number.
    const TEST_ANCHOR_BLOCK: u64 = 0;

    /// Default L1 block number returned by `MockL1`.
    const TEST_L1_BLOCK_NUMBER: u64 = 1000;

    /// Default mock prover delay for recovery tests (minimal).
    const MOCK_PROVER_DELAY: Duration = Duration::from_millis(1);

    // ---- Helper builders for game data ----

    /// Helper: unique proxy address derived from an index.
    ///
    /// Uses `index + 1` so that `proxy_addr(0)` is never `Address::ZERO`
    /// (which the factory uses as the "no game found" sentinel).
    fn proxy_addr(index: u64) -> Address {
        let mut bytes = [0u8; 20];
        bytes[12..20].copy_from_slice(&(index + 1).to_be_bytes());
        Address::new(bytes)
    }

    /// Builds a chain of `N` sequential games starting from the anchor,
    /// registering them in the factory's `uuid_games` map.
    ///
    /// Uses `block_interval == intermediate_block_interval == TEST_BLOCK_INTERVAL`
    /// (one intermediate root per game, equal to the root claim).
    ///
    /// Returns `(factory, output_roots)` ready to use in pipeline builders.
    fn game_chain(n: usize) -> (MockDisputeGameFactory, HashMap<u64, B256>) {
        game_chain_full(n, TEST_ANCHOR_BLOCK, TEST_BLOCK_INTERVAL, TEST_BLOCK_INTERVAL)
    }

    /// Builds a chain of `N` sequential games with configurable intervals.
    fn game_chain_full(
        n: usize,
        anchor_block: u64,
        block_interval: u64,
        intermediate_block_interval: u64,
    ) -> (MockDisputeGameFactory, HashMap<u64, B256>) {
        let mut uuid_games = std::collections::HashMap::new();
        let mut output_roots = HashMap::new();
        let intermediate_count = block_interval / intermediate_block_interval;

        let mut parent = Address::ZERO; // anchor_state_registry_address default
        for i in 0..n {
            let block = anchor_block + block_interval * (i as u64 + 1);
            let root_claim = B256::repeat_byte((i as u8) + 1);

            // Build intermediate roots (canonical values).
            let parent_block = block - block_interval;
            let mut intermediate_roots = Vec::with_capacity(intermediate_count as usize);
            for j in 1..=intermediate_count {
                let ib = parent_block + j * intermediate_block_interval;
                let ir = if ib == block { root_claim } else { B256::repeat_byte(ib as u8) };
                output_roots.insert(ib, ir);
                intermediate_roots.push(ir);
            }
            output_roots.insert(block, root_claim);

            let extra_data = encode_extra_data(block, parent, &intermediate_roots);
            let proxy = proxy_addr(i as u64);

            uuid_games.insert((TEST_GAME_TYPE, root_claim, extra_data), proxy);

            parent = proxy;
        }

        let factory = MockDisputeGameFactory {
            games: Vec::new(),
            game_count_override: Some(n as u64),
            uuid_games,
            games_should_fail: false,
        };

        (factory, output_roots)
    }

    // ---- Pipeline builders ----

    /// Type alias to reduce repetition in builder return types.
    type TestPipeline = ProvingPipeline<
        MockL1,
        MockL2,
        MockRollupClient,
        MockAnchorStateRegistry,
        MockDisputeGameFactory,
    >;

    fn test_pipeline(
        pipeline_config: PipelineConfig,
        safe_block_number: u64,
        cancel: CancellationToken,
    ) -> TestPipeline {
        let l1 = Arc::new(MockL1 { latest_block_number: TEST_L1_BLOCK_NUMBER });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let prover: Arc<dyn ProverClient> = Arc::new(MockProver {
            delay: Duration::from_millis(10),
            block_interval: pipeline_config.driver.block_interval,
        });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(safe_block_number, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let anchor_registry =
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(TEST_ANCHOR_BLOCK) });
        let factory = Arc::new(MockDisputeGameFactory::with_games(vec![]));

        ProvingPipeline::new(
            pipeline_config,
            prover,
            l1,
            l2,
            rollup,
            anchor_registry,
            factory,
            Arc::new(MockAggregateVerifier::default()),
            Arc::new(MockOutputProposer),
            cancel,
        )
    }

    /// Builds a recovery pipeline with a pre-configured factory and canonical
    /// output roots. Uses default anchor block and block interval.
    fn recovery_pipeline(
        factory: MockDisputeGameFactory,
        output_roots: HashMap<u64, B256>,
    ) -> TestPipeline {
        recovery_pipeline_full(
            factory,
            output_roots,
            TEST_ANCHOR_BLOCK,
            TEST_BLOCK_INTERVAL,
            TEST_BLOCK_INTERVAL,
        )
    }

    fn recovery_pipeline_full(
        factory: MockDisputeGameFactory,
        output_roots: HashMap<u64, B256>,
        anchor_block: u64,
        block_interval: u64,
        intermediate_block_interval: u64,
    ) -> TestPipeline {
        let cancel = CancellationToken::new();
        let l1 = Arc::new(MockL1 { latest_block_number: TEST_L1_BLOCK_NUMBER });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let prover: Arc<dyn ProverClient> =
            Arc::new(MockProver { delay: MOCK_PROVER_DELAY, block_interval });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(0, B256::ZERO),
            output_roots,
            max_safe_block: None,
        });
        let anchor_registry =
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(anchor_block) });

        ProvingPipeline::new(
            PipelineConfig {
                max_parallel_proofs: 1,
                max_retries: 1,
                recovery_scan_concurrency: 8,
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    game_type: TEST_GAME_TYPE,
                    block_interval,
                    intermediate_block_interval,
                    ..Default::default()
                },
            },
            prover,
            l1,
            l2,
            rollup,
            anchor_registry,
            Arc::new(factory),
            Arc::new(MockAggregateVerifier::default()),
            Arc::new(MockOutputProposer),
            cancel,
        )
    }

    // ---- Pipeline lifecycle tests ----

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_pipeline_cancellation() {
        let cancel = CancellationToken::new();
        let pipeline = test_pipeline(
            PipelineConfig {
                max_parallel_proofs: 2,
                max_retries: 3,
                recovery_scan_concurrency: 8,
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    poll_interval: Duration::from_secs(3600),
                    block_interval: TEST_BLOCK_INTERVAL,
                    intermediate_block_interval: TEST_BLOCK_INTERVAL,
                    ..Default::default()
                },
            },
            200, // safe head below first target, so no proofs dispatched
            cancel.clone(),
        );

        let handle = tokio::spawn(async move { pipeline.run().await });
        cancel.cancel();

        let result = handle.await.expect("task should not panic");
        assert!(result.is_ok(), "run() should return Ok on cancellation");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_pipeline_proves_and_submits() {
        let cancel = CancellationToken::new();
        let pipeline = test_pipeline(
            PipelineConfig {
                max_parallel_proofs: 2,
                max_retries: 3,
                recovery_scan_concurrency: 8,
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    poll_interval: Duration::from_millis(100),
                    block_interval: TEST_BLOCK_INTERVAL,
                    intermediate_block_interval: TEST_BLOCK_INTERVAL,
                    ..Default::default()
                },
            },
            TEST_BLOCK_INTERVAL, // safe head at first target block
            cancel.clone(),
        );

        let handle = tokio::spawn(async move { pipeline.run().await });

        tokio::time::sleep(Duration::from_secs(5)).await;
        cancel.cancel();

        let result = handle.await.expect("task should not panic");
        assert!(result.is_ok());
    }

    // ---- Recovery: empty factory ----

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_returns_anchor_when_no_games() {
        let factory = MockDisputeGameFactory::with_games(vec![]);
        let pipeline = recovery_pipeline(factory, HashMap::new());

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(
            state.parent_address,
            Address::ZERO,
            "should return anchor_state_registry_address"
        );
        assert_eq!(state.l2_block_number, TEST_ANCHOR_BLOCK, "should return anchor block");
        assert!(cache.is_some(), "cache should still be populated");
    }

    // ---- Recovery: forward walk ----

    #[rstest]
    #[case::single_game(1, 0, TEST_BLOCK_INTERVAL, "single game at first interval")]
    #[case::chain_of_two(2, 1, TEST_BLOCK_INTERVAL * 2, "chain of two sequential games")]
    #[case::chain_of_five(5, 4, TEST_BLOCK_INTERVAL * 5, "chain of five sequential games")]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_forward_walk_chain(
        #[case] game_count: usize,
        #[case] expected_proxy_index: u64,
        #[case] expected_block: u64,
        #[case] scenario: &str,
    ) {
        let (factory, output_roots) = game_chain(game_count);
        let pipeline = recovery_pipeline(factory, output_roots);

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(expected_proxy_index), "{scenario}");
        assert_eq!(state.l2_block_number, expected_block, "{scenario}");
        assert!(cache.is_some(), "{scenario}: cache should be populated");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_forward_walk_stops_at_gap() {
        // Game at block 512 exists, but no game at block 1024.
        // Walk should stop after the first game.
        let root_1 = B256::repeat_byte(0x01);
        let extra_data_1 = encode_extra_data(TEST_BLOCK_INTERVAL, Address::ZERO, &[root_1]);

        let mut factory = MockDisputeGameFactory::with_games(vec![]);
        factory.game_count_override = Some(1);
        factory.uuid_games.insert((TEST_GAME_TYPE, root_1, extra_data_1), proxy_addr(0));

        let output_roots = HashMap::from([(TEST_BLOCK_INTERVAL, root_1)]);

        let pipeline = recovery_pipeline(factory, output_roots);

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(0), "should stop at first game before gap");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(state.output_root, root_1);
    }

    // ---- Recovery: error propagation ----

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_propagates_games_lookup_failure() {
        // A chain of 2 games exists, but factory.games() always fails.
        // The walk should propagate the error as ProposerError::Contract.
        let (mut factory, output_roots) = game_chain(2);
        factory.games_should_fail = true;

        let pipeline = recovery_pipeline(factory, output_roots);

        let mut cache: Option<CachedRecovery> = None;
        let result = pipeline.recover_latest_state(&mut cache).await;

        assert!(result.is_err(), "games() failure should propagate");
        let err = result.unwrap_err();
        assert!(
            matches!(err, ProposerError::Contract(_)),
            "expected ProposerError::Contract, got {err:?}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_forward_walk_stops_at_safe_head() {
        // 3 games exist on-chain, but the rollup node only has blocks up to
        // block 2 * TEST_BLOCK_INTERVAL. The walk should verify games 0 and 1,
        // then terminate gracefully when it can't fetch the output root for
        // game 2's block (ahead of safe head).
        let (factory, output_roots) = game_chain(3);

        let cancel = CancellationToken::new();
        let l1 = Arc::new(MockL1 { latest_block_number: TEST_L1_BLOCK_NUMBER });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let prover: Arc<dyn ProverClient> =
            Arc::new(MockProver { delay: MOCK_PROVER_DELAY, block_interval: TEST_BLOCK_INTERVAL });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(0, B256::ZERO),
            output_roots,
            max_safe_block: Some(TEST_BLOCK_INTERVAL * 2),
        });
        let anchor_registry =
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(TEST_ANCHOR_BLOCK) });

        let pipeline = ProvingPipeline::new(
            PipelineConfig {
                max_parallel_proofs: 1,
                max_retries: 1,
                recovery_scan_concurrency: 8,
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    game_type: TEST_GAME_TYPE,
                    block_interval: TEST_BLOCK_INTERVAL,
                    intermediate_block_interval: TEST_BLOCK_INTERVAL,
                    ..Default::default()
                },
            },
            prover,
            l1,
            l2,
            rollup,
            anchor_registry,
            Arc::new(factory),
            Arc::new(MockAggregateVerifier::default()),
            Arc::new(MockOutputProposer),
            cancel,
        );

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        // Should stop after game 1 (block 1024), not reach game 2 (block 1536).
        assert_eq!(state.parent_address, proxy_addr(1), "should stop at game 1");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL * 2);
    }

    // ---- Recovery: caching ----

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_hit_equal_game_count() {
        let (factory, output_roots) = game_chain(1);
        let game_proxy = proxy_addr(0);

        let pipeline = recovery_pipeline(factory, output_roots);

        // First call: cold start, populates the cache.
        let mut cache: Option<CachedRecovery> = None;
        let state1 = pipeline.recover_latest_state(&mut cache).await.unwrap();
        assert!(cache.is_some(), "cache should be populated after first call");
        assert_eq!(state1.parent_address, game_proxy);
        assert_eq!(state1.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(cache.as_ref().unwrap().game_count, 1);

        // Second call: same game_count → cached state returned without re-walk.
        let state2 = pipeline.recover_latest_state(&mut cache).await.unwrap();
        assert_eq!(state2.parent_address, state1.parent_address);
        assert_eq!(state2.l2_block_number, state1.l2_block_number);
        assert_eq!(state2.output_root, state1.output_root);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_incremental_on_count_increase() {
        // Seed cache with game_count=1, state at game 0. Factory now has 2
        // games. Anchor is still at block 0 (behind the cached tip at
        // TEST_BLOCK_INTERVAL), so the walk resumes from the cached tip
        // and only needs to discover game 1.
        let (factory, output_roots) = game_chain(2);

        let mut cache = Some(CachedRecovery {
            game_count: 1,
            state: RecoveredState {
                parent_address: proxy_addr(0),
                output_root: B256::repeat_byte(0x01),
                l2_block_number: TEST_BLOCK_INTERVAL,
            },
        });

        let pipeline = recovery_pipeline(factory, output_roots);
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(1), "should find game 1 incrementally");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL * 2);
        assert_eq!(cache.as_ref().unwrap().game_count, 2, "cache should reflect new count");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_incremental_resumes_mid_chain() {
        // Build a chain of 5 games. Seed cache at game 2 (game_count=3).
        // Factory now has 5 games. The walk should resume from game 2's
        // tip and discover games 3 and 4 without re-walking games 0–2.
        let (factory, output_roots) = game_chain(5);

        let mut cache = Some(CachedRecovery {
            game_count: 3,
            state: RecoveredState {
                parent_address: proxy_addr(2),
                output_root: B256::repeat_byte(0x03),
                l2_block_number: TEST_BLOCK_INTERVAL * 3,
            },
        });

        let pipeline = recovery_pipeline(factory, output_roots);
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(4), "should reach game 4 from cached tip");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL * 5);
        assert_eq!(cache.as_ref().unwrap().game_count, 5);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_incremental_unrelated_games() {
        // game_count increased (1 → 2) but the new game is not in our
        // chain (no UUID entry at the next expected block). The incremental
        // walk resumes from the cached tip, finds nothing, and returns the
        // same state. This happens when another proposer creates a game
        // with different parameters.
        let (factory, output_roots) = game_chain(1);
        // factory has game_count=1, but we'll seed cache as game_count=0
        // so the code sees an increase (0 → 1). The walk from the anchor
        // will find game 0. But to test the "unrelated game" path, we need
        // game_count > cached_count and no new UUID at the next block.
        //
        // Seed cache at game 0, pretend game_count was 1. Factory reports
        // game_count=2 (simulating someone else's unrelated game), but
        // there's no UUID entry at block 2*TEST_BLOCK_INTERVAL.
        let mut factory_with_extra_count = factory;
        factory_with_extra_count.game_count_override = Some(2);

        let pipeline = recovery_pipeline(factory_with_extra_count, output_roots);

        let mut cache = Some(CachedRecovery {
            game_count: 1,
            state: RecoveredState {
                parent_address: proxy_addr(0),
                output_root: B256::repeat_byte(0x01),
                l2_block_number: TEST_BLOCK_INTERVAL,
            },
        });

        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        // Walk resumed from game 0, found no game at the next block,
        // returned the same state.
        assert_eq!(state.parent_address, proxy_addr(0), "should remain at game 0");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(cache.as_ref().unwrap().game_count, 2, "cache updated to new count");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_invalidated_by_count_decrease() {
        // Seed cache with game_count=5. Factory now has only 1 game (reorg).
        let (factory, output_roots) = game_chain(1);

        let mut cache = Some(CachedRecovery {
            game_count: 5,
            state: RecoveredState {
                parent_address: proxy_addr(99),
                output_root: B256::repeat_byte(0xDD),
                l2_block_number: 5 * TEST_BLOCK_INTERVAL,
            },
        });

        let pipeline = recovery_pipeline(factory, output_roots);
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(0), "reorg: should find the 1 remaining game");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(cache.as_ref().unwrap().game_count, 1, "reorg: cache should reflect new count");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_full_walk_when_anchor_past_tip() {
        // Anchor is at block 2048 (past the cached tip at block 512).
        // This simulates a governance intervention that advanced the
        // anchor past the cached tip. A full walk from the new anchor
        // is required.
        let anchor_block = TEST_BLOCK_INTERVAL * 4; // block 2048
        let (factory, output_roots) =
            game_chain_full(1, anchor_block, TEST_BLOCK_INTERVAL, TEST_BLOCK_INTERVAL);

        let mut cache = Some(CachedRecovery {
            game_count: 0,
            state: RecoveredState {
                parent_address: proxy_addr(99), // stale — will be recomputed
                output_root: B256::repeat_byte(0xDD),
                l2_block_number: TEST_BLOCK_INTERVAL, // tip at 512, anchor at 2048
            },
        });

        let pipeline = recovery_pipeline_full(
            factory,
            output_roots,
            anchor_block,
            TEST_BLOCK_INTERVAL,
            TEST_BLOCK_INTERVAL,
        );
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        // Anchor past cached tip → full walk from new anchor.
        assert_eq!(state.parent_address, proxy_addr(0));
        assert_eq!(state.l2_block_number, anchor_block + TEST_BLOCK_INTERVAL);
    }

    // ---- Recovery: intermediate roots with multiple checkpoints ----

    /// Block intervals for recovery tests with multiple intermediate roots.
    const RECOVERY_BI: u64 = 4;
    const RECOVERY_IBI: u64 = 2;

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_forward_walk_with_intermediate_roots() {
        // block_interval = 4, intermediate_block_interval = 2
        // → intermediate_count = 2 (roots at parent+2 and parent+4)
        //
        // Two games: block 4 (parent = anchor) and block 8 (parent = game 0).
        // Both have correct UUID including intermediate roots. Walk should
        // traverse both games.
        let (factory, output_roots) =
            game_chain_full(2, TEST_ANCHOR_BLOCK, RECOVERY_BI, RECOVERY_IBI);

        let pipeline = recovery_pipeline_full(
            factory,
            output_roots,
            TEST_ANCHOR_BLOCK,
            RECOVERY_BI,
            RECOVERY_IBI,
        );

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        // Both games verified, walk should reach game 1.
        assert_eq!(state.parent_address, proxy_addr(1));
        assert_eq!(state.l2_block_number, RECOVERY_BI * 2);
    }

    // ---- Dispatch: slot filling ----

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_dispatch_skips_inflight_and_proved_blocks() {
        // Scenario: 4 proof slots, blocks 512–2048 were dispatched on the
        // first tick.  Proof for 512 completed (now in `proved`), proofs
        // for 1024/1536/2048 are still in-flight.  The next tick calls
        // dispatch_proofs which must skip past all four handled blocks and
        // dispatch block 2560 to refill the freed slot.
        let cancel = CancellationToken::new();
        let safe_head = TEST_BLOCK_INTERVAL * 6;

        let l1 = Arc::new(MockL1 { latest_block_number: TEST_L1_BLOCK_NUMBER });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let prover: Arc<dyn ProverClient> = Arc::new(MockProver {
            delay: Duration::from_secs(3600),
            block_interval: TEST_BLOCK_INTERVAL,
        });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(safe_head, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let anchor_registry =
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(TEST_ANCHOR_BLOCK) });
        let factory = Arc::new(MockDisputeGameFactory::with_games(vec![]));

        let pipeline = ProvingPipeline::new(
            PipelineConfig {
                max_parallel_proofs: 4,
                max_retries: 3,
                recovery_scan_concurrency: 8,
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    block_interval: TEST_BLOCK_INTERVAL,
                    intermediate_block_interval: TEST_BLOCK_INTERVAL,
                    ..Default::default()
                },
            },
            prover,
            l1,
            l2,
            rollup,
            anchor_registry,
            factory,
            Arc::new(MockAggregateVerifier::default()),
            Arc::new(MockOutputProposer),
            cancel,
        );

        let recovered = RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::ZERO,
            l2_block_number: TEST_ANCHOR_BLOCK,
        };

        let mut state = PipelineState::new();
        state.proved.insert(TEST_BLOCK_INTERVAL, {
            let p = test_proposal(TEST_BLOCK_INTERVAL);
            ProofResult::Tee { aggregate_proposal: p.clone(), proposals: vec![p] }
        });
        state.inflight.insert(TEST_BLOCK_INTERVAL * 2);
        state.inflight.insert(TEST_BLOCK_INTERVAL * 3);
        state.inflight.insert(TEST_BLOCK_INTERVAL * 4);

        pipeline.dispatch_proofs(&recovered, safe_head, &mut state).await.unwrap();

        assert!(
            state.inflight.contains(&(TEST_BLOCK_INTERVAL * 5)),
            "block {} should have been dispatched to fill the freed slot",
            TEST_BLOCK_INTERVAL * 5
        );
        assert_eq!(state.inflight.len(), 4, "should be back to max_parallel_proofs");
        assert!(
            state.proved.contains_key(&TEST_BLOCK_INTERVAL),
            "proved entries must not be removed by dispatch"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_dispatch_skips_submitting_block() {
        let cancel = CancellationToken::new();
        let safe_head = TEST_BLOCK_INTERVAL * 4;

        let l1 = Arc::new(MockL1 { latest_block_number: TEST_L1_BLOCK_NUMBER });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let prover: Arc<dyn ProverClient> = Arc::new(MockProver {
            delay: Duration::from_secs(3600),
            block_interval: TEST_BLOCK_INTERVAL,
        });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(safe_head, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let anchor_registry =
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(TEST_ANCHOR_BLOCK) });
        let factory = Arc::new(MockDisputeGameFactory::with_games(vec![]));

        let pipeline = ProvingPipeline::new(
            PipelineConfig {
                max_parallel_proofs: 4,
                max_retries: 3,
                recovery_scan_concurrency: 8,
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    block_interval: TEST_BLOCK_INTERVAL,
                    intermediate_block_interval: TEST_BLOCK_INTERVAL,
                    ..Default::default()
                },
            },
            prover,
            l1,
            l2,
            rollup,
            anchor_registry,
            factory,
            Arc::new(MockAggregateVerifier::default()),
            Arc::new(MockOutputProposer),
            cancel,
        );

        let recovered = RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::ZERO,
            l2_block_number: TEST_ANCHOR_BLOCK,
        };

        let mut state = PipelineState::new();
        state.submitting = Some(TEST_BLOCK_INTERVAL);

        pipeline.dispatch_proofs(&recovered, safe_head, &mut state).await.unwrap();

        assert!(
            !state.inflight.contains(&TEST_BLOCK_INTERVAL),
            "submitting block must not be re-dispatched"
        );
        assert!(
            state.inflight.contains(&(TEST_BLOCK_INTERVAL * 2)),
            "block after submitting should be dispatched"
        );
    }

    // ---- State management tests ----

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_prune_stale_does_not_abort_inflight_submit() {
        let mut state = PipelineState::new();
        state.submitting = Some(512);
        state.proved.insert(512, {
            let p = test_proposal(512);
            ProofResult::Tee { aggregate_proposal: p.clone(), proposals: vec![p] }
        });
        state.inflight.insert(512);
        state.retry_counts.insert(512, 1);

        state.submit_tasks.spawn(async { SubmitOutcome::Success { target_block: 512 } });

        state.prune_stale(512);

        assert!(state.proved.is_empty());
        assert!(state.inflight.is_empty());
        assert!(state.retry_counts.is_empty());
        assert!(!state.submit_tasks.is_empty(), "submit task must not be aborted by prune_stale");

        let result = state.submit_tasks.join_next().await.expect("task should exist");
        let outcome = result.expect("task should complete without cancellation");
        assert!(
            matches!(outcome, SubmitOutcome::Success { target_block: 512 }),
            "submit task should produce Success, not be cancelled"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_pipeline_state_reset_clears_cache() {
        let mut state = PipelineState::new();
        state.cached_recovery = Some(CachedRecovery {
            game_count: 10,
            state: RecoveredState {
                parent_address: proxy_addr(5),
                output_root: B256::repeat_byte(0x11),
                l2_block_number: TEST_BLOCK_INTERVAL,
            },
        });

        state.reset();
        assert!(state.cached_recovery.is_none(), "reset() should clear cached_recovery");
    }

    // ---- Intermediate output root validation (submission) tests ----

    /// Shared block intervals for submission validation tests.
    const SUBMIT_BLOCK_INTERVAL: u64 = 4;
    const SUBMIT_INTERMEDIATE_INTERVAL: u64 = 2;

    fn submit_pipeline(output_roots: HashMap<u64, B256>) -> TestPipeline {
        recovery_pipeline_full(
            MockDisputeGameFactory::with_games(vec![]),
            output_roots,
            TEST_ANCHOR_BLOCK,
            SUBMIT_BLOCK_INTERVAL,
            SUBMIT_INTERMEDIATE_INTERVAL,
        )
    }

    fn submit_proof_result(target_block: u64) -> ProofResult {
        let proposals: Vec<Proposal> = (1..=target_block).map(test_proposal).collect();
        let aggregate = test_proposal(target_block);
        ProofResult::Tee { aggregate_proposal: aggregate, proposals }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_validate_and_submit_intermediate_roots_match() {
        // MockRollupClient returns B256::repeat_byte(n) for blocks without
        // explicit entries, which matches test_proposal(n).
        let pipeline = submit_pipeline(HashMap::new());
        let proof_result = submit_proof_result(SUBMIT_BLOCK_INTERVAL);

        let result =
            pipeline.validate_and_submit(&proof_result, SUBMIT_BLOCK_INTERVAL, Address::ZERO).await;
        assert!(result.is_ok(), "all roots match, submission should succeed");
    }

    #[rstest]
    #[case::intermediate_mismatch(2, "intermediate root at block 2 differs from canonical")]
    #[case::final_mismatch(4, "final output root at target block differs from canonical")]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_validate_and_submit_root_mismatch(
        #[case] mismatch_block: u64,
        #[case] scenario: &str,
    ) {
        let output_roots = HashMap::from([(mismatch_block, B256::repeat_byte(0xFF))]);
        let pipeline = submit_pipeline(output_roots);
        let proof_result = submit_proof_result(SUBMIT_BLOCK_INTERVAL);

        let result =
            pipeline.validate_and_submit(&proof_result, SUBMIT_BLOCK_INTERVAL, Address::ZERO).await;
        assert!(
            matches!(result, Err(SubmitAction::RootMismatch)),
            "{scenario}: expected RootMismatch, got {result:?}"
        );
    }
}
