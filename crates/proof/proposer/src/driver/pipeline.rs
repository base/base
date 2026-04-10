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
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use alloy_primitives::{Address, B256, Signature, keccak256};
use alloy_sol_types::SolCall;
use base_proof_contracts::{
    AggregateVerifierClient, AnchorStateRegistryClient, DisputeGameFactoryClient, GameInfo,
    ITEEProverRegistry,
};
use base_proof_primitives::{ProofJournal, ProofRequest, ProofResult, ProverClient};
use base_proof_rpc::{L1Provider, L2Provider, RollupProvider};
use eyre::Result;
use futures::{StreamExt, TryStreamExt, stream};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};

use super::core::{DriverConfig, RecoveredState};
use crate::{
    Metrics,
    constants::{MAX_FACTORY_SCAN_LOOKBACK, PROPOSAL_TIMEOUT, RECOVERY_SCAN_CONCURRENCY},
    error::ProposerError,
    output_proposer::{OutputProposer, is_game_already_exists},
};

/// Configuration for the parallel proving pipeline.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Maximum number of concurrent proof tasks.
    pub max_parallel_proofs: usize,
    /// Maximum retries for a single proof range before full pipeline reset.
    pub max_retries: u32,
    /// Base driver configuration.
    pub driver: DriverConfig,
    /// Optional address of the `TEEProverRegistry` contract on L1.
    /// When set, the pipeline validates signers via `isValidSigner` before submission.
    pub tee_prover_registry_address: Option<Address>,
}

/// A game discovered by [`ProvingPipeline::scan_factory_range`].
///
/// Pairs a [`GameInfo`] (from the verifier) with the proxy address
/// (from the factory) so the forward walk has everything it needs.
#[derive(Debug, Clone, Copy)]
struct ScannedGame {
    /// Proxy address of the deployed game contract.
    proxy: Address,
    /// On-chain game details fetched via `game_info`.
    info: GameInfo,
}

/// Cached game map from previous factory scans.
///
/// The factory is append-only, so when `game_count` increases we only scan
/// the new entries (`scanned_up_to..new_count`) and merge them into the
/// existing map. When `game_count` decreases (L1 reorg), the map is rebuilt
/// from scratch.
///
/// The map is separate from the walk result so that anchor-root changes or
/// post-submission re-walks can reuse the map without any factory / `game_info`
/// RPC calls.
#[derive(Debug, Clone)]
struct CachedGameMap {
    /// Factory `game_count` at the time of the last scan.
    scanned_up_to: u64,
    /// `l2_block_number → Vec<ScannedGame>` for games matching our `game_type`.
    map: HashMap<u64, Vec<ScannedGame>>,
}

/// Snapshot of the last successful recovery, combining the cached game map
/// with the walk result.
///
/// The walk result is recomputed (cheaply, from the cached map) whenever
/// the anchor root changes or a new game is added. A full factory rescan
/// only happens on the first startup or after an L1 reorg that reduces
/// `game_count`.
#[derive(Debug, Clone)]
struct CachedRecovery {
    /// Cached factory game map (incrementally updated).
    game_map: CachedGameMap,
    /// The anchor root hash used for the most recent walk.
    anchor_root: B256,
    /// The walk result derived from `game_map` + `anchor_root`.
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
    /// Used by [`super::PipelineHandle`] to create fresh sessions when the
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
                    // On failure / discard the proof stays in `proved`. Retry
                    // can happen from the tick or prove_tasks arms, but those
                    // fire at poll_interval (12s) or proof-completion cadence
                    // (minutes), so the retry rate is naturally bounded.
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

        while cursor <= safe_head
            && !state.inflight.contains(&cursor)
            && !state.proved.contains_key(&cursor)
            && state.submitting != Some(cursor)
            && state.inflight.len() < self.config.max_parallel_proofs
        {
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
                    state.record_gauges();
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
        Ok(())
    }

