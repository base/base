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
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, Signature, keccak256};
use alloy_sol_types::SolCall;
use base_proof_contracts::{
    AggregateVerifierClient, AnchorStateRegistryClient, DisputeGameFactoryClient,
    ITEEProverRegistry, encode_extra_data,
};
use base_proof_primitives::{ProofJournal, ProofRequest, Proposal};
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
    proof_source::{DualPlatformProof, TeeProofError, TeeProofPlatform, TeeProofSources},
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
    /// Optional service readiness flag for the health server.
    pub readiness: Option<Arc<AtomicBool>>,
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
    prove_tasks: JoinSet<(u64, Result<DualPlatformProof, TeeProofError>)>,
    /// At most one concurrent submission task.
    submit_tasks: JoinSet<SubmitOutcome>,
    /// Completed proofs waiting for sequential submission, keyed by target block.
    proved: BTreeMap<u64, DualPlatformProof>,
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

    fn is_empty(&self) -> bool {
        self.prove_tasks.is_empty()
            && self.submit_tasks.is_empty()
            && self.proved.is_empty()
            && self.inflight.is_empty()
            && self.submitting.is_none()
            && self.retry_counts.is_empty()
    }

    fn prune_stale(&mut self, recovered_block: u64) {
        self.proved.retain(|&target, _| target > recovered_block);
        self.inflight.retain(|&target| target > recovered_block);
        self.retry_counts.retain(|&target, _| target > recovered_block);
        // In-flight submit tasks are intentionally NOT aborted: when recovery
        // advances past `submitting`, the L1 tx already landed and the task's
        // Success/GameAlreadyExists outcome is needed to record metrics and
        // clear `submitting` in `handle_submit_result`.
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
    proof_sources: TeeProofSources,
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
            proof_sources: self.proof_sources.clone(),
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
        proof_sources: TeeProofSources,
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
            proof_sources,
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

            // Readiness means the proposer can recover chain state and start
            // work. Proof health is tracked separately by per-platform gauges
            // and failures below can still mark the service unready.
            self.set_ready(true);

            self.dispatch_proofs(&recovered, safe_head, state).await?;
            // No next target left to dispatch and nothing in flight → fully
            // caught up; advertise readiness even if no proof has completed
            // this session.
            let next_target =
                recovered.l2_block_number.checked_add(self.config.driver.block_interval);
            if next_target.is_none_or(|t| t > safe_head) && state.is_empty() {
                self.set_ready(true);
            }
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
            // After a contiguous run of skipped (already-handled) blocks, the
            // chained proof must start from the last skipped block, so its
            // output root is fetched once after the run instead of per skip.
            let mut last_skipped = None;
            while cursor <= safe_head
                && (state.inflight.contains(&cursor)
                    || state.proved.contains_key(&cursor)
                    || state.submitting == Some(cursor))
            {
                last_skipped = Some(cursor);
                cursor = match cursor.checked_add(self.config.driver.block_interval) {
                    Some(c) => c,
                    None => return Ok(()),
                };
            }

            if cursor > safe_head || state.inflight.len() >= self.config.max_parallel_proofs {
                break;
            }

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
                    let proof_sources = self.proof_sources.clone();
                    let nitro_image_hash = self.config.driver.tee_nitro_image_hash;
                    let tdx_image_hash = self.config.driver.tee_tdx_image_hash;
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
                                (target, Err(TeeProofError::Other {
                                    error: ProposerError::Internal("cancelled".into()),
                                }))
                            }
                            result = proof_sources.prove(request, nitro_image_hash, tdx_image_hash) => {
                                drop(proof_timer);
                                (target, result)
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
                        proof: Box::new(proof_result),
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

    /// Shared cleanup after a submission lands on-chain (`Success` or
    /// `GameAlreadyExists`). Records metrics, clears per-block bookkeeping,
    /// and refreshes the recovery cache so `prune_stale` can drop completed
    /// proofs.
    async fn finalize_successful_submit(&self, target_block: u64, state: &mut PipelineState) {
        Metrics::last_proposed_block().set(target_block as f64);
        state.retry_counts.remove(&target_block);
        state.submitting = None;
        match self.recover_latest_state(&mut state.cached_recovery).await {
            Ok(recovered) => state.prune_stale(recovered.l2_block_number),
            Err(e) => warn!(error = %e, "Failed to recover state after submission"),
        }
        state.record_gauges();
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
                self.finalize_successful_submit(target_block, state).await;
                true
            }
            SubmitOutcome::GameAlreadyExists { target_block } => {
                info!(target_block, "Game already exists on chain");
                // Decrement cached game_count so the next forward walk discovers
                // the existing game with a single incremental `factory.games()`
                // lookup. The walk missed it on the prior tick because
                // `game_count` was likely served by a different L1 RPC replica
                // than the one fielding `factory.games()`.
                if let Some(cached) = &mut state.cached_recovery {
                    cached.game_count = cached.game_count.saturating_sub(1);
                }
                self.finalize_successful_submit(target_block, state).await;
                true
            }
            SubmitOutcome::RootMismatch { target_block } => {
                warn!(target_block, "Output root mismatch at submit time, resetting pipeline");
                Metrics::root_mismatch_total().increment(1);
                self.mark_all_platforms_unready();
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
                state.proved.insert(target_block, *proof);
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
        join_result: Result<
            (u64, Result<DualPlatformProof, TeeProofError>),
            tokio::task::JoinError,
        >,
        state: &mut PipelineState,
    ) {
        match join_result {
            Ok((target, Ok(proof_result))) => {
                let signer_validation_required = self.config.tee_prover_registry_address.is_some();
                if !signer_validation_required {
                    self.set_ready(true);
                }
                for proof in proof_result.platform_proofs() {
                    if !signer_validation_required {
                        Metrics::tee_platform_ready(proof.platform.label()).set(1.0);
                    }
                    Metrics::tee_platform_proofs_total(proof.platform.label()).increment(1);
                }
                state.inflight.remove(&target);
                state.retry_counts.remove(&target);
                state.proved.insert(target, proof_result);
                state.record_gauges();
                info!(target_block = target, "Proof completed successfully");
            }
            Ok((target, Err(e))) => {
                self.set_ready(false);
                for (platform, ready) in e.platform_readiness() {
                    Metrics::tee_platform_ready(platform.label()).set(if ready {
                        1.0
                    } else {
                        0.0
                    });
                }
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
                self.mark_all_platforms_unready();
                warn!(error = %join_err, "Proof task panicked");
                state.reset();
            }
        }
    }

    fn set_ready(&self, ready: bool) {
        if let Some(readiness) = &self.config.readiness {
            readiness.store(ready, Ordering::SeqCst);
        }
    }

    fn mark_platform_unready(&self, platform: TeeProofPlatform) {
        self.set_ready(false);
        Metrics::tee_platform_ready(platform.label()).set(0.0);
    }

    fn mark_all_platforms_unready(&self) {
        self.set_ready(false);
        Metrics::tee_platform_ready(TeeProofPlatform::Nitro.label()).set(0.0);
        Metrics::tee_platform_ready(TeeProofPlatform::Tdx.label()).set(0.0);
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
        let (count, snapshot) = tokio::try_join!(
            async {
                self.factory_client.game_count().await.map_err(|e| {
                    ProposerError::Contract(format!("recovery game_count failed: {e}"))
                })
            },
            async {
                self.anchor_registry
                    .anchor_snapshot()
                    .await
                    .map_err(|e| ProposerError::Contract(format!("anchor_snapshot failed: {e}")))
            },
        )?;
        let anchor = snapshot.anchor_root;

        // The anchor only advances when games resolve, so a cached tip stays
        // valid until the anchor catches up to it.
        let tip_still_valid =
            |cached: &CachedRecovery| anchor.l2_block_number <= cached.state.l2_block_number;

        if let Some(cached) = cache.as_ref()
            && tip_still_valid(cached)
            && cached.game_count == count
        {
            debug!(game_count = count, "No changes since last recovery, returning cached state");
            return Ok(cached.state);
        }

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
            let intermediate_roots = match self.fetch_canonical_roots(&intermediate_blocks).await {
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
                    warn!(
                        expected_block,
                        parent_block,
                        error = %e,
                        "Forward walk failed to fetch canonical roots"
                    );
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
        blocks: &[u64],
    ) -> Result<HashMap<u64, B256>, ProposerError> {
        if blocks.is_empty() {
            return Ok(HashMap::new());
        }
        stream::iter(blocks.iter().copied())
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
                    .header_by_number(BlockNumberOrTag::Number(starting_block_number))
                    .await
                    .map_err(ProposerError::Rpc)
            },
            async {
                self.rollup_client.output_at_block(target_block).await.map_err(ProposerError::Rpc)
            },
            async {
                self.l1_client
                    .header_by_number(BlockNumberOrTag::Latest)
                    .await
                    .map_err(ProposerError::Rpc)
            },
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
            image_hash: B256::ZERO,
        };

        info!(request = ?request, "Built proof request for parallel proving");

        Ok(request)
    }

    /// Recovers the TEE signer from the aggregate proposal and checks
    /// `isValidSigner` on the `TEEProverRegistry`.
    ///
    /// Returns a detailed signer validity status, or `Err` if the check itself
    /// failed (RPC error, parse failure, etc.).
    async fn check_signer_validity(
        &self,
        platform: TeeProofPlatform,
        aggregate_proposal: &Proposal,
        starting_block_number: u64,
        intermediate_roots: &[B256],
        registry_address: Address,
    ) -> Result<SignerValidity, ProposerError> {
        let journal = ProofJournal {
            proposer: self.config.driver.proposer_address,
            l1_origin_hash: aggregate_proposal.l1_origin_hash,
            prev_output_root: aggregate_proposal.prev_output_root,
            starting_l2_block: starting_block_number,
            output_root: aggregate_proposal.output_root,
            ending_l2_block: aggregate_proposal.l2_block_number,
            intermediate_roots: intermediate_roots.to_vec(),
            config_hash: aggregate_proposal.config_hash,
            tee_image_hash: match platform {
                TeeProofPlatform::Nitro => self.config.driver.tee_nitro_image_hash,
                TeeProofPlatform::Tdx => self.config.driver.tee_tdx_image_hash,
            },
        };
        let digest = keccak256(journal.encode());

        let sig = Signature::try_from(aggregate_proposal.signature.as_ref())
            .map_err(|e| ProposerError::Internal(format!("invalid proposal signature: {e}")))?;
        let signer = sig
            .recover_address_from_prehash(&digest)
            .map_err(|e| ProposerError::Internal(format!("signer recovery failed: {e}")))?;

        debug!(platform = platform.label(), signer = %signer, "recovered TEE signer from aggregate proposal");

        // isValidSigner errors propagate so the submission can be retried.
        let is_valid = self
            .call_registry(registry_address, ITEEProverRegistry::isValidSignerCall { signer })
            .await?;
        debug!(platform = platform.label(), signer = %signer, is_valid, "isValidSigner check result");

        if is_valid {
            return Ok(SignerValidity::Valid);
        }

        // Diagnostic calls are independent — fan out concurrently. Any failure
        // collapses to `Invalid { signer }` so the proof is discarded with a
        // generic reason.
        let log_diag = |what: &'static str, e: &ProposerError| {
            warn!(error = %e, platform = platform.label(), signer = %signer, "{what}");
        };
        let (registered_res, signer_image_res, expected_image_res) = tokio::join!(
            self.call_registry(
                registry_address,
                ITEEProverRegistry::isRegisteredSignerCall { signer }
            ),
            self.call_registry(
                registry_address,
                ITEEProverRegistry::signerImageHashCall { signer }
            ),
            self.call_expected_image_hash(registry_address, platform),
        );

        let is_registered = match registered_res {
            Ok(v) => v,
            Err(e) => {
                log_diag("failed to classify invalid TEE signer registration status", &e);
                return Ok(SignerValidity::Invalid { signer });
            }
        };
        if !is_registered {
            return Ok(SignerValidity::NotRegistered { signer });
        }
        let signer_image_hash = match signer_image_res {
            Ok(v) => v,
            Err(e) => {
                log_diag("failed to classify invalid TEE signer image hash", &e);
                return Ok(SignerValidity::Invalid { signer });
            }
        };
        let expected_image_hash = match expected_image_res {
            Ok(v) => v,
            Err(e) => {
                log_diag("failed to classify invalid TEE signer expected image hash", &e);
                return Ok(SignerValidity::Invalid { signer });
            }
        };

        Ok(SignerValidity::WrongImageHash { signer, signer_image_hash, expected_image_hash })
    }

    async fn call_expected_image_hash(
        &self,
        registry_address: Address,
        platform: TeeProofPlatform,
    ) -> Result<B256, ProposerError> {
        match platform {
            TeeProofPlatform::Nitro => {
                self.call_registry(
                    registry_address,
                    ITEEProverRegistry::getExpectedNitroImageHashCall {},
                )
                .await
            }
            TeeProofPlatform::Tdx => {
                self.call_registry(
                    registry_address,
                    ITEEProverRegistry::getExpectedTDXImageHashCall {},
                )
                .await
            }
        }
    }

    /// Issues a single `ITEEProverRegistry` call and decodes the typed return.
    async fn call_registry<C: SolCall>(
        &self,
        registry_address: Address,
        call: C,
    ) -> Result<C::Return, ProposerError> {
        let result = self
            .l1_client
            .call_contract(registry_address, call.abi_encode().into(), BlockNumberOrTag::Latest)
            .await
            .map_err(ProposerError::Rpc)?;
        C::abi_decode_returns(&result).map_err(|e| {
            ProposerError::Internal(format!("failed to decode registry response: {e}"))
        })
    }

    /// Translates a per-platform `SignerValidity` result into a `SubmitAction`.
    ///
    /// `Valid` updates the readiness gauge for the platform; everything else
    /// short-circuits the surrounding submission with `Failed` (transient) or
    /// `Discard` (permanently invalid proof).
    fn process_signer_validity(
        &self,
        platform: TeeProofPlatform,
        result: Result<SignerValidity, ProposerError>,
        target_block: u64,
    ) -> Result<(), SubmitAction> {
        match result {
            Ok(SignerValidity::Valid) => {
                Metrics::tee_platform_ready(platform.label()).set(1.0);
                Ok(())
            }
            Ok(SignerValidity::Invalid { signer }) => {
                warn!(
                    platform = platform.label(), signer = %signer, target_block,
                    "TEE signer is invalid on-chain, discarding proof"
                );
                Err(self.discard_invalid_signer(
                    platform,
                    format!("{platform} TEE signer invalid on-chain"),
                ))
            }
            Ok(SignerValidity::NotRegistered { signer }) => {
                warn!(
                    platform = platform.label(), signer = %signer, target_block,
                    "TEE signer is not registered on-chain, discarding proof"
                );
                Err(self.discard_invalid_signer(
                    platform,
                    format!("{platform} TEE signer not registered on-chain"),
                ))
            }
            Ok(SignerValidity::WrongImageHash {
                signer,
                signer_image_hash,
                expected_image_hash,
            }) => {
                warn!(
                    platform = platform.label(),
                    signer = %signer,
                    signer_image_hash = %signer_image_hash,
                    expected_image_hash = %expected_image_hash,
                    target_block,
                    "TEE signer is registered with the wrong image hash, discarding proof"
                );
                Err(self.discard_invalid_signer(
                    platform,
                    format!(
                        "{platform} TEE signer registered with wrong image hash: signer_image_hash={signer_image_hash}, expected_image_hash={expected_image_hash}"
                    ),
                ))
            }
            Err(e) => {
                self.mark_platform_unready(platform);
                warn!(
                    error = %e, platform = platform.label(), target_block,
                    "signer validity check failed, retrying submission"
                );
                Err(SubmitAction::Failed(e))
            }
        }
    }

    fn discard_invalid_signer(&self, platform: TeeProofPlatform, reason: String) -> SubmitAction {
        self.mark_platform_unready(platform);
        Metrics::tee_platform_signer_invalid_total(platform.label()).increment(1);
        Metrics::tee_signer_invalid_total().increment(1);
        SubmitAction::Discard(ProposerError::Internal(reason))
    }

    #[instrument(skip_all, fields(target_block = target_block, parent_address = %parent_address))]
    async fn validate_and_submit(
        &self,
        proof_result: &DualPlatformProof,
        target_block: u64,
        parent_address: Address,
    ) -> Result<(), SubmitAction> {
        let submission_proof = proof_result.submission_proof();
        let aggregate_proposal = &submission_proof.aggregate_proposal;
        let proposals = &submission_proof.proposals;

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

        // Fetch the JIT canonical root for the target block in parallel with
        // the non-target intermediate roots — they're independent RPCs and the
        // happy path needs both.
        let non_target_blocks: Vec<u64> =
            intermediate_blocks.iter().copied().filter(|&b| b != target_block).collect();
        let (canonical_output, mut canonical_map) = tokio::try_join!(
            async {
                self.rollup_client
                    .output_at_block(target_block)
                    .await
                    .map_err(|e| SubmitAction::Failed(ProposerError::Rpc(e)))
            },
            async {
                self.fetch_canonical_roots(&non_target_blocks).await.map_err(SubmitAction::Failed)
            },
        )?;

        if aggregate_proposal.output_root != canonical_output.output_root {
            warn!(
                proposal_root = ?aggregate_proposal.output_root,
                canonical_root = ?canonical_output.output_root,
                target_block,
                "Proposal output root does not match canonical chain at submit time"
            );
            self.mark_all_platforms_unready();
            return Err(SubmitAction::RootMismatch);
        }

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
                self.mark_all_platforms_unready();
                return Err(SubmitAction::RootMismatch);
            }
        }

        // Pre-submission signer validation: if a TEE prover registry is
        // configured, recover the signer from the aggregate proposal signature
        // and check `isValidSigner` on-chain. If the signer is invalid, skip
        // submission to avoid wasting gas on a transaction that will revert.
        if let Some(registry_address) = self.config.tee_prover_registry_address {
            let nitro_aggregate = &proof_result.nitro.aggregate_proposal;
            let tdx_aggregate = &proof_result.tdx.aggregate_proposal;
            let (nitro_res, tdx_res) = tokio::join!(
                self.check_signer_validity(
                    proof_result.nitro.platform,
                    nitro_aggregate,
                    starting_block_number,
                    &intermediate_roots,
                    registry_address,
                ),
                self.check_signer_validity(
                    proof_result.tdx.platform,
                    tdx_aggregate,
                    starting_block_number,
                    &intermediate_roots,
                    registry_address,
                ),
            );
            self.process_signer_validity(proof_result.nitro.platform, nitro_res, target_block)?;
            self.process_signer_validity(proof_result.tdx.platform, tdx_res, target_block)?;
            self.set_ready(true);
        }

        info!(
            target_block,
            output_root = ?aggregate_proposal.output_root,
            parent_address = %parent_address,
            intermediate_roots_count = intermediate_roots.len(),
            proposals_count = proposals.len(),
            "Proposing output (creating dispute game)"
        );

        let proof_data = proof_result
            .build_proof_data(
                self.config.driver.tee_nitro_image_hash,
                self.config.driver.tee_tdx_image_hash,
            )
            .map_err(|e| {
                SubmitAction::Failed(ProposerError::Internal(format!(
                    "failed to build dual-platform proof data: {e}"
                )))
            })?;

        // Submit with timeout.
        let mut propose_timer = base_metrics::timed!(Metrics::proposal_l1_tx_duration_seconds());
        let propose_result = tokio::time::timeout(
            PROPOSAL_TIMEOUT,
            self.output_proposer.propose_output(
                aggregate_proposal,
                parent_address,
                &intermediate_roots,
                proof_data,
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

/// Result of a platform signer registry validation.
#[derive(Debug)]
enum SignerValidity {
    /// Signer is registered and matches the expected image hash.
    Valid,
    /// Signer failed `isValidSigner`, but diagnostics could not classify why.
    Invalid { signer: Address },
    /// Signer is not registered in the TEE prover registry.
    NotRegistered { signer: Address },
    /// Signer is registered but under a different image hash.
    WrongImageHash { signer: Address, signer_image_hash: B256, expected_image_hash: B256 },
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
    Failed { target_block: u64, proof: Box<DualPlatformProof>, error: ProposerError },
    Discard { target_block: u64, error: ProposerError },
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use alloy_sol_types::{SolCall, SolValue};
    use base_proof_primitives::{ProofRequest, ProofResult, Proposal, ProverClient};
    use base_proof_rpc::RpcResult;
    use rstest::rstest;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        proof_source::PlatformProof,
        test_utils::{
            MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
            MockOutputProposer, MockProver, MockRollupClient, test_anchor_root, test_proposal,
            test_sync_status,
        },
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

    /// Nitro image hash used by signer-validation tests.
    const TEST_NITRO_IMAGE_HASH: B256 = B256::repeat_byte(0x55);

    /// TDX image hash used by signer-validation tests.
    const TEST_TDX_IMAGE_HASH: B256 = B256::repeat_byte(0x66);

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

    fn proof_sources(prover: Arc<dyn ProverClient>) -> TeeProofSources {
        TeeProofSources::new(Arc::clone(&prover), prover)
    }

    fn dual_proof_result(target_block: u64) -> DualPlatformProof {
        let nitro_proof = submit_proof_result(target_block, 0xAA, 0);
        let tdx_proof = submit_proof_result(target_block, 0xBB, 1);
        DualPlatformProof::new(
            PlatformProof::new(TeeProofPlatform::Nitro, nitro_proof).unwrap(),
            PlatformProof::new(TeeProofPlatform::Tdx, tdx_proof).unwrap(),
        )
        .unwrap()
    }

    #[derive(Debug)]
    struct RecordingSignedProver {
        platform: TeeProofPlatform,
        signer: PrivateKeySigner,
        calls: Arc<Mutex<Vec<(TeeProofPlatform, ProofRequest)>>>,
        block_interval: u64,
        proposer_address: Address,
        tee_image_hash: B256,
    }

    #[async_trait::async_trait]
    impl ProverClient for RecordingSignedProver {
        async fn prove(
            &self,
            request: ProofRequest,
        ) -> Result<ProofResult, Box<dyn std::error::Error + Send + Sync>> {
            self.calls.lock().unwrap().push((self.platform, request.clone()));

            let target = request.claimed_l2_block_number;
            let start = target.saturating_sub(self.block_interval);
            let proposals: Vec<Proposal> = ((start + 1)..=target).map(test_proposal).collect();
            let mut aggregate_proposal = test_proposal(target);
            let intermediate_roots = proposals
                .iter()
                .enumerate()
                .filter_map(|(index, proposal)| {
                    let block = start + index as u64 + 1;
                    (block.is_multiple_of(SUBMIT_INTERMEDIATE_INTERVAL))
                        .then_some(proposal.output_root)
                })
                .collect::<Vec<_>>();
            let journal = ProofJournal {
                proposer: self.proposer_address,
                l1_origin_hash: aggregate_proposal.l1_origin_hash,
                prev_output_root: aggregate_proposal.prev_output_root,
                starting_l2_block: start,
                output_root: aggregate_proposal.output_root,
                ending_l2_block: target,
                intermediate_roots,
                config_hash: aggregate_proposal.config_hash,
                tee_image_hash: self.tee_image_hash,
            };
            let digest = keccak256(journal.encode());
            let signature = self.signer.sign_hash_sync(&digest)?;
            aggregate_proposal.signature = Bytes::from(signature.as_rsy().to_vec());

            Ok(ProofResult::Tee { aggregate_proposal, proposals })
        }
    }

    #[derive(Debug)]
    struct FailingProver {
        message: &'static str,
    }

    #[async_trait::async_trait]
    impl ProverClient for FailingProver {
        async fn prove(
            &self,
            _request: ProofRequest,
        ) -> Result<ProofResult, Box<dyn std::error::Error + Send + Sync>> {
            Err(self.message.into())
        }
    }

    #[derive(Debug)]
    struct MockRegistryL1 {
        latest_block_number: u64,
        expected_nitro_image_hash: B256,
        expected_tdx_image_hash: B256,
        signer_expected_image_hashes: HashMap<Address, B256>,
        signer_image_hashes: HashMap<Address, B256>,
        failing_call: Option<RegistryCall>,
        failing_signer: Option<Address>,
    }

    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    enum RegistryCall {
        IsValidSigner,
        IsRegisteredSigner,
        SignerImageHash,
        GetExpectedImageHash,
    }

    #[async_trait::async_trait]
    impl L1Provider for MockRegistryL1 {
        async fn block_number(&self) -> RpcResult<u64> {
            Ok(self.latest_block_number)
        }

        async fn header_by_number(&self, _: Option<u64>) -> RpcResult<alloy_rpc_types_eth::Header> {
            Ok(alloy_rpc_types_eth::Header { hash: B256::repeat_byte(0x11), ..Default::default() })
        }

        async fn header_by_hash(&self, _: B256) -> RpcResult<alloy_rpc_types_eth::Header> {
            unimplemented!()
        }

        async fn block_receipts(
            &self,
            _: B256,
        ) -> RpcResult<Vec<alloy_rpc_types_eth::TransactionReceipt>> {
            unimplemented!()
        }

        async fn code_at(&self, _: Address, _: Option<u64>) -> RpcResult<Bytes> {
            unimplemented!()
        }

        async fn call_contract(
            &self,
            _: Address,
            calldata: Bytes,
            _: Option<u64>,
        ) -> RpcResult<Bytes> {
            let selector = &calldata[..4];
            if selector == ITEEProverRegistry::isValidSignerCall::SELECTOR.as_slice() {
                let signer = Self::calldata_signer(&calldata);
                if self.should_fail(RegistryCall::IsValidSigner, signer) {
                    return Err(RpcError::InvalidResponse("isValidSigner failed".into()));
                }
                let is_valid = self
                    .signer_image_hashes
                    .get(&signer)
                    .zip(self.signer_expected_image_hashes.get(&signer))
                    .is_some_and(|(image_hash, expected)| image_hash == expected);
                return Ok(Bytes::from(SolValue::abi_encode(&is_valid)));
            }
            if selector == ITEEProverRegistry::isRegisteredSignerCall::SELECTOR.as_slice() {
                let signer = Self::calldata_signer(&calldata);
                if self.should_fail(RegistryCall::IsRegisteredSigner, signer) {
                    return Err(RpcError::InvalidResponse("isRegisteredSigner failed".into()));
                }
                let is_registered = self.signer_image_hashes.contains_key(&signer);
                return Ok(Bytes::from(SolValue::abi_encode(&is_registered)));
            }
            if selector == ITEEProverRegistry::signerImageHashCall::SELECTOR.as_slice() {
                let signer = Self::calldata_signer(&calldata);
                if self.should_fail(RegistryCall::SignerImageHash, signer) {
                    return Err(RpcError::InvalidResponse("signerImageHash failed".into()));
                }
                let image_hash = self.signer_image_hashes.get(&signer).copied().unwrap_or_default();
                return Ok(Bytes::from(SolValue::abi_encode(&image_hash)));
            }
            if selector == ITEEProverRegistry::getExpectedNitroImageHashCall::SELECTOR.as_slice() {
                if self.failing_call == Some(RegistryCall::GetExpectedImageHash) {
                    return Err(RpcError::InvalidResponse(
                        "getExpectedNitroImageHash failed".into(),
                    ));
                }
                return Ok(Bytes::from(SolValue::abi_encode(&self.expected_nitro_image_hash)));
            }
            if selector == ITEEProverRegistry::getExpectedTDXImageHashCall::SELECTOR.as_slice() {
                if self.failing_call == Some(RegistryCall::GetExpectedImageHash) {
                    return Err(RpcError::InvalidResponse("getExpectedTDXImageHash failed".into()));
                }
                return Ok(Bytes::from(SolValue::abi_encode(&self.expected_tdx_image_hash)));
            }
            Err(RpcError::InvalidResponse("unexpected registry call".into()))
        }

        async fn get_balance(&self, _: Address) -> RpcResult<U256> {
            Ok(U256::ZERO)
        }
    }

    impl MockRegistryL1 {
        fn calldata_signer(calldata: &[u8]) -> Address {
            Address::from_slice(&calldata[16..36])
        }

        fn should_fail(&self, call: RegistryCall, signer: Address) -> bool {
            self.failing_call == Some(call)
                && self.failing_signer.is_none_or(|failing_signer| failing_signer == signer)
        }
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
            proof_sources(prover),
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
                readiness: None,
                driver: DriverConfig {
                    game_type: TEST_GAME_TYPE,
                    block_interval,
                    intermediate_block_interval,
                    ..Default::default()
                },
            },
            proof_sources(prover),
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
                readiness: None,
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
                readiness: None,
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
                readiness: None,
                driver: DriverConfig {
                    game_type: TEST_GAME_TYPE,
                    block_interval: TEST_BLOCK_INTERVAL,
                    intermediate_block_interval: TEST_BLOCK_INTERVAL,
                    ..Default::default()
                },
            },
            proof_sources(prover),
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
                readiness: None,
                driver: DriverConfig {
                    block_interval: TEST_BLOCK_INTERVAL,
                    intermediate_block_interval: TEST_BLOCK_INTERVAL,
                    ..Default::default()
                },
            },
            proof_sources(prover),
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
        state.proved.insert(TEST_BLOCK_INTERVAL, dual_proof_result(TEST_BLOCK_INTERVAL));
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
                readiness: None,
                driver: DriverConfig {
                    block_interval: TEST_BLOCK_INTERVAL,
                    intermediate_block_interval: TEST_BLOCK_INTERVAL,
                    ..Default::default()
                },
            },
            proof_sources(prover),
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

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_tick_marks_idle_pipeline_ready_after_successful_recovery() {
        let readiness = Arc::new(AtomicBool::new(false));
        let pipeline = test_pipeline(
            PipelineConfig {
                max_parallel_proofs: 1,
                max_retries: 1,
                recovery_scan_concurrency: 8,
                tee_prover_registry_address: None,
                readiness: Some(Arc::clone(&readiness)),
                driver: DriverConfig {
                    game_type: TEST_GAME_TYPE,
                    block_interval: TEST_BLOCK_INTERVAL,
                    intermediate_block_interval: TEST_BLOCK_INTERVAL,
                    ..Default::default()
                },
            },
            TEST_ANCHOR_BLOCK,
            CancellationToken::new(),
        );
        let mut state = PipelineState::new();

        pipeline.tick(&mut state).await.unwrap();

        assert!(readiness.load(Ordering::SeqCst));
        assert!(state.is_empty());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_tick_marks_pipeline_ready_before_dispatching_proofs() {
        let readiness = Arc::new(AtomicBool::new(false));
        let pipeline = test_pipeline(
            PipelineConfig {
                max_parallel_proofs: 1,
                max_retries: 1,
                recovery_scan_concurrency: 8,
                tee_prover_registry_address: None,
                readiness: Some(Arc::clone(&readiness)),
                driver: DriverConfig {
                    game_type: TEST_GAME_TYPE,
                    block_interval: TEST_BLOCK_INTERVAL,
                    intermediate_block_interval: TEST_BLOCK_INTERVAL,
                    ..Default::default()
                },
            },
            TEST_BLOCK_INTERVAL,
            CancellationToken::new(),
        );
        let mut state = PipelineState::new();

        pipeline.tick(&mut state).await.unwrap();

        assert!(readiness.load(Ordering::SeqCst));
        assert!(state.inflight.contains(&TEST_BLOCK_INTERVAL));
        assert_eq!(state.inflight.len(), 1);
        assert!(state.proved.is_empty());
    }

    // ---- State management tests ----

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_prune_stale_does_not_abort_inflight_submit() {
        let mut state = PipelineState::new();
        state.submitting = Some(512);
        state.proved.insert(512, dual_proof_result(512));
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

    fn submit_proof_result(target_block: u64, signature_fill: u8, signature_v: u8) -> ProofResult {
        let proposals: Vec<Proposal> = (1..=target_block).map(test_proposal).collect();
        let mut aggregate = test_proposal(target_block);
        let mut signature = vec![signature_fill; 65];
        signature[64] = signature_v;
        aggregate.signature = Bytes::from(signature);
        ProofResult::Tee { aggregate_proposal: aggregate, proposals }
    }

    fn signed_proof_request() -> ProofRequest {
        ProofRequest {
            l1_head: B256::repeat_byte(0x11),
            agreed_l2_head_hash: B256::repeat_byte(0x30),
            agreed_l2_output_root: B256::ZERO,
            claimed_l2_output_root: B256::repeat_byte(SUBMIT_BLOCK_INTERVAL as u8),
            claimed_l2_block_number: SUBMIT_BLOCK_INTERVAL,
            proposer: Address::repeat_byte(0x44),
            intermediate_block_interval: SUBMIT_INTERMEDIATE_INTERVAL,
            l1_head_number: TEST_L1_BLOCK_NUMBER,
            image_hash: B256::ZERO,
        }
    }

    fn recording_sources(
        calls: Arc<Mutex<Vec<(TeeProofPlatform, ProofRequest)>>>,
        nitro_signer: PrivateKeySigner,
        tdx_signer: PrivateKeySigner,
    ) -> TeeProofSources {
        let request = signed_proof_request();
        let nitro: Arc<dyn ProverClient> = Arc::new(RecordingSignedProver {
            platform: TeeProofPlatform::Nitro,
            signer: nitro_signer,
            calls: Arc::clone(&calls),
            block_interval: SUBMIT_BLOCK_INTERVAL,
            proposer_address: request.proposer,
            tee_image_hash: TEST_NITRO_IMAGE_HASH,
        });
        let tdx: Arc<dyn ProverClient> = Arc::new(RecordingSignedProver {
            platform: TeeProofPlatform::Tdx,
            signer: tdx_signer,
            calls,
            block_interval: SUBMIT_BLOCK_INTERVAL,
            proposer_address: request.proposer,
            tee_image_hash: TEST_TDX_IMAGE_HASH,
        });
        TeeProofSources::new(nitro, tdx)
    }

    fn registry_pipeline(
        l1: MockRegistryL1,
        readiness: Arc<AtomicBool>,
    ) -> ProvingPipeline<
        MockRegistryL1,
        MockL2,
        MockRollupClient,
        MockAnchorStateRegistry,
        MockDisputeGameFactory,
    > {
        let request = signed_proof_request();
        let prover: Arc<dyn ProverClient> = Arc::new(MockProver {
            delay: MOCK_PROVER_DELAY,
            block_interval: SUBMIT_BLOCK_INTERVAL,
        });
        ProvingPipeline::new(
            PipelineConfig {
                max_parallel_proofs: 1,
                max_retries: 1,
                recovery_scan_concurrency: 8,
                tee_prover_registry_address: Some(Address::repeat_byte(0x77)),
                readiness: Some(readiness),
                driver: DriverConfig {
                    block_interval: SUBMIT_BLOCK_INTERVAL,
                    intermediate_block_interval: SUBMIT_INTERMEDIATE_INTERVAL,
                    proposer_address: request.proposer,
                    tee_nitro_image_hash: TEST_NITRO_IMAGE_HASH,
                    tee_tdx_image_hash: TEST_TDX_IMAGE_HASH,
                    ..Default::default()
                },
            },
            proof_sources(prover),
            Arc::new(l1),
            Arc::new(MockL2 { block_not_found: true, canonical_hash: None }),
            Arc::new(MockRollupClient {
                sync_status: test_sync_status(SUBMIT_BLOCK_INTERVAL, B256::ZERO),
                output_roots: HashMap::new(),
                max_safe_block: None,
            }),
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(TEST_ANCHOR_BLOCK) }),
            Arc::new(MockDisputeGameFactory::with_games(vec![])),
            Arc::new(MockAggregateVerifier::default()),
            Arc::new(MockOutputProposer),
            CancellationToken::new(),
        )
    }

    #[tokio::test]
    async fn dual_proof_sources_request_both_platforms_for_same_input() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let nitro_signer = PrivateKeySigner::from_slice(&[1u8; 32]).unwrap();
        let tdx_signer = PrivateKeySigner::from_slice(&[2u8; 32]).unwrap();
        let sources = recording_sources(Arc::clone(&calls), nitro_signer, tdx_signer);
        let request = signed_proof_request();

        let proof = sources
            .prove(request.clone(), TEST_NITRO_IMAGE_HASH, TEST_TDX_IMAGE_HASH)
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        let mut nitro_request = request.clone();
        nitro_request.image_hash = TEST_NITRO_IMAGE_HASH;
        let mut tdx_request = request;
        tdx_request.image_hash = TEST_TDX_IMAGE_HASH;
        assert!(calls.contains(&(TeeProofPlatform::Nitro, nitro_request)));
        assert!(calls.contains(&(TeeProofPlatform::Tdx, tdx_request)));
        let nitro_aggregate = &proof.nitro.aggregate_proposal;
        let tdx_aggregate = &proof.tdx.aggregate_proposal;
        assert_ne!(nitro_aggregate.signature, tdx_aggregate.signature);
        assert_eq!(nitro_aggregate.output_root, tdx_aggregate.output_root);
        assert_eq!(nitro_aggregate.l2_block_number, tdx_aggregate.l2_block_number);
    }

    #[tokio::test]
    async fn dual_proof_sources_preserve_single_platform_failure() {
        let nitro: Arc<dyn ProverClient> = Arc::new(MockProver {
            delay: MOCK_PROVER_DELAY,
            block_interval: SUBMIT_BLOCK_INTERVAL,
        });
        let tdx: Arc<dyn ProverClient> = Arc::new(FailingProver { message: "unavailable" });
        let sources = TeeProofSources::new(nitro, tdx);

        let error = sources
            .prove(signed_proof_request(), TEST_NITRO_IMAGE_HASH, TEST_TDX_IMAGE_HASH)
            .await
            .unwrap_err();

        assert!(matches!(&error, TeeProofError::Platform { platform: TeeProofPlatform::Tdx, .. }));
        assert_eq!(
            error.platform_readiness(),
            [(TeeProofPlatform::Nitro, true), (TeeProofPlatform::Tdx, false)]
        );
    }

    #[tokio::test]
    async fn validate_and_submit_accepts_registered_nitro_and_tdx_signers() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let nitro_signer = PrivateKeySigner::from_slice(&[3u8; 32]).unwrap();
        let tdx_signer = PrivateKeySigner::from_slice(&[4u8; 32]).unwrap();
        let sources =
            recording_sources(Arc::clone(&calls), nitro_signer.clone(), tdx_signer.clone());
        let request = signed_proof_request();
        let proof = sources
            .prove(request.clone(), TEST_NITRO_IMAGE_HASH, TEST_TDX_IMAGE_HASH)
            .await
            .unwrap();
        let readiness = Arc::new(AtomicBool::new(false));
        let pipeline = registry_pipeline(
            MockRegistryL1 {
                latest_block_number: TEST_L1_BLOCK_NUMBER,
                expected_nitro_image_hash: TEST_NITRO_IMAGE_HASH,
                expected_tdx_image_hash: TEST_TDX_IMAGE_HASH,
                signer_expected_image_hashes: HashMap::from([
                    (nitro_signer.address(), TEST_NITRO_IMAGE_HASH),
                    (tdx_signer.address(), TEST_TDX_IMAGE_HASH),
                ]),
                signer_image_hashes: HashMap::from([
                    (nitro_signer.address(), TEST_NITRO_IMAGE_HASH),
                    (tdx_signer.address(), TEST_TDX_IMAGE_HASH),
                ]),
                failing_call: None,
                failing_signer: None,
            },
            Arc::clone(&readiness),
        );

        let result =
            pipeline.validate_and_submit(&proof, SUBMIT_BLOCK_INTERVAL, Address::ZERO).await;

        assert!(result.is_ok());
        assert!(readiness.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn validate_and_submit_retries_when_tdx_signer_validation_errors() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let nitro_signer = PrivateKeySigner::from_slice(&[13u8; 32]).unwrap();
        let tdx_signer = PrivateKeySigner::from_slice(&[14u8; 32]).unwrap();
        let sources =
            recording_sources(Arc::clone(&calls), nitro_signer.clone(), tdx_signer.clone());
        let request = signed_proof_request();
        let proof = sources
            .prove(request.clone(), TEST_NITRO_IMAGE_HASH, TEST_TDX_IMAGE_HASH)
            .await
            .unwrap();
        let readiness = Arc::new(AtomicBool::new(true));
        let pipeline = registry_pipeline(
            MockRegistryL1 {
                latest_block_number: TEST_L1_BLOCK_NUMBER,
                expected_nitro_image_hash: TEST_NITRO_IMAGE_HASH,
                expected_tdx_image_hash: TEST_TDX_IMAGE_HASH,
                signer_expected_image_hashes: HashMap::from([
                    (nitro_signer.address(), TEST_NITRO_IMAGE_HASH),
                    (tdx_signer.address(), TEST_TDX_IMAGE_HASH),
                ]),
                signer_image_hashes: HashMap::from([
                    (nitro_signer.address(), TEST_NITRO_IMAGE_HASH),
                    (tdx_signer.address(), TEST_TDX_IMAGE_HASH),
                ]),
                failing_call: Some(RegistryCall::IsValidSigner),
                failing_signer: Some(tdx_signer.address()),
            },
            Arc::clone(&readiness),
        );

        let result =
            pipeline.validate_and_submit(&proof, SUBMIT_BLOCK_INTERVAL, Address::ZERO).await;

        assert!(matches!(&result, Err(SubmitAction::Failed(_))));
        assert!(!readiness.load(Ordering::SeqCst));
        assert!(result.unwrap_err().to_string().contains("isValidSigner failed"));

        let retry_pipeline = registry_pipeline(
            MockRegistryL1 {
                latest_block_number: TEST_L1_BLOCK_NUMBER,
                expected_nitro_image_hash: TEST_NITRO_IMAGE_HASH,
                expected_tdx_image_hash: TEST_TDX_IMAGE_HASH,
                signer_expected_image_hashes: HashMap::from([
                    (nitro_signer.address(), TEST_NITRO_IMAGE_HASH),
                    (tdx_signer.address(), TEST_TDX_IMAGE_HASH),
                ]),
                signer_image_hashes: HashMap::from([
                    (nitro_signer.address(), TEST_NITRO_IMAGE_HASH),
                    (tdx_signer.address(), TEST_TDX_IMAGE_HASH),
                ]),
                failing_call: None,
                failing_signer: None,
            },
            Arc::clone(&readiness),
        );

        let retry_result =
            retry_pipeline.validate_and_submit(&proof, SUBMIT_BLOCK_INTERVAL, Address::ZERO).await;

        assert!(retry_result.is_ok());
        assert!(readiness.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn validate_and_submit_rejects_unregistered_platform_signer() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let nitro_signer = PrivateKeySigner::from_slice(&[5u8; 32]).unwrap();
        let tdx_signer = PrivateKeySigner::from_slice(&[6u8; 32]).unwrap();
        let sources = recording_sources(calls, nitro_signer, tdx_signer);
        let request = signed_proof_request();
        let proof = sources
            .prove(request.clone(), TEST_NITRO_IMAGE_HASH, TEST_TDX_IMAGE_HASH)
            .await
            .unwrap();
        let readiness = Arc::new(AtomicBool::new(true));
        let pipeline = registry_pipeline(
            MockRegistryL1 {
                latest_block_number: TEST_L1_BLOCK_NUMBER,
                expected_nitro_image_hash: TEST_NITRO_IMAGE_HASH,
                expected_tdx_image_hash: TEST_TDX_IMAGE_HASH,
                signer_expected_image_hashes: HashMap::new(),
                signer_image_hashes: HashMap::new(),
                failing_call: None,
                failing_signer: None,
            },
            Arc::clone(&readiness),
        );

        let result =
            pipeline.validate_and_submit(&proof, SUBMIT_BLOCK_INTERVAL, Address::ZERO).await;

        assert!(matches!(&result, Err(SubmitAction::Discard(_))));
        assert!(!readiness.load(Ordering::SeqCst));
        assert!(result.unwrap_err().to_string().contains("not registered"));
    }

    #[tokio::test]
    async fn validate_and_submit_rejects_registered_signer_with_wrong_image_hash() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let nitro_signer = PrivateKeySigner::from_slice(&[7u8; 32]).unwrap();
        let tdx_signer = PrivateKeySigner::from_slice(&[8u8; 32]).unwrap();
        let sources = recording_sources(calls, nitro_signer.clone(), tdx_signer.clone());
        let request = signed_proof_request();
        let proof = sources
            .prove(request.clone(), TEST_NITRO_IMAGE_HASH, TEST_TDX_IMAGE_HASH)
            .await
            .unwrap();
        let readiness = Arc::new(AtomicBool::new(true));
        let pipeline = registry_pipeline(
            MockRegistryL1 {
                latest_block_number: TEST_L1_BLOCK_NUMBER,
                expected_nitro_image_hash: TEST_NITRO_IMAGE_HASH,
                expected_tdx_image_hash: TEST_TDX_IMAGE_HASH,
                signer_expected_image_hashes: HashMap::from([
                    (nitro_signer.address(), TEST_NITRO_IMAGE_HASH),
                    (tdx_signer.address(), TEST_TDX_IMAGE_HASH),
                ]),
                signer_image_hashes: HashMap::from([
                    (nitro_signer.address(), B256::repeat_byte(0x99)),
                    (tdx_signer.address(), TEST_TDX_IMAGE_HASH),
                ]),
                failing_call: None,
                failing_signer: None,
            },
            Arc::clone(&readiness),
        );

        let result =
            pipeline.validate_and_submit(&proof, SUBMIT_BLOCK_INTERVAL, Address::ZERO).await;

        assert!(matches!(&result, Err(SubmitAction::Discard(_))));
        assert!(!readiness.load(Ordering::SeqCst));
        assert!(result.unwrap_err().to_string().contains("wrong image hash"));
    }

    #[rstest]
    #[case::registered_status(RegistryCall::IsRegisteredSigner)]
    #[case::signer_image_hash(RegistryCall::SignerImageHash)]
    #[case::expected_image_hash(RegistryCall::GetExpectedImageHash)]
    #[tokio::test]
    async fn validate_and_submit_discards_invalid_signer_when_diagnostics_fail(
        #[case] failing_call: RegistryCall,
    ) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let nitro_signer = PrivateKeySigner::from_slice(&[9u8; 32]).unwrap();
        let tdx_signer = PrivateKeySigner::from_slice(&[10u8; 32]).unwrap();
        let sources = recording_sources(calls, nitro_signer.clone(), tdx_signer.clone());
        let request = signed_proof_request();
        let proof = sources
            .prove(request.clone(), TEST_NITRO_IMAGE_HASH, TEST_TDX_IMAGE_HASH)
            .await
            .unwrap();
        let readiness = Arc::new(AtomicBool::new(true));
        let pipeline = registry_pipeline(
            MockRegistryL1 {
                latest_block_number: TEST_L1_BLOCK_NUMBER,
                expected_nitro_image_hash: TEST_NITRO_IMAGE_HASH,
                expected_tdx_image_hash: TEST_TDX_IMAGE_HASH,
                signer_expected_image_hashes: HashMap::from([
                    (nitro_signer.address(), TEST_NITRO_IMAGE_HASH),
                    (tdx_signer.address(), TEST_TDX_IMAGE_HASH),
                ]),
                signer_image_hashes: HashMap::from([
                    (nitro_signer.address(), B256::repeat_byte(0x99)),
                    (tdx_signer.address(), TEST_TDX_IMAGE_HASH),
                ]),
                failing_call: Some(failing_call),
                failing_signer: None,
            },
            Arc::clone(&readiness),
        );

        let result =
            pipeline.validate_and_submit(&proof, SUBMIT_BLOCK_INTERVAL, Address::ZERO).await;

        assert!(matches!(&result, Err(SubmitAction::Discard(_))));
        assert!(!readiness.load(Ordering::SeqCst));
        assert!(result.unwrap_err().to_string().contains("invalid on-chain"));
    }

    #[test]
    fn readiness_fails_after_unavailable_or_stale_dual_proof_cycle() {
        let readiness = Arc::new(AtomicBool::new(true));
        let pipeline = registry_pipeline(
            MockRegistryL1 {
                latest_block_number: TEST_L1_BLOCK_NUMBER,
                expected_nitro_image_hash: TEST_NITRO_IMAGE_HASH,
                expected_tdx_image_hash: TEST_TDX_IMAGE_HASH,
                signer_expected_image_hashes: HashMap::new(),
                signer_image_hashes: HashMap::new(),
                failing_call: None,
                failing_signer: None,
            },
            Arc::clone(&readiness),
        );
        let mut state = PipelineState::new();

        pipeline.handle_proof_result(
            Ok((
                SUBMIT_BLOCK_INTERVAL,
                Err(TeeProofError::Platform {
                    platform: TeeProofPlatform::Tdx,
                    error: ProposerError::Prover("tdx prover unavailable".into()),
                }),
            )),
            &mut state,
        );

        assert!(!readiness.load(Ordering::SeqCst));

        readiness.store(true, Ordering::SeqCst);
        pipeline.handle_proof_result(
            Ok((
                SUBMIT_BLOCK_INTERVAL,
                Err(TeeProofError::PayloadMismatch {
                    error: ProposerError::Prover("nitro and tdx proofs do not match".into()),
                }),
            )),
            &mut state,
        );

        assert!(!readiness.load(Ordering::SeqCst));
    }

    #[test]
    fn readiness_waits_for_signer_validation_after_dual_proof_completion() {
        let readiness = Arc::new(AtomicBool::new(false));
        let pipeline = registry_pipeline(
            MockRegistryL1 {
                latest_block_number: TEST_L1_BLOCK_NUMBER,
                expected_nitro_image_hash: TEST_NITRO_IMAGE_HASH,
                expected_tdx_image_hash: TEST_TDX_IMAGE_HASH,
                signer_expected_image_hashes: HashMap::new(),
                signer_image_hashes: HashMap::new(),
                failing_call: None,
                failing_signer: None,
            },
            Arc::clone(&readiness),
        );
        let mut state = PipelineState::new();

        pipeline.handle_proof_result(
            Ok((SUBMIT_BLOCK_INTERVAL, Ok(dual_proof_result(SUBMIT_BLOCK_INTERVAL)))),
            &mut state,
        );

        assert!(!readiness.load(Ordering::SeqCst));
        assert!(state.proved.contains_key(&SUBMIT_BLOCK_INTERVAL));
    }

    #[tokio::test]
    async fn readiness_fails_after_root_mismatch_submission() {
        let readiness = Arc::new(AtomicBool::new(true));
        let pipeline = registry_pipeline(
            MockRegistryL1 {
                latest_block_number: TEST_L1_BLOCK_NUMBER,
                expected_nitro_image_hash: TEST_NITRO_IMAGE_HASH,
                expected_tdx_image_hash: TEST_TDX_IMAGE_HASH,
                signer_expected_image_hashes: HashMap::new(),
                signer_image_hashes: HashMap::new(),
                failing_call: None,
                failing_signer: None,
            },
            Arc::clone(&readiness),
        );
        let mut state = PipelineState::new();

        let chain_next = pipeline
            .handle_submit_result(
                Ok(SubmitOutcome::RootMismatch { target_block: SUBMIT_BLOCK_INTERVAL }),
                &mut state,
            )
            .await;

        assert!(!chain_next);
        assert!(!readiness.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_validate_and_submit_intermediate_roots_match() {
        // MockRollupClient returns B256::repeat_byte(n) for blocks without
        // explicit entries, which matches test_proposal(n).
        let pipeline = submit_pipeline(HashMap::new());
        let proof_result = dual_proof_result(SUBMIT_BLOCK_INTERVAL);

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
        let proof_result = dual_proof_result(SUBMIT_BLOCK_INTERVAL);

        let result =
            pipeline.validate_and_submit(&proof_result, SUBMIT_BLOCK_INTERVAL, Address::ZERO).await;
        assert!(
            matches!(result, Err(SubmitAction::RootMismatch)),
            "{scenario}: expected RootMismatch, got {result:?}"
        );
    }
}
