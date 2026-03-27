//! Parallel proving pipeline for the proposer.
//!
//! The [`ProvingPipeline`] is a three-phase coordinator that runs multiple
//! proofs concurrently while maintaining strictly sequential on-chain submission.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────┐     ┌──────────────┐     ┌──────────────┐
//! │  PLAN    │ ──▶ │  PROVE       │ ──▶ │  SUBMIT      │
//! │ (scan)   │     │ (parallel)   │     │ (sequential) │
//! └──────────┘     └──────────────┘     └──────────────┘
//! ```
//!
//! - **Plan**: Builds `ProofRequest`s for block ranges up to the current safe head.
//! - **Prove**: Dispatches proof tasks into a `JoinSet` with window-based concurrency.
//! - **Submit**: Drains proved results in order, validates against canonical chain (JIT),
//!   and submits on-chain.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{Address, B256, Signature, U256, keccak256};
use alloy_sol_types::SolCall;
use base_proof_contracts::{
    AggregateVerifierClient, AnchorStateRegistryClient, DisputeGameFactoryClient,
    ITEEProverRegistry,
};
use base_proof_primitives::{ProofJournal, ProofRequest, ProofResult, ProverClient};
use base_proof_rpc::{L1Provider, L2Provider, RollupProvider};
use eyre::Result;
use futures::{StreamExt, stream};
use tokio::{task::JoinSet, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::core::{DriverConfig, RecoveredState};
use crate::{
    Metrics,
    constants::{NO_PARENT_INDEX, PROPOSAL_TIMEOUT, RECOVERY_SCAN_CONCURRENCY},
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
    /// Maximum number of games to scan backwards when recovering state on startup.
    pub max_game_recovery_lookback: u64,
    /// Base driver configuration.
    pub driver: DriverConfig,
    /// Optional Unix timestamp at which the V1 hardfork activates.
    /// When set, the pipeline will not perform any work until this
    /// timestamp has been reached.
    pub v1_hardfork_timestamp: Option<u64>,
    /// Optional address of the `TEEProverRegistry` contract on L1.
    /// When set, the pipeline validates signers via `isValidSigner` before submission.
    pub tee_prover_registry_address: Option<Address>,
}

/// Snapshot of the last successful recovery, used to avoid re-scanning the
/// entire `DisputeGameFactory` on every tick.
#[derive(Debug, Clone, Copy)]
struct CachedRecovery {
    /// The factory `game_count` at the time of recovery.
    game_count: u64,
    state: RecoveredState,
}

/// Mutable state for the coordinator loop.
struct PipelineState {
    /// Running proof tasks, each yielding `(target_block, result)`.
    prove_tasks: JoinSet<(u64, Result<ProofResult, ProposerError>)>,
    /// Completed proofs waiting for sequential submission, keyed by target block.
    proved: BTreeMap<u64, ProofResult>,
    /// Target blocks currently being proved.
    inflight: BTreeSet<u64>,
    /// Per-target-block retry counts; exceeding `max_retries` triggers a full reset.
    retry_counts: BTreeMap<u64, u32>,
    /// Cached result from the last successful recovery scan.
    cached_recovery: Option<CachedRecovery>,
}

impl PipelineState {
    fn new() -> Self {
        Self {
            prove_tasks: JoinSet::new(),
            proved: BTreeMap::new(),
            inflight: BTreeSet::new(),
            retry_counts: BTreeMap::new(),
            cached_recovery: None,
        }
    }

    fn reset(&mut self) {
        self.prove_tasks.abort_all();
        self.inflight.clear();
        self.proved.clear();
        self.retry_counts.clear();
        self.cached_recovery = None;
    }

    fn prune_stale(&mut self, recovered_block: u64) {
        self.proved.retain(|&target, _| target > recovered_block);
        self.inflight.retain(|&target| target > recovered_block);
        self.retry_counts.retain(|&target, _| target > recovered_block);
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

    /// Returns `true` if the V1 hardfork is active based on the current wall
    /// clock time, or if no hardfork timestamp is configured (i.e. the gate
    /// is disabled).
    ///
    /// NOTE: This intentionally uses [`SystemTime`] (real wall clock) rather
    /// than `tokio::time::Instant`, because hardfork activation is anchored to
    /// a Unix timestamp and must reflect real-world time regardless of the
    /// tokio runtime's clock state.  Tests that assert on this method use
    /// extreme sentinel values (0 / `u64::MAX`) so they are wall-clock safe.
    fn is_v1_hardfork_active(&self) -> bool {
        self.config.v1_hardfork_timestamp.is_none_or(|ts| {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            now >= ts
        })
    }

    /// Runs the parallel proving pipeline until cancelled.
    pub async fn run(&self) -> Result<()> {
        info!(
            max_parallel_proofs = self.config.max_parallel_proofs,
            block_interval = self.config.driver.block_interval,
            v1_hardfork_timestamp = self.config.v1_hardfork_timestamp,
            "Starting parallel proving pipeline"
        );

        let mut state = PipelineState::new();

        loop {
            tokio::select! {
                biased;

                () = self.cancel.cancelled() => {
                    state.prove_tasks.abort_all();
                    break;
                }
                result = self.tick(&mut state) => {
                    if let Err(e) = result {
                        error!(error = ?e, "Pipeline failed, retrying next interval");
                    }
                }
            }

            while let Some(result) = state.prove_tasks.try_join_next() {
                self.handle_proof_result(result, &mut state);
            }

            tokio::select! {
                () = self.cancel.cancelled() => {
                    state.prove_tasks.abort_all();
                    break;
                }
                () = sleep(self.config.driver.poll_interval) => {}
            }
        }

        info!("Parallel proving pipeline stopped");
        Ok(())
    }

    /// Executes one pipeline tick: recover state, dispatch new proofs, submit
    /// completed results.
    async fn tick(&self, state: &mut PipelineState) -> Result<()> {
        if !self.is_v1_hardfork_active() {
            debug!(
                v1_hardfork_timestamp = self.config.v1_hardfork_timestamp,
                "V1 hardfork not yet active, skipping tick"
            );
            return Ok(());
        }

        if let Some((recovered, safe_head)) =
            self.try_recover_and_plan(&mut state.cached_recovery).await
        {
            state.prune_stale(recovered.l2_block_number);
            self.dispatch_proofs(&recovered, safe_head, state).await?;
            self.try_submit(recovered, state).await?;
        }
        Ok(())
    }

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
            && state.inflight.len() < self.config.max_parallel_proofs
        {
            match self.build_proof_request_for(start_block, start_output, cursor).await {
                Ok(request) => {
                    let claimed_output = request.claimed_l2_output_root;
                    let prover = Arc::clone(&self.prover);
                    let target = cursor;
                    let cancel = self.cancel.child_token();

                    info!(request = ?request, "Dispatching proof task");
                    state.inflight.insert(target);
                    state.prove_tasks.spawn(async move {
                        tokio::select! {
                            () = cancel.cancelled() => {
                                (target, Err(ProposerError::Internal("cancelled".into())))
                            }
                            result = prover.prove(request) => {
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

    async fn try_submit(&self, initial: RecoveredState, state: &mut PipelineState) -> Result<()> {
        let mut recovered = initial;
        loop {
            let next_to_submit = recovered
                .l2_block_number
                .checked_add(self.config.driver.block_interval)
                .ok_or_else(|| {
                    eyre::eyre!(
                        "overflow: l2_block_number {} + block_interval {}",
                        recovered.l2_block_number,
                        self.config.driver.block_interval
                    )
                })?;

            let proof_result = match state.proved.remove(&next_to_submit) {
                Some(r) => r,
                None => return Ok(()),
            };

            match self
                .validate_and_submit(&proof_result, next_to_submit, recovered.game_index)
                .await
            {
                Ok(()) => {
                    info!(target_block = next_to_submit, "Submission successful");
                    state.retry_counts.remove(&next_to_submit);
                    recovered = match self.recover_latest_state(&mut state.cached_recovery).await {
                        Ok(r) => r,
                        Err(e) => {
                            warn!(error = %e, "Failed to recover state after submission");
                            return Ok(());
                        }
                    };
                }
                Err(SubmitAction::Reorg) => {
                    warn!(
                        target_block = next_to_submit,
                        "Reorg detected at submit time, resetting pipeline"
                    );
                    state.reset();
                    return Ok(());
                }
                Err(SubmitAction::Failed(e)) => {
                    warn!(
                        error = %e,
                        target_block = next_to_submit,
                        "Submission failed, will retry next tick"
                    );
                    state.proved.insert(next_to_submit, proof_result);
                    return Ok(());
                }
                Err(SubmitAction::Discard(e)) => {
                    warn!(
                        error = %e,
                        target_block = next_to_submit,
                        "Proof discarded, will re-prove next tick"
                    );
                    // Don't re-insert the proof — it's permanently invalid.
                    // The block leaves `proved` and `inflight`, so the next
                    // tick will re-dispatch a proof task for it.
                    return Ok(());
                }
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
                info!(target_block = target, "Proof completed successfully");
            }
            Ok((target, Err(e))) => {
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
                }
            }
            Err(join_err) => {
                warn!(error = %join_err, "Proof task panicked or was cancelled");
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

    /// Recovers the latest on-chain state.
    ///
    /// Uses a two-tier strategy to minimise RPC fan-out:
    ///
    /// 1. **Incremental scan** – If a cached recovery exists, only the games
    ///    added since the last scan are checked.  When no new matching game is
    ///    found the cached state is returned without any additional
    ///    `game_info` calls.
    /// 2. **Concurrent full scan** – On cold-start (or when the cache is
    ///    cleared after a pipeline reset) the factory is scanned with
    ///    [`futures::stream::StreamExt::buffered`] to fetch up to
    ///    [`RECOVERY_SCAN_CONCURRENCY`]
    ///    `game_at_index` results concurrently.
    ///
    /// Falls back to the anchor state registry when no matching game is found.
    async fn recover_latest_state(
        &self,
        cache: &mut Option<CachedRecovery>,
    ) -> Result<RecoveredState, ProposerError> {
        let count = self
            .factory_client
            .game_count()
            .await
            .map_err(|e| ProposerError::Contract(format!("recovery game_count failed: {e}")))?;

        if let Some(cached) = cache.as_mut() {
            match count.cmp(&cached.game_count) {
                Ordering::Equal => {
                    debug!(
                        game_count = count,
                        "No new games since last recovery, returning cached state"
                    );
                    return Ok(cached.state);
                }
                Ordering::Less => {
                    warn!(
                        cached_count = cached.game_count,
                        current_count = count,
                        "Game count decreased, invalidating recovery cache"
                    );
                    *cache = None;
                }
                Ordering::Greater => {
                    let new_games = count - cached.game_count;

                    if new_games > self.config.max_game_recovery_lookback {
                        warn!(
                            new_games,
                            max = self.config.max_game_recovery_lookback,
                            "Incremental delta exceeds lookback, falling back to full scan"
                        );
                        *cache = None;
                    } else {
                        debug!(
                            new_games,
                            cached_count = cached.game_count,
                            current_count = count,
                            "Incremental recovery scan"
                        );

                        if let Some(state) = self.scan_range_for_recovery(count, new_games).await? {
                            *cache = Some(CachedRecovery { game_count: count, state });
                            return Ok(state);
                        }

                        // No matching game in the new range — keep the
                        // previously cached state and only advance the
                        // watermark.  This assumes factory game indices are
                        // append-only (no reorg replaces a cached game with a
                        // non-matching one at the same count).  The submission
                        // path re-validates against the canonical chain, so a
                        // stale cached state would be caught before any
                        // onchain action.
                        cached.game_count = count;
                        return Ok(cached.state);
                    }
                }
            }
        }
        let search_count = count.min(self.config.max_game_recovery_lookback);
        debug!(search_count, "Full concurrent recovery scan");
        if let Some(state) = self.scan_range_for_recovery(count, search_count).await? {
            *cache = Some(CachedRecovery { game_count: count, state });
            return Ok(state);
        }

        debug!(
            game_type = self.config.driver.game_type,
            searched = search_count,
            "No games found for our game type, falling back to anchor state registry"
        );

        let anchor = self.anchor_registry.get_anchor_root().await?;
        debug!(
            l2_block_number = anchor.l2_block_number,
            root = ?anchor.root,
            "Recovered state from anchor state registry"
        );
        let state = RecoveredState {
            game_index: NO_PARENT_INDEX,
            output_root: anchor.root,
            l2_block_number: anchor.l2_block_number,
        };
        // Do NOT cache the anchor fallback — we want to re-scan when new games
        // appear so we can pick up a real parent game.
        Ok(state)
    }

    /// Scans a range of factory indices for the most recent game matching our
    /// `game_type`, using concurrent RPC calls.
    ///
    /// Indices are scanned most-recent-first: starting at `count - 1` and
    /// walking backwards for `len` entries.  Uses order-preserving
    /// [`futures::stream::StreamExt::buffered`] with a concurrency limit of
    /// [`RECOVERY_SCAN_CONCURRENCY`], so up to that many `game_at_index` RPCs
    /// are in-flight simultaneously.  Once a type-match is found the stream is
    /// dropped, cancelling any not-yet-started futures — however, RPCs already
    /// dispatched (up to the concurrency limit) will have been sent to the
    /// network even if their results are unused.
    async fn scan_range_for_recovery(
        &self,
        count: u64,
        len: u64,
    ) -> Result<Option<RecoveredState>, ProposerError> {
        if len == 0 {
            return Ok(None);
        }

        let game_type = self.config.driver.game_type;

        // Fetch game_at_index concurrently with order-preserving buffering.
        // Because indices are emitted highest-first, the first type-match is
        // the best — stop consuming the stream immediately.
        let mut stream = std::pin::pin!(stream::iter(0..len)
            .map(|i| {
                let factory = &self.factory_client;
                async move {
                    let game_index = count - 1 - i;
                    let result = factory.game_at_index(game_index).await;
                    (game_index, result)
                }
            })
            .buffered(RECOVERY_SCAN_CONCURRENCY)
            .filter_map(|(idx, result)| async move {
                match result {
                    Ok(game) if game.game_type == game_type => Some((idx, game)),
                    Ok(_) => None,
                    Err(e) => {
                        warn!(error = %e, game_index = idx, "Failed to read game at index during recovery");
                        None
                    }
                }
            }));

        // Try each matching game in order (most-recent first). If
        // `game_info` fails for one match, fall through to the next rather
        // than giving up entirely.
        while let Some((game_index, game)) = stream.next().await {
            let game_info = match self.verifier_client.game_info(game.proxy).await {
                Ok(info) => info,
                Err(e) => {
                    warn!(error = %e, game_index, "Failed to read game_info during recovery, trying next match");
                    continue;
                }
            };

            // Verify the on-chain root_claim matches the canonical output root
            // from the rollup node.  Games with mismatched roots are skipped so
            // we never chain off an incorrect proposal.
            match self.rollup_client.output_at_block(game_info.l2_block_number).await {
                // Root matches — safe to chain off this game.
                Ok(canonical) if canonical.output_root == game_info.root_claim => {}
                // Mismatch — this game proposed an incorrect root. Skip it and
                // keep scanning for an older game with a valid root.
                Ok(canonical) => {
                    warn!(
                        game_index,
                        game_proxy = %game.proxy,
                        on_chain_root = ?game_info.root_claim,
                        canonical_root = ?canonical.output_root,
                        l2_block_number = game_info.l2_block_number,
                        "Output root mismatch, skipping game"
                    );
                    continue;
                }
                // RPC failure is transient — bubble up so the caller retries
                // the entire recovery on the next tick rather than silently
                // skipping a potentially valid game.
                Err(e) => {
                    return Err(ProposerError::Rpc(e));
                }
            }

            let idx: u32 = game_index.try_into().map_err(|_| {
                ProposerError::Contract(format!("game index {game_index} exceeds u32"))
            })?;

            debug!(
                game_index,
                game_proxy = %game.proxy,
                output_root = ?game_info.root_claim,
                l2_block_number = game_info.l2_block_number,
                "Recovered parent game state from on-chain"
            );

            return Ok(Some(RecoveredState {
                game_index: idx,
                output_root: game_info.root_claim,
                l2_block_number: game_info.l2_block_number,
            }));
        }

        Ok(None)
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
            starting_l2_block: U256::from(starting_block_number),
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

    async fn validate_and_submit(
        &self,
        proof_result: &ProofResult,
        target_block: u64,
        parent_index: u32,
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
            return Err(SubmitAction::Reorg);
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
            parent_index,
            intermediate_roots_count = intermediate_roots.len(),
            proposals_count = proposals.len(),
            "Proposing output (creating dispute game)"
        );

        // Submit with timeout.
        match tokio::time::timeout(
            PROPOSAL_TIMEOUT,
            self.output_proposer.propose_output(
                aggregate_proposal,
                target_block,
                parent_index,
                &intermediate_roots,
            ),
        )
        .await
        {
            Ok(Ok(())) => {
                info!(target_block, "Dispute game created successfully");
                Metrics::l2_output_proposals_total().increment(1);
                Ok(())
            }
            Ok(Err(e)) => {
                if is_game_already_exists(&e) {
                    info!(
                        target_block,
                        "Game already exists, next tick will load fresh state from chain"
                    );
                    // Treat as success — game already submitted (possibly by another proposer).
                    Ok(())
                } else {
                    Err(SubmitAction::Failed(e))
                }
            }
            Err(_) => Err(SubmitAction::Failed(ProposerError::Internal(format!(
                "dispute game creation timed out after {}s",
                PROPOSAL_TIMEOUT.as_secs()
            )))),
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
    /// Chain reorg detected — output root no longer matches canonical.
    Reorg,
    /// Transient failure — retry later with the same proof.
    Failed(ProposerError),
    /// Proof is permanently invalid (e.g. signer not registered) — discard
    /// and re-prove on the next attempt.
    Discard(ProposerError),
}

impl std::fmt::Display for SubmitAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reorg => write!(f, "reorg detected"),
            Self::Failed(e) | Self::Discard(e) => write!(f, "{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, time::Duration};

    use alloy_primitives::{Address, B256, Bytes, U256};
    use async_trait::async_trait;
    use base_proof_contracts::{GameAtIndex, GameInfo};
    use base_proof_primitives::{ProofResult, Proposal, ProverClient};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        constants::NO_PARENT_INDEX,
        test_utils::{
            MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
            MockOutputProposer, MockRollupClient, test_anchor_root, test_sync_status,
        },
    };

    fn test_proposal(block_number: u64) -> Proposal {
        Proposal {
            output_root: B256::repeat_byte(block_number as u8),
            signature: Bytes::from(vec![0xab; 65]),
            l1_origin_hash: B256::repeat_byte(0x02),
            l1_origin_number: U256::from(100 + block_number),
            l2_block_number: U256::from(block_number),
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
            let aggregate_proposal = Proposal {
                output_root: B256::repeat_byte(block_number as u8),
                signature: Bytes::from(vec![0xab; 65]),
                l1_origin_hash: B256::repeat_byte(0x02),
                l1_origin_number: U256::from(100 + block_number),
                l2_block_number: U256::from(block_number),
                prev_output_root: B256::repeat_byte(0x03),
                config_hash: B256::repeat_byte(0x04),
            };

            // Generate per-block proposals.
            let start = block_number.saturating_sub(512);
            let proposals: Vec<Proposal> =
                ((start + 1)..=block_number).map(test_proposal).collect();

            Ok(ProofResult::Tee { aggregate_proposal, proposals })
        }
    }

    fn test_pipeline(
        pipeline_config: PipelineConfig,
        safe_block_number: u64,
        cancel: CancellationToken,
    ) -> ProvingPipeline<
        MockL1,
        MockL2,
        MockRollupClient,
        MockAnchorStateRegistry,
        MockDisputeGameFactory,
    > {
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let prover: Arc<dyn ProverClient> =
            Arc::new(MockProver { delay: Duration::from_millis(10) });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(safe_block_number, B256::ZERO),
            output_roots: HashMap::new(),
        });
        let anchor_registry =
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(0) });
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

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_pipeline_cancellation() {
        let cancel = CancellationToken::new();
        let pipeline = test_pipeline(
            PipelineConfig {
                max_parallel_proofs: 2,
                max_game_recovery_lookback: 5000,
                max_retries: 3,
                v1_hardfork_timestamp: None,
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    poll_interval: Duration::from_secs(3600),
                    block_interval: 512,
                    intermediate_block_interval: 512,
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
        // Safe head at 512 means target_block=512 is provable (0 + 512 = 512 <= 512).
        let pipeline = test_pipeline(
            PipelineConfig {
                max_parallel_proofs: 2,
                max_game_recovery_lookback: 5000,
                max_retries: 3,
                v1_hardfork_timestamp: None,
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    poll_interval: Duration::from_millis(100),
                    block_interval: 512,
                    intermediate_block_interval: 512,
                    ..Default::default()
                },
            },
            512,
            cancel.clone(),
        );

        // Spawn the pipeline so it starts processing ticks, then cancel
        // from this task after giving it time to run.
        let handle = tokio::spawn(async move { pipeline.run().await });

        tokio::time::sleep(Duration::from_secs(5)).await;
        cancel.cancel();

        let result = handle.await.expect("task should not panic");
        assert!(result.is_ok());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_hardfork_gate_none_always_active() {
        let cancel = CancellationToken::new();
        let pipeline = test_pipeline(
            PipelineConfig {
                max_parallel_proofs: 2,
                max_game_recovery_lookback: 5000,
                max_retries: 3,
                v1_hardfork_timestamp: None,
                tee_prover_registry_address: None,
                driver: DriverConfig::default(),
            },
            0,
            cancel,
        );

        assert!(pipeline.is_v1_hardfork_active(), "gate should be active when no timestamp is set");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_hardfork_gate_past_timestamp_active() {
        let cancel = CancellationToken::new();
        // Timestamp 0 is always in the past.
        let pipeline = test_pipeline(
            PipelineConfig {
                max_parallel_proofs: 2,
                max_game_recovery_lookback: 5000,
                max_retries: 3,
                v1_hardfork_timestamp: Some(0),
                tee_prover_registry_address: None,
                driver: DriverConfig::default(),
            },
            0,
            cancel,
        );

        assert!(
            pipeline.is_v1_hardfork_active(),
            "gate should be active when timestamp is in the past"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_hardfork_gate_future_timestamp_inactive() {
        let cancel = CancellationToken::new();
        let pipeline = test_pipeline(
            PipelineConfig {
                max_parallel_proofs: 2,
                max_game_recovery_lookback: 5000,
                max_retries: 3,
                v1_hardfork_timestamp: Some(u64::MAX),
                tee_prover_registry_address: None,
                driver: DriverConfig::default(),
            },
            0,
            cancel,
        );

        assert!(
            !pipeline.is_v1_hardfork_active(),
            "gate should be inactive when timestamp is in the future"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_tick_skipped_when_hardfork_inactive() {
        let cancel = CancellationToken::new();
        // Safe head at 512: would normally dispatch proofs.
        let pipeline = test_pipeline(
            PipelineConfig {
                max_parallel_proofs: 2,
                max_game_recovery_lookback: 5000,
                max_retries: 3,
                v1_hardfork_timestamp: Some(u64::MAX),
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    poll_interval: Duration::from_millis(100),
                    block_interval: 512,
                    intermediate_block_interval: 512,
                    ..Default::default()
                },
            },
            512,
            cancel,
        );

        let mut state = PipelineState::new();
        let result = pipeline.tick(&mut state).await;

        assert!(result.is_ok(), "tick should return Ok even when gate is inactive");
        assert!(state.inflight.is_empty(), "no proofs should be dispatched when gate is inactive");
        assert!(state.proved.is_empty(), "no proofs should be completed when gate is inactive");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_pipeline_runs_with_past_hardfork_timestamp() {
        let cancel = CancellationToken::new();
        // Same setup as test_pipeline_proves_and_submits but with a past timestamp.
        let pipeline = test_pipeline(
            PipelineConfig {
                max_parallel_proofs: 2,
                max_game_recovery_lookback: 5000,
                max_retries: 3,
                v1_hardfork_timestamp: Some(0),
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    poll_interval: Duration::from_millis(100),
                    block_interval: 512,
                    intermediate_block_interval: 512,
                    ..Default::default()
                },
            },
            512,
            cancel.clone(),
        );

        // Spawn the pipeline so it starts processing ticks with the hardfork
        // gate active, then cancel from this task after giving it time to run.
        let handle = tokio::spawn(async move { pipeline.run().await });

        tokio::time::sleep(Duration::from_secs(5)).await;
        cancel.cancel();

        let result = handle.await.expect("task should not panic");
        assert!(result.is_ok(), "pipeline should run normally with a past hardfork timestamp");
    }

    // ---- Recovery / caching tests ----

    /// Helper: unique proxy address derived from an index.
    fn proxy_addr(index: u64) -> Address {
        let mut bytes = [0u8; 20];
        bytes[12..20].copy_from_slice(&index.to_be_bytes());
        Address::new(bytes)
    }

    /// Builds a pipeline with configurable factory games and verifier game-info.
    fn recovery_pipeline(
        game_type: u32,
        factory: MockDisputeGameFactory,
        verifier: MockAggregateVerifier,
    ) -> ProvingPipeline<
        MockL1,
        MockL2,
        MockRollupClient,
        MockAnchorStateRegistry,
        MockDisputeGameFactory,
    > {
        recovery_pipeline_with_roots(game_type, factory, verifier, HashMap::new())
    }

    fn recovery_pipeline_with_roots(
        game_type: u32,
        factory: MockDisputeGameFactory,
        verifier: MockAggregateVerifier,
        output_roots: HashMap<u64, B256>,
    ) -> ProvingPipeline<
        MockL1,
        MockL2,
        MockRollupClient,
        MockAnchorStateRegistry,
        MockDisputeGameFactory,
    > {
        let cancel = CancellationToken::new();
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let prover: Arc<dyn ProverClient> =
            Arc::new(MockProver { delay: Duration::from_millis(1) });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(0, B256::ZERO),
            output_roots,
        });
        let anchor_registry =
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(0) });

        ProvingPipeline::new(
            PipelineConfig {
                max_parallel_proofs: 1,
                max_game_recovery_lookback: 5000,
                max_retries: 1,
                v1_hardfork_timestamp: None,
                tee_prover_registry_address: None,
                driver: DriverConfig { game_type, ..Default::default() },
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

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_scan_range_for_recovery_zero_length() {
        let pipeline = recovery_pipeline(
            42,
            MockDisputeGameFactory::with_count(0),
            MockAggregateVerifier::empty(),
        );
        let result = pipeline.scan_range_for_recovery(0, 0).await.unwrap();
        assert!(result.is_none(), "zero-length scan should return None");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_scan_range_finds_matching_game() {
        let target_game_type = 42u32;
        let matching_proxy = proxy_addr(2);
        let root = B256::repeat_byte(0xAA);

        // 3 games: indices 0, 1 have wrong type; index 2 matches.
        let games = vec![
            GameAtIndex { game_type: 99, timestamp: 1, proxy: proxy_addr(0) },
            GameAtIndex { game_type: 99, timestamp: 2, proxy: proxy_addr(1) },
            GameAtIndex { game_type: target_game_type, timestamp: 3, proxy: matching_proxy },
        ];

        let mut info_map = HashMap::new();
        info_map.insert(
            matching_proxy,
            GameInfo { root_claim: root, l2_block_number: 512, parent_index: 0 },
        );

        let mut output_roots = HashMap::new();
        output_roots.insert(512, root);

        let pipeline = recovery_pipeline_with_roots(
            target_game_type,
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        let result = pipeline.scan_range_for_recovery(3, 3).await.unwrap();
        let state = result.expect("should find the matching game");
        assert_eq!(state.game_index, 2);
        assert_eq!(state.l2_block_number, 512);
        assert_eq!(state.output_root, root);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_scan_range_returns_most_recent_match() {
        let target_game_type = 42u32;
        let root_1 = B256::repeat_byte(0x01);
        let root_3 = B256::repeat_byte(0x03);

        // Two matching games: index 1 and index 3.  Scan should return index 3
        // (most recent) because indices are walked highest-first.
        let games = vec![
            GameAtIndex { game_type: 99, timestamp: 1, proxy: proxy_addr(0) },
            GameAtIndex { game_type: target_game_type, timestamp: 2, proxy: proxy_addr(1) },
            GameAtIndex { game_type: 99, timestamp: 3, proxy: proxy_addr(2) },
            GameAtIndex { game_type: target_game_type, timestamp: 4, proxy: proxy_addr(3) },
        ];

        let mut info_map = HashMap::new();
        info_map.insert(
            proxy_addr(1),
            GameInfo { root_claim: root_1, l2_block_number: 100, parent_index: 0 },
        );
        info_map.insert(
            proxy_addr(3),
            GameInfo { root_claim: root_3, l2_block_number: 300, parent_index: 1 },
        );

        let mut output_roots = HashMap::new();
        output_roots.insert(100, root_1);
        output_roots.insert(300, root_3);

        let pipeline = recovery_pipeline_with_roots(
            target_game_type,
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        let result = pipeline.scan_range_for_recovery(4, 4).await.unwrap();
        let state = result.expect("should find a match");
        assert_eq!(state.game_index, 3, "should return the highest-index match");
        assert_eq!(state.l2_block_number, 300);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_scan_range_no_matching_game() {
        let target_game_type = 42u32;

        let games = vec![
            GameAtIndex { game_type: 99, timestamp: 1, proxy: proxy_addr(0) },
            GameAtIndex { game_type: 100, timestamp: 2, proxy: proxy_addr(1) },
        ];

        let pipeline = recovery_pipeline(
            target_game_type,
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::empty(),
        );

        let result = pipeline.scan_range_for_recovery(2, 2).await.unwrap();
        assert!(result.is_none(), "should return None when no game matches");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_scan_range_skips_mismatched_output_root() {
        let target_game_type = 42u32;
        let valid_root = B256::repeat_byte(0xBB);

        // Two matching games by type: index 1 (mismatched root) and index 0
        // (valid root).  Scan should skip index 1 and return index 0.
        let games = vec![
            GameAtIndex { game_type: target_game_type, timestamp: 1, proxy: proxy_addr(0) },
            GameAtIndex { game_type: target_game_type, timestamp: 2, proxy: proxy_addr(1) },
        ];

        let mut info_map = HashMap::new();
        info_map.insert(
            proxy_addr(0),
            GameInfo { root_claim: valid_root, l2_block_number: 100, parent_index: 0 },
        );
        info_map.insert(
            proxy_addr(1),
            GameInfo { root_claim: B256::repeat_byte(0xDE), l2_block_number: 200, parent_index: 0 },
        );

        let mut output_roots = HashMap::new();
        output_roots.insert(100, valid_root);
        output_roots.insert(200, B256::repeat_byte(0xAB));

        let pipeline = recovery_pipeline_with_roots(
            target_game_type,
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        let result = pipeline.scan_range_for_recovery(2, 2).await.unwrap();
        let state = result.expect("should find valid match after skipping mismatch");
        assert_eq!(state.game_index, 0);
        assert_eq!(state.l2_block_number, 100);
        assert_eq!(state.output_root, valid_root);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_scan_range_returns_none_when_all_roots_mismatch() {
        let target_game_type = 42u32;

        let games = vec![
            GameAtIndex { game_type: target_game_type, timestamp: 1, proxy: proxy_addr(0) },
            GameAtIndex { game_type: target_game_type, timestamp: 2, proxy: proxy_addr(1) },
        ];

        let mut info_map = HashMap::new();
        info_map.insert(
            proxy_addr(0),
            GameInfo { root_claim: B256::repeat_byte(0xAA), l2_block_number: 100, parent_index: 0 },
        );
        info_map.insert(
            proxy_addr(1),
            GameInfo { root_claim: B256::repeat_byte(0xBB), l2_block_number: 200, parent_index: 0 },
        );

        let mut output_roots = HashMap::new();
        output_roots.insert(100, B256::repeat_byte(0x11));
        output_roots.insert(200, B256::repeat_byte(0x22));

        let pipeline = recovery_pipeline_with_roots(
            target_game_type,
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        let result = pipeline.scan_range_for_recovery(2, 2).await.unwrap();
        assert!(result.is_none(), "all roots mismatch — should return None");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_hit_equal_game_count() {
        let target_game_type = 42u32;
        let matching_proxy = proxy_addr(0);
        let root = B256::repeat_byte(0xBB);

        let games =
            vec![GameAtIndex { game_type: target_game_type, timestamp: 1, proxy: matching_proxy }];

        let mut info_map = HashMap::new();
        info_map.insert(
            matching_proxy,
            GameInfo { root_claim: root, l2_block_number: 256, parent_index: 0 },
        );

        let mut output_roots = HashMap::new();
        output_roots.insert(256, root);

        let pipeline = recovery_pipeline_with_roots(
            target_game_type,
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        // First call: cold start, populates the cache.
        let mut cache: Option<CachedRecovery> = None;
        let state1 = pipeline.recover_latest_state(&mut cache).await.unwrap();
        assert!(cache.is_some(), "cache should be populated after first call");
        assert_eq!(state1.game_index, 0);
        assert_eq!(state1.l2_block_number, 256);
        assert_eq!(cache.as_ref().unwrap().game_count, 1);

        // Second call: same game_count, should return cached state without
        // additional game_at_index calls.
        let state2 = pipeline.recover_latest_state(&mut cache).await.unwrap();
        assert_eq!(state2.game_index, state1.game_index);
        assert_eq!(state2.l2_block_number, state1.l2_block_number);
        assert_eq!(state2.output_root, state1.output_root);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_incremental_new_matching_game() {
        let target_game_type = 42u32;
        let root_0 = B256::repeat_byte(0x01);
        let root_1 = B256::repeat_byte(0x02);

        // Start with one matching game.
        let games = vec![
            GameAtIndex { game_type: target_game_type, timestamp: 1, proxy: proxy_addr(0) },
            // Index 1 will be the new matching game added after the first scan.
            GameAtIndex { game_type: target_game_type, timestamp: 2, proxy: proxy_addr(1) },
        ];

        let mut info_map = HashMap::new();
        info_map.insert(
            proxy_addr(0),
            GameInfo { root_claim: root_0, l2_block_number: 100, parent_index: 0 },
        );
        info_map.insert(
            proxy_addr(1),
            GameInfo { root_claim: root_1, l2_block_number: 200, parent_index: 0 },
        );

        let mut output_roots = HashMap::new();
        output_roots.insert(100, root_0);
        output_roots.insert(200, root_1);

        // Build factory with override: first call sees count=1, but the full
        // Vec has 2 entries so game_at_index(1) works.
        let factory = MockDisputeGameFactory { games: games.clone(), game_count_override: Some(1) };

        let pipeline = recovery_pipeline_with_roots(
            target_game_type,
            factory,
            MockAggregateVerifier::with_game_info(info_map),
            output_roots.clone(),
        );

        // First call: cold start with count=1, finds game at index 0.
        let mut cache: Option<CachedRecovery> = None;
        let state1 = pipeline.recover_latest_state(&mut cache).await.unwrap();
        assert_eq!(state1.game_index, 0);
        assert_eq!(state1.l2_block_number, 100);
        assert_eq!(cache.as_ref().unwrap().game_count, 1);

        // Simulate game_count increasing to 2 by updating the override.
        // Since MockDisputeGameFactory is behind Arc and we can't mutate it,
        // we manually set the cache to simulate the first-call state, then
        // build a new pipeline with game_count_override=2.
        let factory2 = MockDisputeGameFactory { games, game_count_override: Some(2) };

        let pipeline2 = recovery_pipeline_with_roots(
            target_game_type,
            factory2,
            MockAggregateVerifier::with_game_info({
                let mut m = HashMap::new();
                m.insert(
                    proxy_addr(0),
                    GameInfo { root_claim: root_0, l2_block_number: 100, parent_index: 0 },
                );
                m.insert(
                    proxy_addr(1),
                    GameInfo { root_claim: root_1, l2_block_number: 200, parent_index: 0 },
                );
                m
            }),
            output_roots,
        );

        // Incremental scan: cache says count was 1, now it's 2.  The new game
        // at index 1 matches, so the cache is updated.
        let state2 = pipeline2.recover_latest_state(&mut cache).await.unwrap();
        assert_eq!(state2.game_index, 1, "should find the new matching game");
        assert_eq!(state2.l2_block_number, 200);
        assert_eq!(cache.as_ref().unwrap().game_count, 2);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_incremental_no_new_match_returns_cached() {
        let target_game_type = 42u32;
        let root = B256::repeat_byte(0xCC);

        // Game at index 0 matches, game at index 1 does NOT match.
        let games = vec![
            GameAtIndex { game_type: target_game_type, timestamp: 1, proxy: proxy_addr(0) },
            GameAtIndex { game_type: 99, timestamp: 2, proxy: proxy_addr(1) },
        ];

        let mut info_map = HashMap::new();
        info_map.insert(
            proxy_addr(0),
            GameInfo { root_claim: root, l2_block_number: 512, parent_index: 0 },
        );

        let mut output_roots = HashMap::new();
        output_roots.insert(512, root);

        // First call: factory reports count=1, finds game at index 0.
        let factory1 =
            MockDisputeGameFactory { games: games.clone(), game_count_override: Some(1) };
        let pipeline1 = recovery_pipeline_with_roots(
            target_game_type,
            factory1,
            MockAggregateVerifier::with_game_info(info_map.clone()),
            output_roots.clone(),
        );

        let mut cache: Option<CachedRecovery> = None;
        let state1 = pipeline1.recover_latest_state(&mut cache).await.unwrap();
        assert_eq!(state1.game_index, 0);

        // Second call: factory reports count=2, but the new game (index 1) has
        // a different type.  Should return cached state.
        let factory2 = MockDisputeGameFactory { games, game_count_override: Some(2) };
        let pipeline2 = recovery_pipeline_with_roots(
            target_game_type,
            factory2,
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        let state2 = pipeline2.recover_latest_state(&mut cache).await.unwrap();
        assert_eq!(state2.game_index, state1.game_index, "should return cached game_index");
        assert_eq!(state2.l2_block_number, state1.l2_block_number);
        assert_eq!(state2.output_root, state1.output_root);
        // Cache game_count should be updated to 2 even though no new match was
        // found, to avoid re-scanning the same index next tick.
        assert_eq!(cache.as_ref().unwrap().game_count, 2);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_cache_invalidation_count_decreased() {
        let target_game_type = 42u32;
        let root_0 = B256::repeat_byte(0x10);
        let root_2 = B256::repeat_byte(0x30);

        // Seed the cache as if count was 5 and we found a game.
        let mut cache = Some(CachedRecovery {
            game_count: 5,
            state: RecoveredState {
                game_index: 4,
                output_root: B256::repeat_byte(0xDD),
                l2_block_number: 1024,
            },
        });

        // Factory now reports count=3 (e.g. reorg removed games).  The cache
        // should be invalidated and a full scan should occur.
        let games = vec![
            GameAtIndex { game_type: target_game_type, timestamp: 1, proxy: proxy_addr(0) },
            GameAtIndex { game_type: 99, timestamp: 2, proxy: proxy_addr(1) },
            GameAtIndex { game_type: target_game_type, timestamp: 3, proxy: proxy_addr(2) },
        ];

        let mut info_map = HashMap::new();
        info_map.insert(
            proxy_addr(0),
            GameInfo { root_claim: root_0, l2_block_number: 100, parent_index: 0 },
        );
        info_map.insert(
            proxy_addr(2),
            GameInfo { root_claim: root_2, l2_block_number: 300, parent_index: 0 },
        );

        let mut output_roots = HashMap::new();
        output_roots.insert(100, root_0);
        output_roots.insert(300, root_2);

        let pipeline = recovery_pipeline_with_roots(
            target_game_type,
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        // Should find game at index 2 (most recent match) via full scan.
        assert_eq!(state.game_index, 2);
        assert_eq!(state.l2_block_number, 300);
        // Cache should be repopulated with the new count.
        assert_eq!(cache.as_ref().unwrap().game_count, 3);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_falls_back_to_anchor_when_no_games() {
        let target_game_type = 42u32;

        let pipeline = recovery_pipeline(
            target_game_type,
            MockDisputeGameFactory::with_count(0),
            MockAggregateVerifier::empty(),
        );

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.game_index, NO_PARENT_INDEX);
        assert_eq!(state.l2_block_number, 0);
        assert!(cache.is_none(), "anchor fallback should not be cached");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_falls_back_to_anchor_when_no_type_match() {
        let target_game_type = 42u32;

        // Factory has games, but none match target game type.
        let games = vec![
            GameAtIndex { game_type: 99, timestamp: 1, proxy: proxy_addr(0) },
            GameAtIndex { game_type: 100, timestamp: 2, proxy: proxy_addr(1) },
        ];

        let pipeline = recovery_pipeline(
            target_game_type,
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::empty(),
        );

        let mut cache: Option<CachedRecovery> = None;
        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();

        assert_eq!(state.game_index, NO_PARENT_INDEX);
        assert!(cache.is_none(), "anchor fallback should not be cached");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_recovery_incremental_delta_exceeds_lookback_triggers_full_scan() {
        let target_game_type = 42u32;
        let root = B256::repeat_byte(0xFF);

        // Seed the cache as if count was 1.
        let mut cache = Some(CachedRecovery {
            game_count: 1,
            state: RecoveredState {
                game_index: 0,
                output_root: B256::repeat_byte(0xEE),
                l2_block_number: 100,
            },
        });

        // Factory now reports count = 1 + max_game_recovery_lookback + 1,
        // which exceeds the lookback limit.  The cache should be invalidated
        // and a full scan should start.
        let max_lookback = 5000u64; // matches recovery_pipeline's config
        let new_count = 1 + max_lookback + 1;
        // We only need to populate the most recent game for the full scan to
        // find it (scan walks backwards from count-1).
        let last_idx = (new_count - 1) as usize;
        let mut games = Vec::with_capacity(new_count as usize);
        for i in 0..new_count {
            let gt = if i as usize == last_idx { target_game_type } else { 99 };
            games.push(GameAtIndex { game_type: gt, timestamp: i, proxy: proxy_addr(i) });
        }

        let mut info_map = HashMap::new();
        info_map.insert(
            proxy_addr(last_idx as u64),
            GameInfo { root_claim: root, l2_block_number: 9999, parent_index: 0 },
        );

        let mut output_roots = HashMap::new();
        output_roots.insert(9999, root);

        let pipeline = recovery_pipeline_with_roots(
            target_game_type,
            MockDisputeGameFactory::with_games(games),
            MockAggregateVerifier::with_game_info(info_map),
            output_roots,
        );

        let state = pipeline.recover_latest_state(&mut cache).await.unwrap();
        assert_eq!(state.game_index, last_idx as u32);
        assert_eq!(state.l2_block_number, 9999);
        assert_eq!(cache.as_ref().unwrap().game_count, new_count);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_pipeline_state_reset_clears_cache() {
        let mut state = PipelineState::new();
        state.cached_recovery = Some(CachedRecovery {
            game_count: 10,
            state: RecoveredState {
                game_index: 5,
                output_root: B256::repeat_byte(0x11),
                l2_block_number: 512,
            },
        });

        state.reset();
        assert!(state.cached_recovery.is_none(), "reset() should clear cached_recovery");
    }
}