    fn try_submit(&self, state: &mut PipelineState) {
        if state.submitting.is_some() || !state.submit_tasks.is_empty() {
            return;
        }

        let recovered = match &state.cached_recovery {
            Some(c) => c.state,
            None => return,
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
        let state = match self.recover_latest_state(cache).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "Failed to recover on-chain state, retrying next tick");
                return None;
            }
        };

        let safe_head = match self.latest_safe_block_number().await {
            Ok(n) => n,
            Err(e) => {
                warn!(error = %e, "Failed to fetch safe head, retrying next tick");
                return None;
            }
        };

        Some((state, safe_head))
    }

    /// Recovers the latest on-chain state using a forward walk from the anchor
    /// root.
    ///
    /// # Strategy
    ///
    /// 1. Read `game_count` from the factory and anchor root from the registry
    ///    (2 RPC calls per tick — always needed for cache validation).
    /// 2. **Cache check — fast path.** If both `game_count` and `anchor_root`
    ///    match the cache, return the cached walk result immediately.
    /// 3. **Game map update.** The factory is append-only, so:
    ///    - If `game_count` *increased*, scan only the new entries
    ///      (`cached.scanned_up_to..count`) and merge into the existing map.
    ///    - If `game_count` *decreased* (L1 reorg) or no cache exists, do a
    ///      full scan of the most recent `MAX_FACTORY_SCAN_LOOKBACK` entries.
    /// 4. **Forward walk.** Walk from the anchor block, stepping by
    ///    `block_interval`. For each step, find a game whose
    ///    `parent_address` matches the expected parent, whose
    ///    `root_claim` matches the canonical output root, AND whose
    ///    intermediate output roots all match canonical. Stop at the
    ///    first missing, mismatched, or unchained game.
    ///
    /// The last verified game becomes the parent for the next proposal. If no
    /// games exist, the anchor root is used as the starting point.
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
        let anchor = self.anchor_registry.get_anchor_root().await?;
        let anchor_state_registry_address = self.config.driver.anchor_state_registry_address;

        // Fast path: both game_count and anchor_root unchanged → return
        // cached walk result with zero factory RPCs.
        if let Some(cached) = cache.as_ref()
            && cached.anchor_root == anchor.root
            && cached.game_map.scanned_up_to == count
        {
            debug!(game_count = count, "No changes since last recovery, returning cached state");
            return Ok(cached.state);
        }

        // ── Game map update ─────────────────────────────────────────────
        //
        // Reuse the cached map when possible, scanning only new entries.
        // A full rescan is needed only on cold start or L1 reorg.
        let game_map = self.updated_game_map(cache.as_ref(), count).await?;

        // ── Pre-fetch canonical output roots ───────────────────────────
        //
        // Compute the set of block numbers the forward walk *could* visit
        // (consecutive strides from the anchor that have games in the map),
        // then fetch all their canonical output roots concurrently. The walk
        // itself becomes purely in-memory lookups against this pre-fetched
        // map, eliminating O(N) sequential RPCs.
        //
        // In addition to the game block numbers themselves, intermediate
        // block numbers are included so that each game's intermediate output
        // roots can be verified against the canonical chain.
        let block_interval = self.config.driver.block_interval;
        let intermediate_block_interval = self.config.driver.intermediate_block_interval;
        let intermediate_count = block_interval / intermediate_block_interval;

        let prefetch_blocks: Vec<u64> = {
            let mut blocks = Vec::new();
            let mut block = anchor.l2_block_number;
            while let Some(next) = block.checked_add(block_interval) {
                if game_map.map.contains_key(&next) {
                    blocks.push(next);
                    block = next;
                } else {
                    // The walk cannot continue past a missing block.
                    break;
                }
            }
            blocks
        };

        // Expand to include intermediate block numbers for each game block.
        // For a game at block B with parent at B - block_interval, the
        // intermediate blocks are at parent + i * intermediate_block_interval
        // for i in 1..=intermediate_count. The last one (i = count) equals B
        // itself, so the set is a superset of `prefetch_blocks`.
        let all_canonical_blocks: Vec<u64> = {
            let mut blocks = HashSet::new();
            for &game_block in &prefetch_blocks {
                let parent = game_block - block_interval;
                for i in 1..=intermediate_count {
                    blocks.insert(parent + i * intermediate_block_interval);
                }
            }
            blocks.into_iter().collect()
        };

        let canonical_roots: HashMap<u64, B256> = if all_canonical_blocks.is_empty() {
            HashMap::new()
        } else {
            debug!(
                blocks = all_canonical_blocks.len(),
                game_blocks = prefetch_blocks.len(),
                intermediate_count,
                "Pre-fetching canonical output roots concurrently"
            );
            stream::iter(all_canonical_blocks)
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
                .buffered(RECOVERY_SCAN_CONCURRENCY)
                .try_collect()
                .await?
        };

        // ── Pre-fetch intermediate output roots from game contracts ─────
        //
        // For every game proxy that the walk could visit, fetch its on-chain
        // intermediate output roots concurrently. These will be compared
        // against the canonical roots during the walk to verify the game's
        // intermediate state commitments.
        let walk_proxies: Vec<Address> = prefetch_blocks
            .iter()
            .filter_map(|b| game_map.map.get(b))
            .flat_map(|games| games.iter().map(|g| g.proxy))
            .collect();

        let intermediate_roots_map: HashMap<Address, Vec<B256>> = if walk_proxies.is_empty() {
            HashMap::new()
        } else {
            debug!(
                proxies = walk_proxies.len(),
                "Pre-fetching intermediate output roots from game contracts"
            );
            stream::iter(walk_proxies)
                .map(|proxy| {
                    let verifier = &self.verifier_client;
                    async move {
                        verifier
                            .intermediate_output_roots(proxy)
                            .await
                            .map(|roots| (proxy, roots))
                            .map_err(|e| {
                                ProposerError::Contract(format!(
                                    "intermediate_output_roots failed for proxy {proxy}: {e}"
                                ))
                            })
                    }
                })
                .buffered(RECOVERY_SCAN_CONCURRENCY)
                .try_collect()
                .await?
        };

        // ── Forward walk ────────────────────────────────────────────────
        //
        // Walk from the anchor root, verifying parent-chain linkage,
        // output root correctness, and intermediate output root
        // correctness at each step. All output root lookups are served
        // from the pre-fetched `canonical_roots` map, and intermediate
        // roots from the pre-fetched `intermediate_roots_map`.
        let mut parent_address = anchor_state_registry_address;
        let mut parent_output_root = anchor.root;
        let mut parent_block = anchor.l2_block_number;
        let mut steps: u64 = 0;

        while let Some(expected_block) = parent_block.checked_add(block_interval) {
            // Look up the pre-fetched canonical root. A missing entry means
            // the block had no game in the map (the pre-fetch stopped there).
            let canonical_root = match canonical_roots.get(&expected_block) {
                Some(root) => *root,
                None => {
                    info!(
                        gap_block = expected_block,
                        parent_block,
                        parent_address = %parent_address,
                        games_verified = steps,
                        "Found first missing game, will propose from here"
                    );
                    break;
                }
            };

            // Look up all games at this block number, then find one whose
            // parent_address matches the expected parent.
            let candidates = match game_map.map.get(&expected_block) {
                Some(c) => c,
                None => {
                    // No game exists for this block number — this is the gap.
                    info!(
                        gap_block = expected_block,
                        parent_block,
                        parent_address = %parent_address,
                        games_verified = steps,
                        "Found first missing game, will propose from here"
                    );
                    break;
                }
            };

            // Filter to candidates that reference our expected parent.
            let mut matching =
                candidates.iter().filter(|g| g.info.parent_address == parent_address);

            let first = match matching.next() {
                Some(g) => g,
                None => {
                    warn!(
                        l2_block_number = expected_block,
                        expected_parent = %parent_address,
                        candidates = candidates.len(),
                        "No game at block has correct parent_address, treating as gap"
                    );
                    break;
                }
            };

            if matching.next().is_some() {
                warn!(
                    l2_block_number = expected_block,
                    expected_parent = %parent_address,
                    "Multiple games with same parent at block, using first"
                );
            }

            let ScannedGame { proxy, info } = *first;

            // Verify the root claim matches the canonical output root.
            if canonical_root != info.root_claim {
                warn!(
                    l2_block_number = expected_block,
                    game_proxy = %proxy,
                    onchain_root = ?info.root_claim,
                    canonical_root = ?canonical_root,
                    "Output root mismatch during forward walk, treating as gap"
                );
                break;
            }

            // Verify intermediate output roots against canonical.
            //
            // Each game commits to intermediate output roots at every
            // `intermediate_block_interval` blocks between its parent and
            // itself. A mismatch indicates the game could be challenged,
            // so we treat it as a gap to avoid chaining off an invalid parent.
            let onchain_intermediate = intermediate_roots_map.get(&proxy).ok_or_else(|| {
                ProposerError::Internal(format!(
                    "missing pre-fetched intermediate roots for proxy {proxy}"
                ))
            })?;

            if onchain_intermediate.len() as u64 != intermediate_count {
                warn!(
                    l2_block_number = expected_block,
                    game_proxy = %proxy,
                    expected = intermediate_count,
                    actual = onchain_intermediate.len(),
                    "Unexpected intermediate root count, treating as gap"
                );
                break;
            }

            // The last intermediate root must equal root_claim (enforced
            // on-chain by the contract). Verify this consistency invariant
            // to catch any divergence between intermediateOutputRoots() and
            // rootClaim(). This also makes the intermediate_count == 1 path
            // non-trivial (where all intermediate blocks are skipped below).
            if onchain_intermediate.last() != Some(&info.root_claim) {
                warn!(
                    l2_block_number = expected_block,
                    game_proxy = %proxy,
                    last_intermediate = ?onchain_intermediate.last(),
                    root_claim = ?info.root_claim,
                    "Last intermediate root does not match root_claim, treating as gap"
                );
                break;
            }

            let mut intermediate_mismatch = false;
            for (i, onchain_root) in onchain_intermediate.iter().enumerate() {
                let intermediate_block = (expected_block - block_interval)
                    + (i as u64 + 1) * intermediate_block_interval;

                // The last intermediate root equals root_claim, already
                // verified above — skip the redundant check.
                if intermediate_block == expected_block {
                    continue;
                }

                let canonical = canonical_roots.get(&intermediate_block).ok_or_else(|| {
                    ProposerError::Internal(format!(
                        "missing canonical root for intermediate block {intermediate_block}"
                    ))
                })?;

                if *onchain_root != *canonical {
                    warn!(
                        l2_block_number = expected_block,
                        intermediate_block,
                        intermediate_index = i,
                        game_proxy = %proxy,
                        onchain_root = ?onchain_root,
                        canonical_root = ?canonical,
                        "Intermediate root mismatch during forward walk, treating as gap"
                    );
                    intermediate_mismatch = true;
                    break;
                }
            }

            if intermediate_mismatch {
                break;
            }

            debug!(
                l2_block_number = expected_block,
                game_proxy = %proxy,
                step = steps,
                "Game exists onchain, continuing forward"
            );

            parent_address = proxy;
            parent_output_root = info.root_claim;
            parent_block = expected_block;
            steps += 1;
        }

        if steps > 0 {
            info!(
                latest_block = parent_block,
                parent_address = %parent_address,
                games_verified = steps,
                "Recovery forward walk complete"
            );
        }

        let state = RecoveredState {
            parent_address,
            output_root: parent_output_root,
            l2_block_number: parent_block,
        };

        *cache = Some(CachedRecovery { game_map, anchor_root: anchor.root, state });

        Ok(state)
    }

    /// Returns an up-to-date game map, reusing the cached map when possible.
    ///
    /// - **Cold start / reorg (count decreased):** Full scan of the most
    ///   recent `MAX_FACTORY_SCAN_LOOKBACK` entries.
    /// - **Incremental (count increased):** Scan only the new entries
    ///   (`cached.scanned_up_to..count`) and merge into the existing map.
    /// - **Anchor root changed (count unchanged):** Reuse the existing map
    ///   as-is — no factory RPCs needed.
    async fn updated_game_map(
        &self,
        cache: Option<&CachedRecovery>,
        count: u64,
    ) -> Result<CachedGameMap, ProposerError> {
        match cache {
            Some(cached) if count >= cached.game_map.scanned_up_to => {
                let new_entries = count - cached.game_map.scanned_up_to;
                if new_entries == 0 {
                    // Anchor root changed but game_count is the same —
                    // reuse the map, just re-walk.
                    debug!("Anchor root changed, re-walking with existing game map");
                    return Ok(cached.game_map.clone());
                }

                // If the delta exceeds the lookback window (e.g. proposer
                // was offline for an extended period), fall back to a full
                // scan rather than issuing an unbounded number of RPCs.
                if new_entries > MAX_FACTORY_SCAN_LOOKBACK {
                    warn!(
                        new_entries,
                        max = MAX_FACTORY_SCAN_LOOKBACK,
                        "Incremental delta exceeds lookback, falling back to full scan"
                    );
                    return self.full_scan(count).await;
                }

                // Incremental scan: only fetch the new factory entries.
                info!(
                    cached_count = cached.game_map.scanned_up_to,
                    current_count = count,
                    new_entries,
                    "Incrementally scanning new factory entries"
                );
                let mut map = cached.game_map.map.clone();
                self.scan_factory_range(cached.game_map.scanned_up_to, count, &mut map).await?;
                Ok(CachedGameMap { scanned_up_to: count, map })
            }
            Some(cached) => {
                // game_count decreased — L1 reorg. Full rescan needed
                // because we can't know which entries were removed.
                warn!(
                    cached_count = cached.game_map.scanned_up_to,
                    current_count = count,
                    "Game count decreased (possible L1 reorg), performing full rescan"
                );
                self.full_scan(count).await
            }
            None => {
                // Cold start — no cache exists.
                info!(game_count = count, "Cold start, performing full factory scan");
                self.full_scan(count).await
            }
        }
    }

    /// Performs a full factory scan of the most recent
    /// [`MAX_FACTORY_SCAN_LOOKBACK`] entries and returns a fresh
    /// [`CachedGameMap`].
    async fn full_scan(&self, count: u64) -> Result<CachedGameMap, ProposerError> {
        let search_count = count.min(MAX_FACTORY_SCAN_LOOKBACK);
        let start_index = count.saturating_sub(search_count);
        let mut map = HashMap::new();
        self.scan_factory_range(start_index, count, &mut map).await?;
        Ok(CachedGameMap { scanned_up_to: count, map })
    }

    /// Scans factory entries in `start_index..end_index` and inserts matching
    /// games into `game_map`.
    ///
    /// Only games whose `game_type` matches ours are fetched via `game_info`.
    ///
    /// Under normal operation, at most one game exists per block number
    /// because each game requires a valid cryptographic proof at creation
    /// time and the factory rejects duplicate `(gameType, rootClaim,
    /// extraData)` tuples. Multiple games at the same block number can
    /// only occur in exceptional circumstances (prover soundness bug, L2
    /// reorg between competing submissions, or compromised TEE signer).
    /// All are retained so the forward walk can select the correct one by
    /// validating the parent chain.
    ///
    /// Uses concurrent RPC calls via [`futures::stream::StreamExt::buffered`]
    /// with [`RECOVERY_SCAN_CONCURRENCY`] parallelism.
    async fn scan_factory_range(
        &self,
        start_index: u64,
        end_index: u64,
        game_map: &mut HashMap<u64, Vec<ScannedGame>>,
    ) -> Result<(), ProposerError> {
        if start_index >= end_index {
            return Ok(());
        }

        let game_type = self.config.driver.game_type;
        let scan_count = end_index - start_index;

        debug!(scan_count, start_index, end_index, "Scanning factory range");

        // Fetch game_at_index concurrently, propagating RPC errors so a
        // transient failure doesn't silently drop a game and create a false
        // gap in the forward walk. Non-matching game types are filtered after
        // error propagation.
        let all_games: Vec<_> = stream::iter(start_index..end_index)
            .map(|i| {
                let factory = &self.factory_client;
                async move {
                    factory.game_at_index(i).await.map_err(|e| {
                        ProposerError::Contract(format!("game_at_index failed for index {i}: {e}"))
                    })
                }
            })
            .buffered(RECOVERY_SCAN_CONCURRENCY)
            .try_collect()
            .await?;

        let matching_games: Vec<_> = all_games
            .into_iter()
            .enumerate()
            .filter_map(|(offset, game)| {
                (game.game_type == game_type).then_some((start_index + offset as u64, game))
            })
            .collect();

        let game_infos: Vec<ScannedGame> = stream::iter(matching_games)
            .map(|(game_index, game)| {
                let verifier = &self.verifier_client;
                async move {
                    match verifier.game_info(game.proxy).await {
                        Ok(info) => Ok(ScannedGame { proxy: game.proxy, info }),
                        Err(e) => {
                            // Propagate game_info failures — a transient RPC
                            // error must not be silently treated as a missing
                            // game, which would cause the forward walk to see
                            // a false gap and re-propose from an earlier point.
                            Err(ProposerError::Contract(format!(
                                "game_info failed for proxy {game_proxy} \
                                 (factory index {game_index}): {e}",
                                game_proxy = game.proxy,
                            )))
                        }
                    }
                }
            })
            .buffered(RECOVERY_SCAN_CONCURRENCY)
            .try_collect()
            .await?;

        let new_games = game_infos.len();
        for scanned in game_infos {
            game_map.entry(scanned.info.l2_block_number).or_default().push(scanned);
        }

        debug!(new_games, "Factory range scan complete");

        Ok(())
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

    async fn build_proof_request_for(
        &self,
        starting_block_number: u64,
        agreed_output_root: B256,
        target_block: u64,
    ) -> Result<ProofRequest, ProposerError> {
        let agreed_l2_head = self
            .l2_client
            .header_by_number(Some(starting_block_number))
            .await
            .map_err(ProposerError::Rpc)?;

        let claimed_output =
            self.rollup_client.output_at_block(target_block).await.map_err(ProposerError::Rpc)?;

        let l1_head = self.l1_client.header_by_number(None).await.map_err(ProposerError::Rpc)?;

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
        let intermediate_roots = self
            .extract_intermediate_roots(starting_block_number, proposals)
            .map_err(SubmitAction::Failed)?;

        // JIT validation: check that each intermediate output root matches
        // the canonical chain.  This catches TEE prover bugs or reorgs that
        // occurred between proving and submission.
        let interval = self.config.driver.intermediate_block_interval;
        for (i, root) in intermediate_roots.iter().enumerate() {
            let block = starting_block_number
                .checked_add((i as u64 + 1).checked_mul(interval).ok_or_else(|| {
                    SubmitAction::Failed(ProposerError::Internal(
                        "overflow computing intermediate root block number".into(),
                    ))
                })?)
                .ok_or_else(|| {
                    SubmitAction::Failed(ProposerError::Internal(
                        "overflow computing intermediate root block number".into(),
                    ))
                })?;

            let canonical = self
                .rollup_client
                .output_at_block(block)
                .await
                .map_err(|e| SubmitAction::Failed(ProposerError::Rpc(e)))?;

            if *root != canonical.output_root {
                warn!(
                    intermediate_block = block,
                    proposal_root = ?root,
                    canonical_root = ?canonical.output_root,
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
                target_block,
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
                if is_game_already_exists(&e) {
                    drop(propose_timer);
                    info!(
                        target_block,
                        "Game already exists, next tick will load fresh state from chain"
                    );
                    Ok(())
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

    /// Extracts intermediate output roots from per-block proposals.
    ///
    /// Samples at every `intermediate_block_interval` within the range.
    fn extract_intermediate_roots(
        &self,
        starting_block_number: u64,
        proposals: &[base_proof_primitives::Proposal],
    ) -> Result<Vec<B256>, ProposerError> {
        let interval = self.config.driver.intermediate_block_interval;
        if interval == 0 {
            return Err(ProposerError::Config(
                "intermediate_block_interval must not be zero".into(),
            ));
        }
        let count = self.config.driver.block_interval / interval;
        let mut roots = Vec::with_capacity(count as usize);
        for i in 1..=count {
            let target_block = starting_block_number
                .checked_add(i.checked_mul(interval).ok_or_else(|| {
                    ProposerError::Internal("overflow computing intermediate root target".into())
                })?)
                .ok_or_else(|| {
                    ProposerError::Internal("overflow computing intermediate root target".into())
                })?;

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
            Self::Failed(e) | Self::Discard(e) => write!(f, "{e}"),
        }
    }
}

/// Result of a concurrent submission task, returned to the coordinator.
enum SubmitOutcome {
    Success { target_block: u64 },
    RootMismatch { target_block: u64 },
    Failed { target_block: u64, proof: ProofResult, error: ProposerError },
    Discard { target_block: u64, error: ProposerError },
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, time::Duration};

    use alloy_primitives::{Address, B256, Bytes};
    use async_trait::async_trait;
    use base_proof_contracts::{GameAtIndex, GameInfo};
    use base_proof_primitives::{ProofResult, Proposal, ProverClient};
    use rstest::rstest;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
        MockOutputProposer, MockRollupClient, test_anchor_root, test_sync_status,
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

    fn test_proposal(block_number: u64) -> Proposal {
        Proposal {
            output_root: B256::repeat_byte(block_number as u8),
            signature: Bytes::from(vec![0xab; 65]),
            l1_origin_hash: B256::repeat_byte(0x02),
            l1_origin_number: 100 + block_number,
            l2_block_number: block_number,
            prev_output_root: B256::repeat_byte(0x03),
            config_hash: B256::repeat_byte(0x04),
        }
    }

    /// A mock prover that returns immediately with a configurable delay.
    #[derive(Debug)]
    struct MockProver {
        delay: Duration,
    }

    #[async_trait]
    impl ProverClient for MockProver {
        async fn prove(
            &self,
            request: base_proof_primitives::ProofRequest,
        ) -> Result<ProofResult, Box<dyn std::error::Error + Send + Sync>> {
            tokio::time::sleep(self.delay).await;

            let block_number = request.claimed_l2_block_number;
            let aggregate_proposal = test_proposal(block_number);

            // Generate per-block proposals.
            let start = block_number.saturating_sub(TEST_BLOCK_INTERVAL);
            let proposals: Vec<Proposal> =
                ((start + 1)..=block_number).map(test_proposal).collect();

            Ok(ProofResult::Tee { aggregate_proposal, proposals })
        }
    }

    // ---- Helper builders for game data ----

    /// Builds a single `GameAtIndex` entry.
    fn game_entry(game_type: u32, index: u64) -> GameAtIndex {
        GameAtIndex { game_type, timestamp: index + 1, proxy: proxy_addr(index) }
    }

    /// Builds a chain of `N` sequential games starting from the anchor.
    ///
    /// Returns `(factory_games, info_map, output_roots)` ready to pass to
    /// [`recovery_pipeline_with_roots`].
    fn game_chain(n: usize) -> (Vec<GameAtIndex>, HashMap<Address, GameInfo>, HashMap<u64, B256>) {
        let mut games = Vec::with_capacity(n);
        let mut info_map = HashMap::with_capacity(n);
        let mut output_roots = HashMap::with_capacity(n);

        let mut parent = Address::ZERO; // anchor_state_registry_address default
        for i in 0..n {
            let block = TEST_BLOCK_INTERVAL * (i as u64 + 1);
            let proxy = proxy_addr(i as u64);
            let info = GameInfo {
                root_claim: B256::repeat_byte((block / TEST_BLOCK_INTERVAL) as u8),
                l2_block_number: block,
                parent_address: parent,
            };

            games.push(game_entry(TEST_GAME_TYPE, i as u64));
            output_roots.insert(block, info.root_claim);
            info_map.insert(proxy, info);

            parent = proxy;
        }
        (games, info_map, output_roots)
    }

    // ---- Pipeline builders ----

    /// Helper: unique proxy address derived from an index.
    fn proxy_addr(index: u64) -> Address {
        let mut bytes = [0u8; 20];
        bytes[12..20].copy_from_slice(&index.to_be_bytes());
        Address::new(bytes)
    }

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
        let prover: Arc<dyn ProverClient> =
            Arc::new(MockProver { delay: Duration::from_millis(10) });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(safe_block_number, B256::ZERO),
            output_roots: HashMap::new(),
        });
        let anchor_registry =
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(TEST_ANCHOR_BLOCK) });
        let factory = Arc::new(MockDisputeGameFactory::with_count(0));

        ProvingPipeline::new(
            pipeline_config,
            prover,
            l1,
            l2,
            rollup,
            anchor_registry,
            factory,
            Arc::new(MockAggregateVerifier::empty()),
            Arc::new(MockOutputProposer),
            cancel,
        )
    }

    /// Builds a recovery pipeline with factory games, verifier info, and
    /// canonical output roots. Uses default anchor block and block interval.
    fn recovery_pipeline_with_roots(
        factory: MockDisputeGameFactory,
        verifier: MockAggregateVerifier,
        output_roots: HashMap<u64, B256>,
    ) -> TestPipeline {
        recovery_pipeline_full(
            factory,
            verifier,
            output_roots,
            TEST_ANCHOR_BLOCK,
            TEST_BLOCK_INTERVAL,
            TEST_BLOCK_INTERVAL,
        )
    }

    fn recovery_pipeline_full(
        factory: MockDisputeGameFactory,
        verifier: MockAggregateVerifier,
        output_roots: HashMap<u64, B256>,
        anchor_block: u64,
        block_interval: u64,
        intermediate_block_interval: u64,
    ) -> TestPipeline {
        let cancel = CancellationToken::new();
        let l1 = Arc::new(MockL1 { latest_block_number: TEST_L1_BLOCK_NUMBER });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let prover: Arc<dyn ProverClient> = Arc::new(MockProver { delay: MOCK_PROVER_DELAY });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(0, B256::ZERO),
            output_roots,
        });
        let anchor_registry =
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(anchor_block) });

        ProvingPipeline::new(
            PipelineConfig {
                max_parallel_proofs: 1,
                max_retries: 1,
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
            Arc::new(verifier),
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

    // ---- Recovery: anchor / empty factory ----

    #[rstest]
    #[case::no_games(vec![], "empty factory")]
    #[case::no_type_match(
        vec![
            GameAtIndex { game_type: 99, timestamp: 1, proxy: proxy_addr(0) },
            GameAtIndex { game_type: 100, timestamp: 2, proxy: proxy_addr(1) },
        ],
        "games exist but none match our type"
    )]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_returns_anchor_when_no_usable_games(
        #[case] games: Vec<GameAtIndex>,
        #[case] scenario: &str,
    ) {
        let factory = if games.is_empty() {
            MockDisputeGameFactory::with_count(0)
        } else {
            MockDisputeGameFactory::with_games(games)
        };
        let pipeline =
            recovery_pipeline_with_roots(factory, MockAggregateVerifier::empty(), HashMap::new());

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(
            state.parent_address,
            Address::ZERO,
            "{scenario}: should return anchor_state_registry_address"
        );
        assert_eq!(
            state.l2_block_number, TEST_ANCHOR_BLOCK,
            "{scenario}: should return anchor block"
        );
        assert!(cache.is_some(), "{scenario}: cache should still be populated");
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
        let (games, info_map, output_roots) = game_chain(game_count);

        let pipeline = recovery_pipeline_with_roots(
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(expected_proxy_index), "{scenario}");
        assert_eq!(state.l2_block_number, expected_block, "{scenario}");
        assert!(cache.is_some(), "{scenario}: cache should be populated");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_forward_walk_stops_at_gap() {
        let root_1 = B256::repeat_byte(0x01);
        let root_skip = B256::repeat_byte(0x03);

        // Games at blocks 512 and 1536 (missing 1024) — gap stops the walk.
        let games = vec![game_entry(TEST_GAME_TYPE, 0), game_entry(TEST_GAME_TYPE, 2)];

        let info_map = HashMap::from([
            (
                proxy_addr(0),
                GameInfo {
                    root_claim: root_1,
                    l2_block_number: TEST_BLOCK_INTERVAL,
                    parent_address: Address::ZERO,
                },
            ),
            (
                proxy_addr(2),
                GameInfo {
                    root_claim: root_skip,
                    l2_block_number: TEST_BLOCK_INTERVAL * 3,
                    parent_address: proxy_addr(0),
                },
            ),
        ]);

        let output_roots =
            HashMap::from([(TEST_BLOCK_INTERVAL, root_1), (TEST_BLOCK_INTERVAL * 3, root_skip)]);

        let pipeline = recovery_pipeline_with_roots(
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(0), "should stop at first game before gap");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(state.output_root, root_1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_forward_walk_stops_at_root_mismatch() {
        let valid_root = B256::repeat_byte(0xBB);
        let bad_onchain_root = B256::repeat_byte(0xDE);
        let canonical_root_at_1024 = B256::repeat_byte(0xAB);

        let games = vec![game_entry(TEST_GAME_TYPE, 0), game_entry(TEST_GAME_TYPE, 1)];

        let info_map = HashMap::from([
            (
                proxy_addr(0),
                GameInfo {
                    root_claim: valid_root,
                    l2_block_number: TEST_BLOCK_INTERVAL,
                    parent_address: Address::ZERO,
                },
            ),
            (
                proxy_addr(1),
                GameInfo {
                    root_claim: bad_onchain_root,
                    l2_block_number: TEST_BLOCK_INTERVAL * 2,
                    parent_address: proxy_addr(0),
                },
            ),
        ]);

        let output_roots = HashMap::from([
            (TEST_BLOCK_INTERVAL, valid_root),
            (TEST_BLOCK_INTERVAL * 2, canonical_root_at_1024),
        ]);

        let pipeline = recovery_pipeline_with_roots(
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(0), "should stop before root mismatch");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(state.output_root, valid_root);
    }

    // ---- Recovery: scan resilience (game_info failure propagation) ----

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_propagates_game_info_failures() {
        // 3 games in the factory. The verifier fails for the game at block 512.
        // This must propagate as an error (not silently skip), so the tick
        // retries on the next interval rather than treating it as a false gap.
        let root_2 = B256::repeat_byte(0x02);
        let root_3 = B256::repeat_byte(0x03);

        let games = vec![
            game_entry(TEST_GAME_TYPE, 0),
            game_entry(TEST_GAME_TYPE, 1),
            game_entry(TEST_GAME_TYPE, 2),
        ];

        let info_map = HashMap::from([
            (
                proxy_addr(1),
                GameInfo {
                    root_claim: root_2,
                    l2_block_number: TEST_BLOCK_INTERVAL * 2,
                    parent_address: proxy_addr(0),
                },
            ),
            (
                proxy_addr(2),
                GameInfo {
                    root_claim: root_3,
                    l2_block_number: TEST_BLOCK_INTERVAL * 3,
                    parent_address: proxy_addr(1),
                },
            ),
        ]);

        let mut verifier = MockAggregateVerifier::with_game_info(info_map);
        verifier.failing_addresses.insert(proxy_addr(0));

        let output_roots =
            HashMap::from([(TEST_BLOCK_INTERVAL * 2, root_2), (TEST_BLOCK_INTERVAL * 3, root_3)]);

        let pipeline = recovery_pipeline_with_roots(
            MockDisputeGameFactory::with_games(games),
            verifier,
            output_roots,
        );

        let mut cache: Option<CachedRecovery> = None;
        let result = pipeline.recover_latest_state(&mut cache).await;

        assert!(result.is_err(), "game_info failure should propagate as error");
        assert!(matches!(result, Err(ProposerError::Contract(_))), "should be a Contract error");
        assert!(cache.is_none(), "cache should not be populated on error");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_propagates_game_at_index_failures() {
        // Factory reports game_count=3 but only has 2 entries. The third
        // game_at_index call returns an error. This must propagate (not
        // silently skip), so the tick retries rather than treating the
        // missing index as a false gap.
        let games = vec![game_entry(TEST_GAME_TYPE, 0), game_entry(TEST_GAME_TYPE, 1)];
        let mut factory = MockDisputeGameFactory::with_games(games);
        factory.game_count_override = Some(3); // one more than actual entries

        let pipeline =
            recovery_pipeline_with_roots(factory, MockAggregateVerifier::empty(), HashMap::new());

        let mut cache: Option<CachedRecovery> = None;
        let result = pipeline.recover_latest_state(&mut cache).await;

        assert!(result.is_err(), "game_at_index failure should propagate as error");
        assert!(matches!(result, Err(ProposerError::Contract(_))), "should be a Contract error");
        assert!(cache.is_none(), "cache should not be populated on error");
    }

    // ---- Recovery: caching ----

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_hit_equal_game_count() {
        let (games, info_map, output_roots) = game_chain(1);
        let game_proxy = proxy_addr(0);

        let pipeline = recovery_pipeline_with_roots(
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        // First call: cold start, populates the cache.
        let mut cache: Option<CachedRecovery> = None;
        let state1 = pipeline.recover_latest_state(&mut cache).await.unwrap();
        assert!(cache.is_some(), "cache should be populated after first call");
        assert_eq!(state1.parent_address, game_proxy);
        assert_eq!(state1.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(cache.as_ref().unwrap().game_map.scanned_up_to, 1);

        // Second call: same game_count → cached state returned without re-scan.
        let state2 = pipeline.recover_latest_state(&mut cache).await.unwrap();
        assert_eq!(state2.parent_address, state1.parent_address);
        assert_eq!(state2.l2_block_number, state1.l2_block_number);
        assert_eq!(state2.output_root, state1.output_root);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_incremental_on_count_increase() {
        // Seed the cache with 1 game already scanned. Factory now has 2
        // games. The incremental scan should pick up only the new entry
        // (index 1) and the walk should find both games.
        let (games, info_map, output_roots) = game_chain(2);

        // Build a cached map containing just the first game.
        let first_info = info_map[&proxy_addr(0)];
        let cached_map = HashMap::from([(
            TEST_BLOCK_INTERVAL,
            vec![ScannedGame { proxy: proxy_addr(0), info: first_info }],
        )]);

        let mut cache = Some(CachedRecovery {
            game_map: CachedGameMap { scanned_up_to: 1, map: cached_map },
            anchor_root: B256::ZERO,
            state: RecoveredState {
                parent_address: proxy_addr(99), // stale — will be recomputed
                output_root: B256::repeat_byte(0xDD),
                l2_block_number: TEST_BLOCK_INTERVAL,
            },
        });

        let pipeline = recovery_pipeline_with_roots(
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(1), "should walk through both games");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL * 2);
        assert_eq!(
            cache.as_ref().unwrap().game_map.scanned_up_to,
            2,
            "cache should reflect new count"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_full_rescan_on_count_decrease() {
        // Seed the cache with scanned_up_to=5. Factory now has only 1
        // game (reorg). This triggers a full rescan since count decreased.
        let (games, info_map, output_roots) = game_chain(1);

        let mut cache = Some(CachedRecovery {
            game_map: CachedGameMap { scanned_up_to: 5, map: HashMap::new() },
            anchor_root: B256::ZERO,
            state: RecoveredState {
                parent_address: proxy_addr(99),
                output_root: B256::repeat_byte(0xDD),
                l2_block_number: 5 * TEST_BLOCK_INTERVAL,
            },
        });

        let pipeline = recovery_pipeline_with_roots(
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(0), "reorg: should find the 1 remaining game");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(
            cache.as_ref().unwrap().game_map.scanned_up_to,
            1,
            "reorg: cache should reflect new count"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_incremental_filters_non_matching_types() {
        // Cache has 1 game. Factory now has 3 entries total, but the 2 new
        // entries have a different game_type. The incremental scan should
        // filter them out and the walk result should remain at the first game.
        let (games_1, info_map_1, output_roots) = game_chain(1);
        let first_info = info_map_1[&proxy_addr(0)];

        // Build factory with 3 entries: index 0 is our type, 1-2 are other types.
        let factory_games = vec![
            games_1[0],
            GameAtIndex { game_type: 99, timestamp: 2, proxy: proxy_addr(1) },
            GameAtIndex { game_type: 100, timestamp: 3, proxy: proxy_addr(2) },
        ];

        let cached_map = HashMap::from([(
            TEST_BLOCK_INTERVAL,
            vec![ScannedGame { proxy: proxy_addr(0), info: first_info }],
        )]);

        let mut cache = Some(CachedRecovery {
            game_map: CachedGameMap { scanned_up_to: 1, map: cached_map },
            anchor_root: B256::ZERO,
            state: RecoveredState {
                parent_address: proxy_addr(0),
                output_root: first_info.root_claim,
                l2_block_number: TEST_BLOCK_INTERVAL,
            },
        });

        let pipeline = recovery_pipeline_with_roots(
            MockDisputeGameFactory::with_games(factory_games),
            MockAggregateVerifier::with_game_info(info_map_1),
            output_roots,
        );

        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        // Walk should still stop at the first game since no new matching games.
        assert_eq!(state.parent_address, proxy_addr(0));
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(
            cache.as_ref().unwrap().game_map.scanned_up_to,
            3,
            "scanned_up_to should advance even when no matching games found"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_incremental_multiple_new_entries() {
        // Cache has 1 game. Factory now has 4 games total (3 new entries).
        // The incremental scan picks up entries 1-3 and the walk finds all 4.
        let (games, info_map, output_roots) = game_chain(4);
        let first_info = info_map[&proxy_addr(0)];

        let cached_map = HashMap::from([(
            TEST_BLOCK_INTERVAL,
            vec![ScannedGame { proxy: proxy_addr(0), info: first_info }],
        )]);

        let mut cache = Some(CachedRecovery {
            game_map: CachedGameMap { scanned_up_to: 1, map: cached_map },
            anchor_root: B256::ZERO,
            state: RecoveredState {
                parent_address: proxy_addr(0),
                output_root: first_info.root_claim,
                l2_block_number: TEST_BLOCK_INTERVAL,
            },
        });

        let pipeline = recovery_pipeline_with_roots(
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, proxy_addr(3), "should walk through all 4 games");
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL * 4);
        assert_eq!(
            cache.as_ref().unwrap().game_map.scanned_up_to,
            4,
            "cache should reflect all 4 entries scanned"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_grows_across_sequential_ticks() {
        // Simulate 3 sequential ticks where 1 game is added each tick.
        // Tick 1: cold start, 1 game. Tick 2: +1 game. Tick 3: +1 game.
        // The cache's game_map should grow incrementally across ticks.
        let (all_games, all_info, all_roots) = game_chain(3);

        // ---- Tick 1: 1 game exists ----
        let pipeline_t1 = recovery_pipeline_with_roots(
            MockDisputeGameFactory::with_games(all_games[..1].to_vec()),
            MockAggregateVerifier::with_game_info(all_info.clone()),
            all_roots.clone(),
        );
        let mut cache: Option<CachedRecovery> = None;
        let state1 = pipeline_t1.recover_latest_state(&mut cache).await.unwrap();
        assert_eq!(state1.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(cache.as_ref().unwrap().game_map.scanned_up_to, 1);

        // ---- Tick 2: 2 games exist ----
        let pipeline_t2 = recovery_pipeline_with_roots(
            MockDisputeGameFactory::with_games(all_games[..2].to_vec()),
            MockAggregateVerifier::with_game_info(all_info.clone()),
            all_roots.clone(),
        );
        let state2 = pipeline_t2.recover_latest_state(&mut cache).await.unwrap();
        assert_eq!(state2.l2_block_number, TEST_BLOCK_INTERVAL * 2);
        assert_eq!(cache.as_ref().unwrap().game_map.scanned_up_to, 2);
        assert_eq!(
            cache.as_ref().unwrap().game_map.map.len(),
            2,
            "map should contain entries for 2 distinct block numbers"
        );

        // ---- Tick 3: 3 games exist ----
        let pipeline_t3 = recovery_pipeline_with_roots(
            MockDisputeGameFactory::with_games(all_games),
            MockAggregateVerifier::with_game_info(all_info),
            all_roots,
        );
        let state3 = pipeline_t3.recover_latest_state(&mut cache).await.unwrap();
        assert_eq!(state3.l2_block_number, TEST_BLOCK_INTERVAL * 3);
        assert_eq!(state3.parent_address, proxy_addr(2));
        assert_eq!(cache.as_ref().unwrap().game_map.scanned_up_to, 3);
        assert_eq!(
            cache.as_ref().unwrap().game_map.map.len(),
            3,
            "map should contain entries for 3 distinct block numbers"
        );
    }

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
            game_map: CachedGameMap { scanned_up_to: 10, map: HashMap::new() },
            anchor_root: B256::ZERO,
            state: RecoveredState {
                parent_address: proxy_addr(5),
                output_root: B256::repeat_byte(0x11),
                l2_block_number: TEST_BLOCK_INTERVAL,
            },
        });

        state.reset();
        assert!(state.cached_recovery.is_none(), "reset() should clear cached_recovery");
    }

    // ---- Recovery: parent chain validation (C1) ----

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_rejects_game_with_wrong_parent() {
        // Two games exist at blocks 512 and 1024, but the second game's
        // parent_address does NOT point to the first game's proxy. The
        // forward walk should stop after the first game.
        let root_1 = B256::repeat_byte(0x01);
        let root_2 = B256::repeat_byte(0x02);

        let games = vec![game_entry(TEST_GAME_TYPE, 0), game_entry(TEST_GAME_TYPE, 1)];

        let info_map = HashMap::from([
            (
                proxy_addr(0),
                GameInfo {
                    root_claim: root_1,
                    l2_block_number: TEST_BLOCK_INTERVAL,
                    parent_address: Address::ZERO, // correct: points to anchor
                },
            ),
            (
                proxy_addr(1),
                GameInfo {
                    root_claim: root_2,
                    l2_block_number: TEST_BLOCK_INTERVAL * 2,
                    // WRONG parent: points to some unrelated address
                    parent_address: Address::repeat_byte(0xFF),
                },
            ),
        ]);

        let output_roots =
            HashMap::from([(TEST_BLOCK_INTERVAL, root_1), (TEST_BLOCK_INTERVAL * 2, root_2)]);

        let pipeline = recovery_pipeline_with_roots(
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        // Should stop at first game because the second has wrong parent.
        assert_eq!(state.parent_address, proxy_addr(0));
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL);
        assert_eq!(state.output_root, root_1);
    }

    // ---- Recovery: duplicate block number handling (C2) ----

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_selects_correct_game_among_duplicates() {
        // Two games exist at the same block number (512), but only one has
        // the correct parent_address. The walk should select the right one.
        let root_1 = B256::repeat_byte(0x01);

        let wrong_proxy = Address::repeat_byte(0xAA);
        let correct_proxy = proxy_addr(1);

        let games = vec![
            game_entry(TEST_GAME_TYPE, 0), // proxy_addr(0) — wrong parent
            game_entry(TEST_GAME_TYPE, 1), // proxy_addr(1) — correct parent
        ];

        let info_map = HashMap::from([
            (
                proxy_addr(0),
                GameInfo {
                    root_claim: root_1,
                    l2_block_number: TEST_BLOCK_INTERVAL,
                    parent_address: wrong_proxy, // wrong parent
                },
            ),
            (
                correct_proxy,
                GameInfo {
                    root_claim: root_1,
                    l2_block_number: TEST_BLOCK_INTERVAL,
                    parent_address: Address::ZERO, // correct: points to anchor
                },
            ),
        ]);

        let output_roots = HashMap::from([(TEST_BLOCK_INTERVAL, root_1)]);

        let pipeline = recovery_pipeline_with_roots(
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        // Should pick the game with the correct parent_address.
        assert_eq!(state.parent_address, correct_proxy);
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL);
    }

    // ---- Recovery: anchor root cache invalidation (H3) ----

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_invalidated_by_anchor_root_change() {
        let (games, info_map, output_roots) = game_chain(1);

        // Extract the first game info BEFORE moving info_map into the mock.
        let first_info = info_map[&proxy_addr(0)];

        let pipeline = recovery_pipeline_with_roots(
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        // Seed cache with same game_count and a populated game map, but a
        // DIFFERENT anchor root. The map is reused (no factory RPCs) but the
        // walk is re-executed from the new anchor.
        let cached_map = HashMap::from([(
            TEST_BLOCK_INTERVAL,
            vec![ScannedGame { proxy: proxy_addr(0), info: first_info }],
        )]);
        let mut cache = Some(CachedRecovery {
            game_map: CachedGameMap { scanned_up_to: 1, map: cached_map },
            anchor_root: B256::repeat_byte(0xAA), // different from test_anchor_root
            state: RecoveredState {
                parent_address: proxy_addr(99), // stale — will be recomputed
                output_root: B256::repeat_byte(0xDD),
                l2_block_number: 9999,
            },
        });

        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        // Even though game_count matches, the anchor root changed, so the
        // cache should be invalidated and a fresh scan should be performed.
        assert_eq!(state.parent_address, proxy_addr(0));
        assert_eq!(state.l2_block_number, TEST_BLOCK_INTERVAL);
        // Cache should be repopulated with the new anchor root.
        assert_eq!(cache.as_ref().unwrap().anchor_root, B256::ZERO);
    }

    // ---- Recovery: intermediate output root verification ----

    /// Block intervals for recovery tests with multiple intermediate roots.
    const RECOVERY_BI: u64 = 4;
    const RECOVERY_IBI: u64 = 2;

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_forward_walk_verifies_intermediate_roots() {
        // block_interval = 4, intermediate_block_interval = 2
        // → intermediate_count = 2 (roots at parent+2 and parent+4)
        //
        // Two games: block 4 (parent = anchor) and block 8 (parent = game 0).
        // Both have correct root_claim AND correct intermediate roots.
        // Walk should traverse both games.
        let root_1 = B256::repeat_byte(0x01);
        let root_2 = B256::repeat_byte(0x02);
        let intermediate_at_2 = B256::repeat_byte(0xA1);
        let intermediate_at_6 = B256::repeat_byte(0xA2);

        let games = vec![game_entry(TEST_GAME_TYPE, 0), game_entry(TEST_GAME_TYPE, 1)];

        let info_map = HashMap::from([
            (
                proxy_addr(0),
                GameInfo {
                    root_claim: root_1,
                    l2_block_number: RECOVERY_BI,
                    parent_address: Address::ZERO,
                },
            ),
            (
                proxy_addr(1),
                GameInfo {
                    root_claim: root_2,
                    l2_block_number: RECOVERY_BI * 2,
                    parent_address: proxy_addr(0),
                },
            ),
        ]);

        // Canonical output roots for all intermediate + game blocks.
        let output_roots = HashMap::from([
            (2, intermediate_at_2),
            (RECOVERY_BI, root_1),
            (6, intermediate_at_6),
            (RECOVERY_BI * 2, root_2),
        ]);

        let mut verifier = MockAggregateVerifier::with_game_info(info_map);
        // Game 0: intermediate roots at blocks 2 and 4 (= root_claim)
        verifier.intermediate_roots_map.insert(proxy_addr(0), vec![intermediate_at_2, root_1]);
        // Game 1: intermediate roots at blocks 6 and 8 (= root_claim)
        verifier.intermediate_roots_map.insert(proxy_addr(1), vec![intermediate_at_6, root_2]);

        let pipeline = recovery_pipeline_full(
            MockDisputeGameFactory::with_games(games),
            verifier,
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
        assert_eq!(state.output_root, root_2);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_forward_walk_stops_at_intermediate_root_mismatch() {
        // block_interval = 4, intermediate_block_interval = 2
        // → intermediate_count = 2 (roots at parent+2 and parent+4)
        //
        // Game 0 at block 4: correct root_claim AND correct intermediate roots.
        // Game 1 at block 8: correct root_claim BUT wrong intermediate root at
        // block 6. Walk should stop at game 0.
        let root_1 = B256::repeat_byte(0x01);
        let root_2 = B256::repeat_byte(0x02);
        let intermediate_at_2 = B256::repeat_byte(0xA1);
        let canonical_at_6 = B256::repeat_byte(0xA2);
        let wrong_intermediate = B256::repeat_byte(0xFF);

        let games = vec![game_entry(TEST_GAME_TYPE, 0), game_entry(TEST_GAME_TYPE, 1)];

        let info_map = HashMap::from([
            (
                proxy_addr(0),
                GameInfo {
                    root_claim: root_1,
                    l2_block_number: RECOVERY_BI,
                    parent_address: Address::ZERO,
                },
            ),
            (
                proxy_addr(1),
                GameInfo {
                    root_claim: root_2,
                    l2_block_number: RECOVERY_BI * 2,
                    parent_address: proxy_addr(0),
                },
            ),
        ]);

        // Canonical output roots.
        let output_roots = HashMap::from([
            (2, intermediate_at_2),
            (RECOVERY_BI, root_1),
            (6, canonical_at_6),
            (RECOVERY_BI * 2, root_2),
        ]);

        let mut verifier = MockAggregateVerifier::with_game_info(info_map);
        // Game 0: correct intermediate roots.
        verifier.intermediate_roots_map.insert(proxy_addr(0), vec![intermediate_at_2, root_1]);
        // Game 1: WRONG intermediate root at index 0 (block 6).
        verifier.intermediate_roots_map.insert(proxy_addr(1), vec![wrong_intermediate, root_2]);

        let pipeline = recovery_pipeline_full(
            MockDisputeGameFactory::with_games(games),
            verifier,
            output_roots,
            TEST_ANCHOR_BLOCK,
            RECOVERY_BI,
            RECOVERY_IBI,
        );

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        // Walk should stop at game 0 because game 1 has wrong intermediate root.
        assert_eq!(state.parent_address, proxy_addr(0));
        assert_eq!(state.l2_block_number, RECOVERY_BI);
        assert_eq!(state.output_root, root_1);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_forward_walk_stops_at_wrong_intermediate_root_count() {
        // block_interval = 4, intermediate_block_interval = 2
        // → intermediate_count = 2
        //
        // Game at block 4 returns only 1 intermediate root instead of the
        // expected 2. Walk should treat this as a gap.
        let root_1 = B256::repeat_byte(0x01);
        let intermediate_at_2 = B256::repeat_byte(0xA1);

        let games = vec![game_entry(TEST_GAME_TYPE, 0)];

        let info_map = HashMap::from([(
            proxy_addr(0),
            GameInfo {
                root_claim: root_1,
                l2_block_number: RECOVERY_BI,
                parent_address: Address::ZERO,
            },
        )]);

        let output_roots = HashMap::from([(2, intermediate_at_2), (RECOVERY_BI, root_1)]);

        let mut verifier = MockAggregateVerifier::with_game_info(info_map);
        // Only 1 intermediate root instead of expected 2.
        verifier.intermediate_roots_map.insert(proxy_addr(0), vec![root_1]);

        let pipeline = recovery_pipeline_full(
            MockDisputeGameFactory::with_games(games),
            verifier,
            output_roots,
            TEST_ANCHOR_BLOCK,
            RECOVERY_BI,
            RECOVERY_IBI,
        );

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        // Walk should not advance past anchor since game has wrong count.
        assert_eq!(state.parent_address, Address::ZERO);
        assert_eq!(state.l2_block_number, TEST_ANCHOR_BLOCK);
    }

    // ---- Intermediate output root validation (submission) tests ----

    /// Shared block intervals for submission validation tests.
    const SUBMIT_BLOCK_INTERVAL: u64 = 4;
    const SUBMIT_INTERMEDIATE_INTERVAL: u64 = 2;

    fn submit_pipeline(output_roots: HashMap<u64, B256>) -> TestPipeline {
        let cancel = CancellationToken::new();
        let l1 = Arc::new(MockL1 { latest_block_number: TEST_L1_BLOCK_NUMBER });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let prover: Arc<dyn ProverClient> = Arc::new(MockProver { delay: MOCK_PROVER_DELAY });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(0, B256::ZERO),
            output_roots,
        });
        let anchor_registry =
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(TEST_ANCHOR_BLOCK) });

        ProvingPipeline::new(
            PipelineConfig {
                max_parallel_proofs: 1,
                max_retries: 1,
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    block_interval: SUBMIT_BLOCK_INTERVAL,
                    intermediate_block_interval: SUBMIT_INTERMEDIATE_INTERVAL,
                    ..Default::default()
                },
            },
            prover,
            l1,
            l2,
            rollup,
            anchor_registry,
            Arc::new(MockDisputeGameFactory::with_count(0)),
            Arc::new(MockAggregateVerifier::empty()),
            Arc::new(MockOutputProposer),
            cancel,
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
