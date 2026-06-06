//! Self-driving proving pipeline for the proposer.
//!
//! The [`ProvingPipeline`] runs a single sequential loop: each iteration
//! recovers the latest on-chain tip, derives the next target block, polls the
//! prover service for its deterministic session, and either submits inline,
//! dispatches a fresh request, waits, or treats the result as a transient
//! failure.
//!
//! # Iteration
//!
//! ```text
//! ┌──────────┐     ┌──────────────────┐     ┌────────────────────────┐
//! │ RECOVER  │ ──▶ │ POLL(target)     │ ──▶ │ Ready    → submit      │
//! │ (cached) │     │ (deterministic)  │     │ NotFound → dispatch    │
//! └──────────┘     └──────────────────┘     │ Pending  → wait        │
//!                                           │ Failed   → retry/drop  │
//!                                           │ Unknown  → wait        │
//!                                           └────────────────────────┘
//! ```
//!
//! There is no in-memory queue of dispatched-but-not-yet-collected sessions:
//! the prover service is the source of truth and the collector rederives
//! sessions from canonical output roots. State that survives across
//! iterations is limited to the recovery cache and a per-target retry
//! counter, both passed by reference into [`ProvingPipeline::step`].
//!
//! On success the loop advances by exactly one `block_interval` per iteration;
//! on `Pending` / `Unknown` it sleeps for `poll_interval` and retries. Submit
//! is wrapped in [`tokio::time::timeout`] so a stuck L1 RPC never blocks
//! progress beyond `submit_timeout`.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use alloy_primitives::{Address, B256};
use base_proof_contracts::{
    AggregateVerifierClient, AnchorStateRegistryClient, DisputeGameFactoryClient, encode_extra_data,
};
use base_proof_primitives::{ProofRequest, ProofResult};
use base_proof_rpc::{L1Provider, L2Provider, RollupProvider, RpcError};
use base_prover_service_client::ProofRequesterProvider;
use eyre::Result;
use futures::{StreamExt, stream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};

use crate::{
    Metrics,
    driver::{DriverConfig, RecoveredState},
    error::ProposerError,
    output_proposer::OutputProposer,
    proof_adapter::ProofRequesterDispatcher,
    proof_collector::{ProofCollector, TargetPoll},
    proof_submitter::{ProofSubmitter, ProofSubmitterConfig, SubmitAction},
};

/// Configuration for the self-driving proving pipeline.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Maximum retries for a single target block before dropping the cached
    /// recovery. Only proof failures and dispatch RPC errors count against
    /// this budget; transient submit and poll errors do not.
    pub max_retries: u32,
    /// Maximum number of concurrent RPC calls during the recovery scan.
    pub recovery_scan_concurrency: usize,
    /// Maximum duration for a single inline submit (validation + L1
    /// transaction). When exceeded, the loop logs and continues to the next
    /// iteration without counting against the retry budget.
    pub submit_timeout: Duration,
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
/// - No cache exists (cold start, or invalidated by a submit `RootMismatch`
///   or a target hitting `max_retries`).
/// - The anchor advanced past the cached tip (governance intervention).
/// - `game_count` decreased (L1 reorg removed games).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CachedRecovery {
    /// Factory `game_count` at the time of the last walk.
    game_count: u64,
    /// The recovered on-chain state from the walk.
    state: RecoveredState,
}

/// The self-driving proving pipeline.
///
/// Runs a single sequential loop per [`Self::run`] call. Each iteration is
/// independent and re-derives all required state from on-chain reads plus
/// deterministic prover-service session lookups; the only state that
/// survives across iterations is the recovery cache and per-target retry
/// counts, both passed by reference into [`Self::step`].
pub struct ProvingPipeline<L1, L2, R, ASR, F>
where
    L1: L1Provider,
    L2: L2Provider,
    R: RollupProvider,
    ASR: AnchorStateRegistryClient,
    F: DisputeGameFactoryClient,
{
    config: PipelineConfig,
    proof_requester: Arc<dyn ProofRequesterProvider>,
    proof_dispatcher: ProofRequesterDispatcher,
    proof_collector: ProofCollector<R>,
    proof_submitter: ProofSubmitter<L1, R>,
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
            proof_requester: Arc::clone(&self.proof_requester),
            proof_dispatcher: self.proof_dispatcher.clone(),
            proof_collector: self.proof_collector.clone(),
            proof_submitter: self.proof_submitter.clone(),
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
    /// Creates a new self-driving proving pipeline.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: PipelineConfig,
        proof_requester: Arc<dyn ProofRequesterProvider>,
        l1_client: Arc<L1>,
        l2_client: Arc<L2>,
        rollup_client: Arc<R>,
        anchor_registry: Arc<ASR>,
        factory_client: Arc<F>,
        verifier_client: Arc<dyn AggregateVerifierClient>,
        output_proposer: Arc<dyn OutputProposer>,
        cancel: CancellationToken,
    ) -> Self {
        let proof_collector =
            ProofCollector::aws_nitro(Arc::clone(&proof_requester), Arc::clone(&rollup_client));
        let proof_submitter = ProofSubmitter::new(
            Arc::clone(&output_proposer),
            Arc::clone(&rollup_client),
            Arc::clone(&l1_client),
            ProofSubmitterConfig {
                proposer_address: config.driver.proposer_address,
                block_interval: config.driver.block_interval,
                intermediate_block_interval: config.driver.intermediate_block_interval,
                tee_image_hash: config.driver.tee_image_hash,
                tee_prover_registry_address: config.tee_prover_registry_address,
                output_fetch_concurrency: config.recovery_scan_concurrency,
            },
        );

        Self {
            config,
            proof_requester: Arc::clone(&proof_requester),
            proof_dispatcher: ProofRequesterDispatcher::aws_nitro(proof_requester),
            proof_collector,
            proof_submitter,
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

    /// Runs the self-driving proving pipeline until cancelled.
    ///
    /// Each iteration recovers the on-chain tip, derives the next target,
    /// polls the prover service, and acts on the [`TargetPoll`] outcome by
    /// submitting inline, dispatching a fresh request, waiting, or applying
    /// the retry policy. Sleeps for `poll_interval` between iterations.
    /// Cancellation is honored at every `await`.
    pub async fn run(&self) -> Result<()> {
        info!(
            block_interval = self.config.driver.block_interval,
            poll_interval_secs = self.config.driver.poll_interval.as_secs(),
            submit_timeout_secs = self.config.submit_timeout.as_secs(),
            "Starting self-driving proving pipeline"
        );

        let mut cache: Option<CachedRecovery> = None;
        let mut retry_counts: HashMap<u64, u32> = HashMap::new();
        // Targets the submitter has irrecoverably discarded (e.g.
        // `L1OriginTooOld`, `InvalidSigner`). The prover-service session for
        // a discarded target is `Succeeded` with a deterministic id, so a
        // naive next-iteration poll would re-deliver the same `Ready` proof
        // and re-discard it indefinitely. Tracking discarded targets in
        // memory lets us short-circuit polling until the chain advances past
        // them; the set is cleared on restart, matching the implicit
        // skip-via-dropped-`proved`-map behavior of the previous parallel
        // pipeline.
        let mut discarded_targets: HashSet<u64> = HashSet::new();

        loop {
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => break,
                () = self.step(&mut cache, &mut retry_counts, &mut discarded_targets) => {}
            }

            tokio::select! {
                biased;
                () = self.cancel.cancelled() => break,
                () = tokio::time::sleep(self.config.driver.poll_interval) => {}
            }
        }

        info!("Self-driving proving pipeline stopped");
        Ok(())
    }

    /// Executes a single iteration of the self-driving loop.
    ///
    /// Recovers the on-chain tip, derives the next target block, polls the
    /// prover service, and acts on the [`TargetPoll`] outcome.
    #[instrument(skip_all)]
    async fn step(
        &self,
        cache: &mut Option<CachedRecovery>,
        retry_counts: &mut HashMap<u64, u32>,
        discarded_targets: &mut HashSet<u64>,
    ) {
        let _tick_timer = base_metrics::timed!(Metrics::tick_duration_seconds());

        let (recovered, safe_head) = match self.try_recover_and_plan(cache).await {
            Some(pair) => pair,
            None => {
                Metrics::pipeline_retries().set(retry_counts.values().sum::<u32>() as f64);
                return;
            }
        };

        Metrics::safe_head().set(safe_head as f64);
        // Reflect the on-chain proposer tip on every iteration. Without this,
        // dashboards that alert on proposer lag would see the gauge stuck at
        // its last submit value (or unset on cold start) until the next
        // successful inline submit, even if other proposers or a previous
        // run had advanced the chain.
        Metrics::last_proposed_block().set(recovered.l2_block_number as f64);

        // Drop retry counters and discarded markers for targets the chain
        // has already passed.
        retry_counts.retain(|&target, _| target > recovered.l2_block_number);
        discarded_targets.retain(|&target| target > recovered.l2_block_number);

        let target_block =
            match recovered.l2_block_number.checked_add(self.config.driver.block_interval) {
                Some(t) => t,
                None => {
                    error!(
                        recovered_block = recovered.l2_block_number,
                        block_interval = self.config.driver.block_interval,
                        "Overflow computing next target block, halting iteration"
                    );
                    return;
                }
            };

        if target_block > safe_head {
            debug!(
                recovered_block = recovered.l2_block_number,
                target_block,
                safe_head,
                "Safe head below next target, waiting for L2 head to advance"
            );
            Metrics::pipeline_retries().set(retry_counts.values().sum::<u32>() as f64);
            return;
        }

        // Skip targets the submitter has already discarded. The
        // prover-service session for a discarded target is deterministic
        // and sticky in `Succeeded`, so polling again would re-deliver the
        // same `Ready` proof and re-discard it. The discarded marker is
        // cleared above once the on-chain tip advances past `target_block`.
        if discarded_targets.contains(&target_block) {
            debug!(
                target_block,
                "Target previously discarded by submitter, waiting for chain to advance"
            );
            return;
        }

        match self.proof_collector.poll(target_block).await {
            TargetPoll::Ready { session_id, proof } => {
                info!(
                    target_block,
                    session_id = %session_id,
                    "Proof ready, submitting inline"
                );
                Metrics::proof_collection_total(Metrics::COLLECTION_OUTCOME_READY).increment(1);
                self.submit_inline(
                    target_block,
                    &recovered,
                    proof,
                    retry_counts,
                    cache,
                    discarded_targets,
                )
                .await;
            }
            TargetPoll::Pending { session_id, status } => {
                debug!(
                    target_block,
                    session_id = %session_id,
                    ?status,
                    "Proof pending, waiting for prover service"
                );
            }
            TargetPoll::NotFound { session_id, claimed_l2_output_root } => {
                info!(
                    target_block,
                    session_id = %session_id,
                    "No prover-service session for target, dispatching"
                );
                self.dispatch_for(
                    target_block,
                    &recovered,
                    claimed_l2_output_root,
                    retry_counts,
                    cache,
                )
                .await;
            }
            TargetPoll::Failed { session_id, error } => {
                warn!(
                    target_block,
                    session_id = %session_id,
                    error = %error,
                    "Prover service reported failed session"
                );
                Metrics::proof_collection_total(Metrics::COLLECTION_OUTCOME_FAILED).increment(1);
                self.handle_proof_failure(target_block, error, retry_counts, cache);
            }
            TargetPoll::Unknown { session_id, error } => {
                debug!(
                    target_block,
                    session_id = ?session_id,
                    error = %error,
                    "Transient poll failure, will retry next iteration"
                );
            }
        }

        Metrics::pipeline_retries().set(retry_counts.values().sum::<u32>() as f64);
    }

    /// Validates and submits the proof inline against the `submit_timeout`
    /// budget.
    ///
    /// On success, advances `last_proposed_block` and `last_collected_block`,
    /// drops the per-target retry counter, and refreshes the recovery cache
    /// incrementally. Submit failures are transient by default — they do not
    /// count against the per-target retry budget — except `RootMismatch` and
    /// `Failed { is_invalid_parent_game }`, which drop the cached recovery
    /// so the next iteration re-walks the chain.
    async fn submit_inline(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        proof: ProofResult,
        retry_counts: &mut HashMap<u64, u32>,
        cache: &mut Option<CachedRecovery>,
        discarded_targets: &mut HashSet<u64>,
    ) {
        let parent_address = recovered.parent_address;
        info!(target_block, parent_address = %parent_address, "Submitting proof inline");

        let mut submit_timer = base_metrics::timed!(Metrics::proposal_total_duration_seconds());
        let result = tokio::time::timeout(
            self.config.submit_timeout,
            self.validate_and_submit(&proof, target_block, parent_address),
        )
        .await;

        match result {
            Err(_) => {
                submit_timer.disarm();
                warn!(
                    target_block,
                    timeout_secs = self.config.submit_timeout.as_secs(),
                    "Inline submit timed out, will retry next iteration"
                );
            }
            Ok(Ok(())) => {
                drop(submit_timer);
                info!(target_block, "Submission successful");
                Metrics::last_proposed_block().set(target_block as f64);
                Metrics::last_collected_block().set(target_block as f64);
                retry_counts.remove(&target_block);
                if let Err(e) = self.recover_latest_state(cache).await {
                    warn!(error = %e, "Failed to recover state after submission");
                }
            }
            Ok(Err(SubmitAction::RootMismatch)) => {
                submit_timer.disarm();
                warn!(target_block, "Output root mismatch at submit time, dropping recovery cache");
                Metrics::root_mismatch_total().increment(1);
                *cache = None;
            }
            Ok(Err(SubmitAction::GameAlreadyExists)) => {
                submit_timer.disarm();
                info!(target_block, "Game already exists on chain");
                Metrics::last_proposed_block().set(target_block as f64);
                Metrics::last_collected_block().set(target_block as f64);
                retry_counts.remove(&target_block);
                // The game exists but the forward walk missed it — most
                // likely because `game_count` was read from a different L1
                // RPC replica than the one serving `factory.games()`.
                // Decrement the cached game_count so the next recovery sees
                // `actual_count > cached_count` and performs an incremental
                // forward walk.
                if let Some(cached) = cache.as_mut() {
                    cached.game_count = cached.game_count.saturating_sub(1);
                }
                if let Err(e) = self.recover_latest_state(cache).await {
                    warn!(error = %e, "Failed to recover state after GameAlreadyExists");
                }
            }
            Ok(Err(SubmitAction::Failed(error))) => {
                submit_timer.disarm();
                Metrics::errors_total(error.metric_label()).increment(1);
                if error.is_invalid_parent_game() {
                    warn!(
                        target_block,
                        error = %error,
                        "Submission rejected: parent game invalid, dropping recovery cache"
                    );
                    *cache = None;
                } else {
                    warn!(
                        target_block,
                        error = %error,
                        "Submission failed, will retry next iteration"
                    );
                }
            }
            Ok(Err(SubmitAction::Discard(error))) => {
                submit_timer.disarm();
                Metrics::errors_total(error.metric_label()).increment(1);
                Metrics::discarded_targets_total().increment(1);
                // The prover-service session for this target is keyed
                // deterministically on the canonical output root, so the
                // next poll would return the same `Ready` proof and
                // re-discard it. Mark the target as discarded so subsequent
                // iterations skip polling until the chain advances past it.
                discarded_targets.insert(target_block);
                warn!(
                    target_block,
                    error = %error,
                    "Proof discarded by submitter, skipping until chain advances past target"
                );
            }
        }
    }

    /// Builds and dispatches a fresh `prove_block_range` request for
    /// `target_block`.
    ///
    /// Request-build failures (transient L1/L2 RPC errors while assembling
    /// the request) are logged and skipped without bumping the per-target
    /// retry budget — they never reached the prover service, so the
    /// proof-failure retry policy does not apply. Dispatcher errors (the
    /// prover service rejected an otherwise valid request) flow through
    /// [`Self::handle_proof_failure`] and do count against the budget.
    async fn dispatch_for(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        claimed_l2_output_root: B256,
        retry_counts: &mut HashMap<u64, u32>,
        cache: &mut Option<CachedRecovery>,
    ) {
        let request = match self
            .build_proof_request_for(target_block, recovered, claimed_l2_output_root)
            .await
        {
            Ok(req) => req,
            Err(e) => {
                warn!(
                    target_block,
                    error = %e,
                    "Failed to build proof request, will retry next iteration"
                );
                Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_BUILD_FAILED).increment(1);
                return;
            }
        };

        match self.proof_dispatcher.dispatch_tee(request).await {
            Ok(dispatched) => {
                info!(
                    target_block,
                    session_id = %dispatched.session_id,
                    from_block = recovered.l2_block_number,
                    "Proof request accepted by prover service"
                );
                Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_ACCEPTED).increment(1);
            }
            Err(error) => {
                Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_FAILED).increment(1);
                self.handle_proof_failure(target_block, error, retry_counts, cache);
            }
        }
    }

    /// Records a proof failure for `target` and applies the retry policy.
    ///
    /// Increments `proof_retries_total` and the per-target counter. When the
    /// counter reaches `max_retries`, drops the cached recovery so the next
    /// iteration performs a full forward walk.
    fn handle_proof_failure(
        &self,
        target: u64,
        error: ProposerError,
        retry_counts: &mut HashMap<u64, u32>,
        cache: &mut Option<CachedRecovery>,
    ) {
        Metrics::errors_total(error.metric_label()).increment(1);
        Metrics::proof_retries_total().increment(1);

        let count = retry_counts.entry(target).or_insert(0);
        *count += 1;
        if *count >= self.config.max_retries {
            error!(
                target_block = target,
                attempts = *count,
                error = %error,
                "Proof failed after max retries, dropping cached recovery"
            );
            retry_counts.remove(&target);
            *cache = None;
        } else {
            warn!(
                target_block = target,
                attempt = *count,
                error = %error,
                "Proof failed, will retry next iteration"
            );
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
        if let Some(cached) = cache.as_ref() {
            let safe_head = match self.latest_safe_block_number().await {
                Ok(n) => n,
                Err(e) => {
                    warn!(error = %e, "Failed to fetch safe head, retrying next tick");
                    return None;
                }
            };

            let next_proposal_block =
                match cached.state.l2_block_number.checked_add(self.config.driver.block_interval) {
                    Some(block) => block,
                    None => {
                        warn!(
                            cached_block = cached.state.l2_block_number,
                            block_interval = self.config.driver.block_interval,
                            "Cannot compute next proposal block, retrying next tick"
                        );
                        return None;
                    }
                };

            if safe_head < next_proposal_block {
                debug!(
                    safe_head,
                    cached_block = cached.state.l2_block_number,
                    next_proposal_block,
                    "Safe head below next proposal target, skipping recovery"
                );
                return Some((cached.state, safe_head));
            }

            let state = match self.recover_latest_state(cache).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "Failed to recover on-chain state, retrying next tick");
                    return None;
                }
            };

            return Some((state, safe_head));
        }

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
    ///    once the safe head is high enough to need recovery.
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

        // Read the anchor root and anchor game from one L1 snapshot so
        // recovery cannot combine an old root with a newer anchor game.
        let anchor_snapshot = self
            .anchor_registry
            .anchor_snapshot()
            .await
            .map_err(|e| ProposerError::Contract(format!("anchor_snapshot failed: {e}")))?;
        let anchor = anchor_snapshot.anchor_root;

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
        // - No cache exists (cold start, or invalidated by RootMismatch /
        //   max_retries).
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
            _ => {
                let parent_address = if anchor_snapshot.anchor_game.is_zero() {
                    self.config.driver.anchor_state_registry_address
                } else {
                    anchor_snapshot.anchor_game
                };

                RecoveredState {
                    parent_address,
                    output_root: anchor.root,
                    l2_block_number: anchor.l2_block_number,
                }
            }
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
            let intermediate_blocks =
                self.proof_submitter.intermediate_block_numbers(parent_block)?;
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
        blocks: Vec<u64>,
    ) -> Result<HashMap<u64, B256>, ProposerError> {
        self.fetch_canonical_root_results(blocks)
            .await
            .into_iter()
            .map(|(block_number, result)| result.map(|root| (block_number, root)))
            .collect()
    }

    async fn fetch_canonical_root_results(
        &self,
        blocks: Vec<u64>,
    ) -> HashMap<u64, Result<B256, ProposerError>> {
        if blocks.is_empty() {
            return HashMap::new();
        }
        stream::iter(blocks)
            .map(|block_number| {
                let rollup = &self.rollup_client;
                async move {
                    let result = rollup
                        .output_at_block(block_number)
                        .await
                        .map(|out| out.output_root)
                        .map_err(ProposerError::Rpc);
                    (block_number, result)
                }
            })
            .buffered(self.config.recovery_scan_concurrency)
            .collect()
            .await
    }

    /// Builds a proof request for a single `target_block`, parallelising the
    /// two required RPCs: L1 head header and L2 head at the recovered tip.
    /// The canonical output root for `target_block` is supplied by the
    /// caller (already fetched while deriving the prover-service session id
    /// in [`crate::proof_collector::ProofCollector::poll`]).
    async fn build_proof_request_for(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        claimed_l2_output_root: B256,
    ) -> Result<ProofRequest, ProposerError> {
        let (l1_head_result, agreed_head_result) = tokio::join!(
            async { self.l1_client.header_by_number(None).await.map_err(ProposerError::Rpc) },
            async {
                self.l2_client
                    .header_by_number(Some(recovered.l2_block_number))
                    .await
                    .map_err(ProposerError::Rpc)
            },
        );

        let l1_head = l1_head_result?;
        let agreed_l2_head = agreed_head_result?;

        let request = ProofRequest {
            l1_head: l1_head.hash,
            agreed_l2_head_hash: agreed_l2_head.hash,
            agreed_l2_output_root: recovered.output_root,
            claimed_l2_output_root,
            claimed_l2_block_number: target_block,
            proposer: self.config.driver.proposer_address,
            intermediate_block_interval: self.config.driver.intermediate_block_interval,
            l1_head_number: l1_head.number,
            image_hash: self.config.driver.tee_image_hash,
        };

        info!(
            from_block = recovered.l2_block_number,
            to_block = target_block,
            l1_head_number = l1_head.number,
            "Built proof request"
        );

        Ok(request)
    }

    /// Validates the proof and submits it to L1 by delegating to the
    /// [`ProofSubmitter`].
    ///
    /// Kept on the pipeline as a thin wrapper so the inline submit path in
    /// [`Self::submit_inline`] (and existing tests) can continue to call a
    /// single entry point. This method itself does NOT apply
    /// `submit_timeout`; the timeout is applied by [`Self::submit_inline`].
    async fn validate_and_submit(
        &self,
        proof_result: &ProofResult,
        target_block: u64,
        parent_address: Address,
    ) -> Result<(), SubmitAction> {
        self.proof_submitter.submit(proof_result, target_block, parent_address).await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        sync::Arc,
        time::Duration,
    };

    use alloy_primitives::{Address, B256};
    use async_trait::async_trait;
    use base_proof_primitives::{ProofResult, Proposal};
    #[cfg(feature = "metrics")]
    use metrics_util::{
        CompositeKey, MetricKind,
        debugging::{DebugValue, DebuggingRecorder, Snapshotter},
    };
    use rstest::rstest;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
        MockOutputProposer, MockProofRequester, MockRollupClient, test_anchor_root, test_proposal,
        test_sync_status,
    };

    // ---- Named constants for test data ----

    #[cfg(feature = "metrics")]
    type SnapEntry =
        (CompositeKey, Option<metrics::Unit>, Option<metrics::SharedString>, DebugValue);

    #[cfg(feature = "metrics")]
    fn with_recorder(f: impl FnOnce(Snapshotter)) {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || f(snapshotter));
    }

    #[cfg(feature = "metrics")]
    fn find_metric<'a>(
        snap: &'a [SnapEntry],
        kind: MetricKind,
        name: &str,
    ) -> Option<&'a DebugValue> {
        snap.iter()
            .find(|(ck, _, _, _)| ck.kind() == kind && ck.key().name() == name)
            .map(|(_, _, _, v)| v)
    }

    /// Game type used across recovery tests.
    const TEST_GAME_TYPE: u32 = 42;

    /// Default block interval for recovery tests (matches `DriverConfig` default).
    const TEST_BLOCK_INTERVAL: u64 = 512;

    /// Default anchor block number.
    const TEST_ANCHOR_BLOCK: u64 = 0;

    /// Default L1 block number returned by `MockL1`.
    const TEST_L1_BLOCK_NUMBER: u64 = 1000;

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
            game_count_calls: None,
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

    #[derive(Debug)]
    struct SnapshotOnlyAnchorStateRegistry {
        snapshot: base_proof_contracts::AnchorSnapshot,
    }

    #[async_trait::async_trait]
    impl AnchorStateRegistryClient for SnapshotOnlyAnchorStateRegistry {
        async fn anchor_snapshot(
            &self,
        ) -> std::result::Result<
            base_proof_contracts::AnchorSnapshot,
            base_proof_contracts::ContractError,
        > {
            Ok(self.snapshot)
        }
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
        recovery_pipeline_full_with_output_proposer(
            factory,
            output_roots,
            anchor_block,
            block_interval,
            intermediate_block_interval,
            Arc::new(MockOutputProposer),
        )
    }

    fn recovery_pipeline_full_with_output_proposer(
        factory: MockDisputeGameFactory,
        output_roots: HashMap<u64, B256>,
        anchor_block: u64,
        block_interval: u64,
        intermediate_block_interval: u64,
        output_proposer: Arc<dyn OutputProposer>,
    ) -> TestPipeline {
        recovery_pipeline_full_with_anchor_game_and_output_proposer(
            factory,
            output_roots,
            anchor_block,
            Address::ZERO,
            block_interval,
            intermediate_block_interval,
            output_proposer,
        )
    }

    fn recovery_pipeline_full_with_anchor_game(
        factory: MockDisputeGameFactory,
        output_roots: HashMap<u64, B256>,
        anchor_block: u64,
        anchor_game: Address,
        block_interval: u64,
        intermediate_block_interval: u64,
    ) -> TestPipeline {
        recovery_pipeline_full_with_anchor_game_and_output_proposer(
            factory,
            output_roots,
            anchor_block,
            anchor_game,
            block_interval,
            intermediate_block_interval,
            Arc::new(MockOutputProposer),
        )
    }

    fn recovery_pipeline_full_with_anchor_game_and_output_proposer(
        factory: MockDisputeGameFactory,
        output_roots: HashMap<u64, B256>,
        anchor_block: u64,
        anchor_game: Address,
        block_interval: u64,
        intermediate_block_interval: u64,
        output_proposer: Arc<dyn OutputProposer>,
    ) -> TestPipeline {
        let cancel = CancellationToken::new();
        let l1 = Arc::new(MockL1 { latest_block_number: TEST_L1_BLOCK_NUMBER });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(0, B256::ZERO),
            output_roots,
            max_safe_block: None,
        });
        let anchor_registry = Arc::new(MockAnchorStateRegistry {
            anchor_root: test_anchor_root(anchor_block),
            anchor_game,
        });

        ProvingPipeline::new(
            PipelineConfig {
                submit_timeout: std::time::Duration::from_secs(60),
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
            Arc::new(MockProofRequester::default()),
            l1,
            l2,
            rollup,
            anchor_registry,
            Arc::new(factory),
            Arc::new(MockAggregateVerifier::default()),
            output_proposer,
            cancel,
        )
    }

    // ---- Pipeline lifecycle tests ----

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

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cold_start_uses_anchor_game_after_anchor_advance() {
        let anchor_game = proxy_addr(0);
        let anchor_block = TEST_BLOCK_INTERVAL;

        let mut factory = MockDisputeGameFactory::with_games(vec![]);
        factory.game_count_override = Some(1);
        let pipeline = recovery_pipeline_full_with_anchor_game(
            factory,
            HashMap::new(),
            anchor_block,
            anchor_game,
            TEST_BLOCK_INTERVAL,
            TEST_BLOCK_INTERVAL,
        );

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.parent_address, anchor_game, "advanced anchor game should be the parent");
        assert_eq!(state.l2_block_number, anchor_block, "should propose after the live anchor");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_reads_anchor_root_and_game_from_one_snapshot() {
        let anchor_game = proxy_addr(0);
        let anchor_root = B256::repeat_byte(0xAA);
        let anchor_block = TEST_BLOCK_INTERVAL;
        let cancel = CancellationToken::new();
        let l1 = Arc::new(MockL1 { latest_block_number: TEST_L1_BLOCK_NUMBER });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(TEST_BLOCK_INTERVAL * 2, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let anchor_registry = Arc::new(SnapshotOnlyAnchorStateRegistry {
            snapshot: base_proof_contracts::AnchorSnapshot {
                anchor_root: base_proof_contracts::AnchorRoot {
                    root: anchor_root,
                    l2_block_number: anchor_block,
                },
                anchor_game,
            },
        });
        let mut factory = MockDisputeGameFactory::with_games(vec![]);
        factory.game_count_override = Some(1);

        let pipeline = ProvingPipeline::new(
            PipelineConfig {
                submit_timeout: std::time::Duration::from_secs(60),
                max_retries: 3,
                recovery_scan_concurrency: 8,
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    block_interval: TEST_BLOCK_INTERVAL,
                    intermediate_block_interval: TEST_BLOCK_INTERVAL,
                    ..Default::default()
                },
            },
            Arc::new(MockProofRequester::default()),
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

        assert_eq!(state.parent_address, anchor_game);
        assert_eq!(state.output_root, anchor_root);
        assert_eq!(state.l2_block_number, anchor_block);
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
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(0, B256::ZERO),
            output_roots,
            max_safe_block: Some(TEST_BLOCK_INTERVAL * 2),
        });
        let anchor_registry = Arc::new(MockAnchorStateRegistry {
            anchor_root: test_anchor_root(TEST_ANCHOR_BLOCK),
            anchor_game: Address::ZERO,
        });

        let pipeline = ProvingPipeline::new(
            PipelineConfig {
                submit_timeout: std::time::Duration::from_secs(60),
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
            Arc::new(MockProofRequester::default()),
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

    #[derive(Debug)]
    struct DelayedOutputProposer {
        delay: Duration,
    }

    #[async_trait]
    impl OutputProposer for DelayedOutputProposer {
        async fn propose_output(
            &self,
            _proposal: &Proposal,
            _parent_address: Address,
            _intermediate_roots: &[B256],
        ) -> Result<(), ProposerError> {
            tokio::time::sleep(self.delay).await;
            Ok(())
        }
    }

    #[derive(Debug)]
    struct L1OriginTooOldOutputProposer;

    #[async_trait]
    impl OutputProposer for L1OriginTooOldOutputProposer {
        async fn propose_output(
            &self,
            _proposal: &Proposal,
            _parent_address: Address,
            _intermediate_roots: &[B256],
        ) -> Result<(), ProposerError> {
            Err(ProposerError::L1OriginTooOld)
        }
    }

    #[derive(Debug)]
    struct InvalidSignerOutputProposer;

    #[async_trait]
    impl OutputProposer for InvalidSignerOutputProposer {
        async fn propose_output(
            &self,
            _proposal: &Proposal,
            _parent_address: Address,
            _intermediate_roots: &[B256],
        ) -> Result<(), ProposerError> {
            Err(ProposerError::InvalidSigner)
        }
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

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_validate_and_submit_does_not_apply_outer_timeout() {
        let pipeline = recovery_pipeline_full_with_output_proposer(
            MockDisputeGameFactory::with_games(vec![]),
            HashMap::new(),
            TEST_ANCHOR_BLOCK,
            SUBMIT_BLOCK_INTERVAL,
            SUBMIT_INTERMEDIATE_INTERVAL,
            Arc::new(DelayedOutputProposer {
                delay: crate::constants::PROPOSAL_TIMEOUT + Duration::from_secs(1),
            }),
        );
        let proof_result = submit_proof_result(SUBMIT_BLOCK_INTERVAL);

        let result =
            pipeline.validate_and_submit(&proof_result, SUBMIT_BLOCK_INTERVAL, Address::ZERO).await;

        assert!(
            result.is_ok(),
            "submission should rely on tx-manager timeout, not an outer timeout"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_validate_and_submit_discards_l1_origin_too_old() {
        let pipeline = recovery_pipeline_full_with_output_proposer(
            MockDisputeGameFactory::with_games(vec![]),
            HashMap::new(),
            TEST_ANCHOR_BLOCK,
            SUBMIT_BLOCK_INTERVAL,
            SUBMIT_INTERMEDIATE_INTERVAL,
            Arc::new(L1OriginTooOldOutputProposer),
        );
        let proof_result = submit_proof_result(SUBMIT_BLOCK_INTERVAL);

        let result =
            pipeline.validate_and_submit(&proof_result, SUBMIT_BLOCK_INTERVAL, Address::ZERO).await;

        assert!(
            matches!(result, Err(SubmitAction::Discard(ProposerError::L1OriginTooOld))),
            "stale L1 origin should discard the proof, got {result:?}"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_validate_and_submit_discards_invalid_signer() {
        let pipeline = recovery_pipeline_full_with_output_proposer(
            MockDisputeGameFactory::with_games(vec![]),
            HashMap::new(),
            TEST_ANCHOR_BLOCK,
            SUBMIT_BLOCK_INTERVAL,
            SUBMIT_INTERMEDIATE_INTERVAL,
            Arc::new(InvalidSignerOutputProposer),
        );
        let proof_result = submit_proof_result(SUBMIT_BLOCK_INTERVAL);

        let result =
            pipeline.validate_and_submit(&proof_result, SUBMIT_BLOCK_INTERVAL, Address::ZERO).await;

        assert!(
            matches!(result, Err(SubmitAction::Discard(ProposerError::InvalidSigner))),
            "invalid signer should discard the proof, got {result:?}"
        );
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

    // ---- Self-driving loop: step / submit_inline / handle_proof_failure ----

    /// Builds a pipeline tailored for `step()` / `submit_inline()` tests.
    ///
    /// Uses `SUBMIT_BLOCK_INTERVAL` for short cycles and exposes the
    /// underlying [`MockProofRequester`] so tests can pre-seed the
    /// prover-service stub or assert on its post-state. Also returns the
    /// `CancellationToken` so tests covering `run()` can stop the loop.
    fn step_pipeline_full(
        output_roots: HashMap<u64, B256>,
        safe_head_block: u64,
        max_retries: u32,
        submit_timeout: Duration,
        output_proposer: Arc<dyn OutputProposer>,
    ) -> (TestPipeline, Arc<MockProofRequester>, CancellationToken) {
        let proof_requester = Arc::new(MockProofRequester::default());
        let cancel = CancellationToken::new();
        let l1 = Arc::new(MockL1 { latest_block_number: TEST_L1_BLOCK_NUMBER });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(safe_head_block, B256::ZERO),
            output_roots,
            max_safe_block: None,
        });
        let anchor_registry = Arc::new(MockAnchorStateRegistry {
            anchor_root: test_anchor_root(TEST_ANCHOR_BLOCK),
            anchor_game: Address::ZERO,
        });

        let pipeline = ProvingPipeline::new(
            PipelineConfig {
                submit_timeout,
                max_retries,
                recovery_scan_concurrency: 8,
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    game_type: TEST_GAME_TYPE,
                    block_interval: SUBMIT_BLOCK_INTERVAL,
                    intermediate_block_interval: SUBMIT_INTERMEDIATE_INTERVAL,
                    poll_interval: Duration::from_millis(10),
                    ..Default::default()
                },
            },
            Arc::clone(&proof_requester) as Arc<dyn ProofRequesterProvider>,
            l1,
            l2,
            rollup,
            anchor_registry,
            Arc::new(MockDisputeGameFactory::with_games(vec![])),
            Arc::new(MockAggregateVerifier::default()),
            output_proposer,
            cancel.clone(),
        );

        (pipeline, proof_requester, cancel)
    }

    fn step_pipeline_default(
        safe_head_block: u64,
    ) -> (TestPipeline, Arc<MockProofRequester>, CancellationToken) {
        step_pipeline_full(
            HashMap::new(),
            safe_head_block,
            3,
            Duration::from_secs(60),
            Arc::new(MockOutputProposer),
        )
    }

    fn anchor_recovered_state() -> RecoveredState {
        RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::ZERO,
            l2_block_number: TEST_ANCHOR_BLOCK,
        }
    }

    /// Output proposer that always rejects with `InvalidParentGame`.
    #[derive(Debug)]
    struct InvalidParentGameOutputProposer;

    #[async_trait]
    impl OutputProposer for InvalidParentGameOutputProposer {
        async fn propose_output(
            &self,
            _: &Proposal,
            _: Address,
            _: &[B256],
        ) -> Result<(), ProposerError> {
            Err(ProposerError::InvalidParentGame)
        }
    }

    /// Output proposer that always rejects with a transient internal error.
    #[derive(Debug)]
    struct TransientFailOutputProposer;

    #[async_trait]
    impl OutputProposer for TransientFailOutputProposer {
        async fn propose_output(
            &self,
            _: &Proposal,
            _: Address,
            _: &[B256],
        ) -> Result<(), ProposerError> {
            Err(ProposerError::Internal("simulated transient failure".into()))
        }
    }

    /// `handle_proof_failure` increments per-target counters and drops the
    /// cached recovery once the target reaches `max_retries`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_handle_proof_failure_drops_cache_at_max_retries() {
        let (pipeline, _proof_requester, _cancel) = step_pipeline_full(
            HashMap::new(),
            0,
            3,
            Duration::from_secs(60),
            Arc::new(MockOutputProposer),
        );

        let mut retry_counts: HashMap<u64, u32> = HashMap::new();
        let mut cache = Some(CachedRecovery { game_count: 0, state: anchor_recovered_state() });

        // First two failures: counter increments, cache is preserved.
        for attempt in 1..=2u32 {
            pipeline.handle_proof_failure(
                SUBMIT_BLOCK_INTERVAL,
                ProposerError::Internal("simulated".into()),
                &mut retry_counts,
                &mut cache,
            );
            assert_eq!(
                retry_counts.get(&SUBMIT_BLOCK_INTERVAL).copied(),
                Some(attempt),
                "attempt {attempt}: counter should equal attempt count",
            );
            assert!(cache.is_some(), "attempt {attempt}: cache should still be populated");
        }

        // Third failure trips max_retries=3: counter is removed and cache is cleared.
        pipeline.handle_proof_failure(
            SUBMIT_BLOCK_INTERVAL,
            ProposerError::Internal("simulated".into()),
            &mut retry_counts,
            &mut cache,
        );

        assert!(
            !retry_counts.contains_key(&SUBMIT_BLOCK_INTERVAL),
            "retry counter should be removed at max_retries"
        );
        assert!(cache.is_none(), "cache should be dropped when max_retries is reached");
    }

    /// `run()` honors cancellation between iterations.
    #[tokio::test(flavor = "current_thread")]
    async fn test_run_returns_when_cancelled() {
        let (pipeline, _proof_requester, cancel) = step_pipeline_default(0);
        let pipeline = Arc::new(pipeline);

        let runner = tokio::spawn({
            let pipeline = Arc::clone(&pipeline);
            async move { pipeline.run().await }
        });

        // Yield once so the spawned task can begin its first iteration.
        tokio::task::yield_now().await;
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(5), runner)
            .await
            .expect("run should return promptly after cancel")
            .expect("run task should not panic");
        assert!(result.is_ok(), "run should return Ok when cancelled");
    }

    /// When `safe_head < target_block`, `step()` returns without dispatching
    /// or submitting and leaves retry counters untouched.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_step_skips_when_safe_head_below_next_target() {
        // safe_head=0, target = 0 + SUBMIT_BLOCK_INTERVAL = 4 > 0 → skip.
        let (pipeline, proof_requester, _cancel) = step_pipeline_default(0);

        let mut cache: Option<CachedRecovery> = None;
        let mut retry_counts: HashMap<u64, u32> = HashMap::new();
        let mut discarded_targets: HashSet<u64> = HashSet::new();
        pipeline.step(&mut cache, &mut retry_counts, &mut discarded_targets).await;

        assert!(
            proof_requester.requests.lock().unwrap().is_empty(),
            "no proof should have been dispatched while safe head is behind target"
        );
        assert!(retry_counts.is_empty(), "retry counters should be untouched");
    }

    /// When the prover service has no session for the next target,
    /// `step()` dispatches a fresh request via `dispatch_for`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_step_dispatches_when_no_session_exists() {
        let (pipeline, proof_requester, _cancel) = step_pipeline_default(SUBMIT_BLOCK_INTERVAL);

        let mut cache: Option<CachedRecovery> = None;
        let mut retry_counts: HashMap<u64, u32> = HashMap::new();
        let mut discarded_targets: HashSet<u64> = HashSet::new();
        pipeline.step(&mut cache, &mut retry_counts, &mut discarded_targets).await;

        let requests = proof_requester.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "exactly one prove_block_range request should have been dispatched"
        );
        assert!(retry_counts.is_empty(), "successful dispatch should not bump the retry counter");
    }

    /// `submit_inline` with a `RootMismatch` outcome drops the cached
    /// recovery but leaves retry counters untouched (transient submit
    /// failures never count against the per-target retry budget).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_submit_inline_root_mismatch_clears_cache_only() {
        // Force a final-root mismatch by overriding the canonical root for
        // the target block.
        let output_roots = HashMap::from([(SUBMIT_BLOCK_INTERVAL, B256::repeat_byte(0xFF))]);
        let (pipeline, _proof_requester, _cancel) = step_pipeline_full(
            output_roots,
            SUBMIT_BLOCK_INTERVAL,
            3,
            Duration::from_secs(60),
            Arc::new(MockOutputProposer),
        );

        let recovered = anchor_recovered_state();
        let proof = submit_proof_result(SUBMIT_BLOCK_INTERVAL);
        let mut cache = Some(CachedRecovery { game_count: 0, state: recovered });
        let mut retry_counts: HashMap<u64, u32> = HashMap::from([(SUBMIT_BLOCK_INTERVAL, 1)]);
        let mut discarded_targets: HashSet<u64> = HashSet::new();

        pipeline
            .submit_inline(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                proof,
                &mut retry_counts,
                &mut cache,
                &mut discarded_targets,
            )
            .await;

        assert!(cache.is_none(), "RootMismatch should drop the recovery cache");
        assert_eq!(
            retry_counts.get(&SUBMIT_BLOCK_INTERVAL).copied(),
            Some(1),
            "submit failures should not bump per-target retry counters"
        );
    }

    /// `submit_inline` with an `InvalidParentGame` rejection drops the
    /// cached recovery (so the next iteration re-walks) and does not bump
    /// retry counters.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_submit_inline_invalid_parent_game_clears_cache() {
        let (pipeline, _proof_requester, _cancel) = step_pipeline_full(
            HashMap::new(),
            SUBMIT_BLOCK_INTERVAL,
            3,
            Duration::from_secs(60),
            Arc::new(InvalidParentGameOutputProposer),
        );

        let recovered = anchor_recovered_state();
        let proof = submit_proof_result(SUBMIT_BLOCK_INTERVAL);
        let mut cache = Some(CachedRecovery { game_count: 0, state: recovered });
        let mut retry_counts: HashMap<u64, u32> = HashMap::new();
        let mut discarded_targets: HashSet<u64> = HashSet::new();

        pipeline
            .submit_inline(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                proof,
                &mut retry_counts,
                &mut cache,
                &mut discarded_targets,
            )
            .await;

        assert!(cache.is_none(), "InvalidParentGame should drop the recovery cache");
        assert!(retry_counts.is_empty(), "submit failures should not bump retry counters");
    }

    /// Other transient submit failures preserve both the cache and retry
    /// counters — the next loop iteration re-collects and retries.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_submit_inline_transient_failure_preserves_cache() {
        let (pipeline, _proof_requester, _cancel) = step_pipeline_full(
            HashMap::new(),
            SUBMIT_BLOCK_INTERVAL,
            3,
            Duration::from_secs(60),
            Arc::new(TransientFailOutputProposer),
        );

        let recovered = anchor_recovered_state();
        let proof = submit_proof_result(SUBMIT_BLOCK_INTERVAL);
        let mut cache = Some(CachedRecovery { game_count: 0, state: recovered });
        let mut retry_counts: HashMap<u64, u32> = HashMap::new();
        let mut discarded_targets: HashSet<u64> = HashSet::new();

        pipeline
            .submit_inline(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                proof,
                &mut retry_counts,
                &mut cache,
                &mut discarded_targets,
            )
            .await;

        assert!(cache.is_some(), "transient submit failures should preserve the recovery cache");
        assert!(
            retry_counts.is_empty(),
            "transient submit failures should not bump retry counters"
        );
    }

    /// When `submit_inline` exceeds `submit_timeout`, neither the cache
    /// nor retry counters are mutated.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_submit_inline_timeout_does_not_count_against_retries() {
        let submit_timeout = Duration::from_millis(50);
        let (pipeline, _proof_requester, _cancel) = step_pipeline_full(
            HashMap::new(),
            SUBMIT_BLOCK_INTERVAL,
            3,
            submit_timeout,
            Arc::new(DelayedOutputProposer { delay: submit_timeout * 10 }),
        );

        let recovered = anchor_recovered_state();
        let proof = submit_proof_result(SUBMIT_BLOCK_INTERVAL);
        let mut cache = Some(CachedRecovery { game_count: 0, state: recovered });
        let mut retry_counts: HashMap<u64, u32> = HashMap::new();
        let mut discarded_targets: HashSet<u64> = HashSet::new();

        pipeline
            .submit_inline(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                proof,
                &mut retry_counts,
                &mut cache,
                &mut discarded_targets,
            )
            .await;

        assert!(cache.is_some(), "submit timeout should preserve the recovery cache");
        assert!(retry_counts.is_empty(), "submit timeout should not bump retry counters");
    }

    /// On a successful submission `submit_inline` advances both
    /// `last_proposed_block` and `last_collected_block` to the target block.
    #[cfg(feature = "metrics")]
    #[test]
    fn test_submit_inline_advances_block_gauges_on_success() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        with_recorder(|snap| {
            rt.block_on(async {
                let (pipeline, _proof_requester, _cancel) = step_pipeline_full(
                    HashMap::new(),
                    SUBMIT_BLOCK_INTERVAL,
                    3,
                    Duration::from_secs(60),
                    Arc::new(MockOutputProposer),
                );
                let recovered = anchor_recovered_state();
                let proof = submit_proof_result(SUBMIT_BLOCK_INTERVAL);
                let mut cache: Option<CachedRecovery> = None;
                let mut retry_counts: HashMap<u64, u32> = HashMap::new();
                let mut discarded_targets: HashSet<u64> = HashSet::new();
                pipeline
                    .submit_inline(
                        SUBMIT_BLOCK_INTERVAL,
                        &recovered,
                        proof,
                        &mut retry_counts,
                        &mut cache,
                        &mut discarded_targets,
                    )
                    .await;
            });

            let snapshot = snap.snapshot().into_vec();
            for name in ["base_proposer.last_proposed_block", "base_proposer.last_collected_block"]
            {
                match find_metric(&snapshot, MetricKind::Gauge, name) {
                    Some(DebugValue::Gauge(value)) => {
                        assert_eq!(
                            value.into_inner(),
                            SUBMIT_BLOCK_INTERVAL as f64,
                            "{name} should advance to target block on success",
                        );
                    }
                    other => panic!("expected {name} gauge, got {other:?}"),
                }
            }
        });
    }

    /// `submit_inline` with a `RootMismatch` outcome increments the
    /// `root_mismatch_total` counter.
    #[cfg(feature = "metrics")]
    #[test]
    fn test_submit_inline_increments_root_mismatch_total() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        with_recorder(|snap| {
            rt.block_on(async {
                let output_roots =
                    HashMap::from([(SUBMIT_BLOCK_INTERVAL, B256::repeat_byte(0xFF))]);
                let (pipeline, _proof_requester, _cancel) = step_pipeline_full(
                    output_roots,
                    SUBMIT_BLOCK_INTERVAL,
                    3,
                    Duration::from_secs(60),
                    Arc::new(MockOutputProposer),
                );
                let recovered = anchor_recovered_state();
                let proof = submit_proof_result(SUBMIT_BLOCK_INTERVAL);
                let mut cache = Some(CachedRecovery { game_count: 0, state: recovered });
                let mut retry_counts: HashMap<u64, u32> = HashMap::new();
                let mut discarded_targets: HashSet<u64> = HashSet::new();
                pipeline
                    .submit_inline(
                        SUBMIT_BLOCK_INTERVAL,
                        &recovered,
                        proof,
                        &mut retry_counts,
                        &mut cache,
                        &mut discarded_targets,
                    )
                    .await;
            });

            let snapshot = snap.snapshot().into_vec();
            match find_metric(&snapshot, MetricKind::Counter, "base_proposer.root_mismatch_total") {
                Some(DebugValue::Counter(value)) => {
                    assert_eq!(*value, 1, "root_mismatch_total should increment once");
                }
                other => panic!("expected root_mismatch_total counter, got {other:?}"),
            }
        });
    }

    /// On successful submission, `submit_inline` clears the per-target
    /// retry counter and refreshes the recovery cache.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_submit_inline_success_clears_retry_counter() {
        let (pipeline, _proof_requester, _cancel) = step_pipeline_full(
            HashMap::new(),
            SUBMIT_BLOCK_INTERVAL,
            3,
            Duration::from_secs(60),
            Arc::new(MockOutputProposer),
        );

        let recovered = anchor_recovered_state();
        let proof = submit_proof_result(SUBMIT_BLOCK_INTERVAL);
        let mut cache: Option<CachedRecovery> = None;
        let mut retry_counts: HashMap<u64, u32> = HashMap::from([(SUBMIT_BLOCK_INTERVAL, 2)]);
        let mut discarded_targets: HashSet<u64> = HashSet::new();

        pipeline
            .submit_inline(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                proof,
                &mut retry_counts,
                &mut cache,
                &mut discarded_targets,
            )
            .await;

        assert!(
            !retry_counts.contains_key(&SUBMIT_BLOCK_INTERVAL),
            "successful submit should clear the per-target retry counter"
        );
        assert!(cache.is_some(), "successful submit should refresh the cache");
    }

    /// L1 mock whose `header_by_number` always errors. Used to drive
    /// `dispatch_for` through its build-failure path.
    #[derive(Debug)]
    struct FailingL1;

    #[async_trait]
    impl L1Provider for FailingL1 {
        async fn block_number(&self) -> base_proof_rpc::RpcResult<u64> {
            Ok(TEST_L1_BLOCK_NUMBER)
        }
        async fn header_by_number(
            &self,
            _: Option<u64>,
        ) -> base_proof_rpc::RpcResult<alloy_rpc_types_eth::Header> {
            Err(RpcError::Transport("simulated L1 outage".into()))
        }
        async fn header_by_hash(
            &self,
            _: B256,
        ) -> base_proof_rpc::RpcResult<alloy_rpc_types_eth::Header> {
            unimplemented!()
        }
        async fn block_receipts(
            &self,
            _: B256,
        ) -> base_proof_rpc::RpcResult<Vec<alloy_rpc_types_eth::TransactionReceipt>> {
            unimplemented!()
        }
        async fn code_at(
            &self,
            _: Address,
            _: Option<u64>,
        ) -> base_proof_rpc::RpcResult<alloy_primitives::Bytes> {
            unimplemented!()
        }
        async fn call_contract(
            &self,
            _: Address,
            _: alloy_primitives::Bytes,
            _: Option<u64>,
        ) -> base_proof_rpc::RpcResult<alloy_primitives::Bytes> {
            unimplemented!()
        }
        async fn get_balance(
            &self,
            _: Address,
        ) -> base_proof_rpc::RpcResult<alloy_primitives::U256> {
            Ok(alloy_primitives::U256::ZERO)
        }
    }

    /// `dispatch_for` build failures are transient infrastructure errors and
    /// must not bump the per-target retry budget — they never reached the
    /// prover service, so the proof-failure retry policy does not apply.
    /// Without this guard a sustained L1 RPC outage would burn the whole
    /// retry budget and drop the recovery cache, causing a noisy
    /// re-walk-and-fail-again cycle on every tick.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_dispatch_for_build_failure_does_not_bump_retries() {
        let proof_requester = Arc::new(MockProofRequester::default());
        let cancel = CancellationToken::new();
        let l1 = Arc::new(FailingL1);
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(SUBMIT_BLOCK_INTERVAL, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let anchor_registry = Arc::new(MockAnchorStateRegistry {
            anchor_root: test_anchor_root(TEST_ANCHOR_BLOCK),
            anchor_game: Address::ZERO,
        });

        let pipeline = ProvingPipeline::new(
            PipelineConfig {
                submit_timeout: Duration::from_secs(60),
                max_retries: 3,
                recovery_scan_concurrency: 8,
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    game_type: TEST_GAME_TYPE,
                    block_interval: SUBMIT_BLOCK_INTERVAL,
                    intermediate_block_interval: SUBMIT_INTERMEDIATE_INTERVAL,
                    poll_interval: Duration::from_millis(10),
                    ..Default::default()
                },
            },
            Arc::clone(&proof_requester) as Arc<dyn ProofRequesterProvider>,
            l1,
            l2,
            rollup,
            anchor_registry,
            Arc::new(MockDisputeGameFactory::with_games(vec![])),
            Arc::new(MockAggregateVerifier::default()),
            Arc::new(MockOutputProposer),
            cancel,
        );

        let recovered = anchor_recovered_state();
        let mut cache = Some(CachedRecovery { game_count: 0, state: recovered });
        let mut retry_counts: HashMap<u64, u32> = HashMap::new();

        pipeline
            .dispatch_for(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                B256::repeat_byte(SUBMIT_BLOCK_INTERVAL as u8),
                &mut retry_counts,
                &mut cache,
            )
            .await;

        assert!(
            proof_requester.requests.lock().unwrap().is_empty(),
            "build failure should not reach the prover service"
        );
        assert!(retry_counts.is_empty(), "build failures must not bump per-target retry counters");
        assert!(cache.is_some(), "build failures must not drop the recovery cache");
    }

    /// `step()` short-circuits polling when the target block has already
    /// been marked as discarded. The submitter's `Discard` outcomes (e.g.
    /// `L1OriginTooOld`, `InvalidSigner`) leave the prover-service session
    /// in `Succeeded` with a deterministic id, so re-polling would loop on
    /// the same `Ready` proof indefinitely.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_step_skips_polling_for_discarded_targets() {
        let (pipeline, proof_requester, _cancel) = step_pipeline_default(SUBMIT_BLOCK_INTERVAL);

        let mut cache: Option<CachedRecovery> = None;
        let mut retry_counts: HashMap<u64, u32> = HashMap::new();
        let mut discarded_targets: HashSet<u64> = HashSet::from([SUBMIT_BLOCK_INTERVAL]);

        pipeline.step(&mut cache, &mut retry_counts, &mut discarded_targets).await;

        assert!(
            proof_requester.requests.lock().unwrap().is_empty(),
            "discarded targets must not trigger a fresh prover-service dispatch"
        );
        assert!(
            discarded_targets.contains(&SUBMIT_BLOCK_INTERVAL),
            "discard marker should persist while target is ahead of the chain tip"
        );
    }

    /// `submit_inline` with a `Discard` outcome (e.g. `L1OriginTooOld`)
    /// records the target in `discarded_targets` so subsequent iterations
    /// short-circuit polling instead of re-delivering and re-discarding the
    /// same proof indefinitely.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_submit_inline_discard_marks_target() {
        let (pipeline, _proof_requester, _cancel) = step_pipeline_full(
            HashMap::new(),
            SUBMIT_BLOCK_INTERVAL,
            3,
            Duration::from_secs(60),
            Arc::new(L1OriginTooOldOutputProposer),
        );

        let recovered = anchor_recovered_state();
        let proof = submit_proof_result(SUBMIT_BLOCK_INTERVAL);
        let mut cache = Some(CachedRecovery { game_count: 0, state: recovered });
        let mut retry_counts: HashMap<u64, u32> = HashMap::new();
        let mut discarded_targets: HashSet<u64> = HashSet::new();

        pipeline
            .submit_inline(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                proof,
                &mut retry_counts,
                &mut cache,
                &mut discarded_targets,
            )
            .await;

        assert!(
            discarded_targets.contains(&SUBMIT_BLOCK_INTERVAL),
            "Discard outcome should mark the target so subsequent polls skip it"
        );
        assert!(retry_counts.is_empty(), "Discard must not bump per-target retry counters");
        assert!(cache.is_some(), "Discard must not drop the recovery cache");
    }
}
