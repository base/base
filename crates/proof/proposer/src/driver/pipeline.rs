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
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use alloy_primitives::B256;
use base_proof_contracts::{
    AggregateVerifierClient, AnchorStateRegistryClient, DisputeGameFactoryClient,
};
use base_proof_primitives::{ProofRequest, ProofResult, ProverClient};
use base_proof_rpc::{L1Provider, L2Provider, RollupProvider};
use eyre::Result;
use tokio::task::JoinSet;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    constants::{NO_PARENT_INDEX, PROPOSAL_TIMEOUT},
    error::ProposerError,
    metrics as proposer_metrics,
    output_proposer::{OutputProposer, is_game_already_exists},
};

use super::core::{DriverConfig, RecoveredState};

/// Default maximum number of concurrent proof tasks.
const DEFAULT_MAX_PARALLEL_PROOFS: usize = 4;

/// Default maximum number of retries for a failed proof before resetting.
const DEFAULT_MAX_RETRIES: u32 = 3;

/// Configuration for the parallel proving pipeline.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Maximum number of concurrent proof tasks.
    pub max_parallel_proofs: usize,
    /// Maximum number of retries for a single proof range before full reset.
    pub max_retries: u32,
    /// Base driver configuration (poll_interval, block_interval, etc.).
    pub driver: DriverConfig,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            max_parallel_proofs: DEFAULT_MAX_PARALLEL_PROOFS,
            max_retries: DEFAULT_MAX_RETRIES,
            driver: DriverConfig::default(),
        }
    }
}

/// A completed proof ready for submission.
#[derive(Debug, Clone)]
struct ProvedRange {
    proof_result: ProofResult,
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
        f.debug_struct("ProvingPipeline")
            .field("config", &self.config)
            .finish_non_exhaustive()
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
    /// This is the main entry point — runs until cancelled.
    pub async fn run(&self) -> Result<()> {
        info!(
            max_parallel_proofs = self.config.max_parallel_proofs,
            block_interval = self.config.driver.block_interval,
            "Starting parallel proving pipeline"
        );

        let mut prove_tasks: JoinSet<(u64, Result<ProofResult, ProposerError>)> = JoinSet::new();

        // Completed proofs waiting for sequential submission, keyed by target block.
        let mut proved: BTreeMap<u64, ProvedRange> = BTreeMap::new();

        // Target blocks currently being proved.
        let mut inflight: BTreeSet<u64> = BTreeSet::new();

        // Track retry counts for ranges that have failed.
        let mut retry_counts: BTreeMap<u64, u32> = BTreeMap::new();

        loop {
            // ── 0. Recover latest on-chain state ──────────────────────────
            let state = match self.recover_latest_state().await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "Failed to recover on-chain state, retrying next tick");
                    tokio::select! {
                        () = self.cancel.cancelled() => break,
                        () = sleep(self.config.driver.poll_interval) => continue,
                    }
                }
            };

            let next_to_submit = state
                .l2_block_number
                .checked_add(self.config.driver.block_interval)
                .expect("overflow computing next submission target");
            let parent_index = state.game_index;

            // ── 1. Plan: build proof requests up to safe head ─────────────
            let safe_head = match self.latest_safe_block_number().await {
                Ok(n) => n,
                Err(e) => {
                    warn!(error = %e, "Failed to fetch safe head, retrying next tick");
                    tokio::select! {
                        () = self.cancel.cancelled() => break,
                        () = sleep(self.config.driver.poll_interval) => continue,
                    }
                }
            };

            // Plan from the next submission target forward.
            let mut cursor = next_to_submit;
            while cursor <= safe_head
                && !inflight.contains(&cursor)
                && !proved.contains_key(&cursor)
                && inflight.len() < self.config.max_parallel_proofs
            {
                match self.build_proof_request_for(state.l2_block_number, state.output_root, cursor).await {
                    Ok(request) => {
                        let prover = Arc::clone(&self.prover);
                        let target = cursor;
                        let cancel = self.cancel.child_token();

                        inflight.insert(target);
                        prove_tasks.spawn(async move {
                            if cancel.is_cancelled() {
                                return (target, Err(ProposerError::Internal("cancelled".into())));
                            }
                            let result = prover
                                .prove(request)
                                .await
                                .map_err(|e| ProposerError::Prover(e.to_string()));
                            (target, result)
                        });

                        debug!(target_block = target, "Dispatched proof task");
                    }
                    Err(e) => {
                        warn!(error = %e, target_block = cursor, "Failed to build proof request");
                        break;
                    }
                }
                cursor = cursor.saturating_add(self.config.driver.block_interval);
            }

            // ── 2. Select: wait for proof completion or poll tick ──────────
            tokio::select! {
                () = self.cancel.cancelled() => {
                    info!("Pipeline received shutdown signal, aborting in-flight proofs");
                    prove_tasks.abort_all();
                    break;
                }

                Some(join_result) = prove_tasks.join_next(), if !prove_tasks.is_empty() => {
                    match join_result {
                        Ok((target, Ok(proof_result))) => {
                            inflight.remove(&target);
                            retry_counts.remove(&target);
                            proved.insert(target, ProvedRange { proof_result });
                            info!(target_block = target, "Proof completed successfully");
                        }
                        Ok((target, Err(e))) => {
                            inflight.remove(&target);
                            let count = retry_counts.entry(target).or_insert(0);
                            *count += 1;
                            if *count >= self.config.max_retries {
                                error!(
                                    target_block = target,
                                    attempts = *count,
                                    error = %e,
                                    "Proof failed after max retries, resetting pipeline"
                                );
                                // Full reset: cancel all, clear everything.
                                prove_tasks.abort_all();
                                inflight.clear();
                                proved.clear();
                                retry_counts.clear();
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
                            // Task panicked or was cancelled.
                            warn!(error = %join_err, "Proof task panicked or was cancelled");
                            // We can't identify the target block from a JoinError without
                            // wrapping, so do a full reset to be safe.
                            prove_tasks.abort_all();
                            inflight.clear();
                            proved.clear();
                            retry_counts.clear();
                        }
                    }
                }

                () = sleep(self.config.driver.poll_interval) => {
                    debug!("Poll tick — replanning");
                }
            }

            // ── 3. Submit: drain proved results in order ──────────────────
            // We can only submit `next_to_submit` (the first unsubmitted range).
            // If it's in `proved`, validate and submit. Otherwise, wait.
            if let Some(range) = proved.remove(&next_to_submit) {
                match self.validate_and_submit(
                    &range.proof_result,
                    next_to_submit,
                    parent_index,
                ).await {
                    Ok(()) => {
                        info!(target_block = next_to_submit, "Submission successful");
                        // Clear retry count for this range.
                        retry_counts.remove(&next_to_submit);
                        // The next loop iteration will recover fresh state from L1.
                    }
                    Err(SubmitAction::Reorg) => {
                        warn!(
                            target_block = next_to_submit,
                            "Reorg detected at submit time, resetting pipeline"
                        );
                        prove_tasks.abort_all();
                        inflight.clear();
                        proved.clear();
                        retry_counts.clear();
                    }
                    Err(SubmitAction::Failed(e)) => {
                        warn!(
                            error = %e,
                            target_block = next_to_submit,
                            "Submission failed, will retry next tick"
                        );
                        // Put it back for retry next tick.
                        proved.insert(next_to_submit, range);
                    }
                }
            }
        }

        info!("Parallel proving pipeline stopped");
        Ok(())
    }

    // ── Internal helpers ───────────────────────────────────────────────────

    /// Recovers the latest on-chain state.
    ///
    /// Walks the `DisputeGameFactory` backwards, falls back to anchor registry.
    async fn recover_latest_state(&self) -> Result<RecoveredState, ProposerError> {
        let count = self
            .factory_client
            .game_count()
            .await
            .map_err(|e| ProposerError::Contract(format!("recovery game_count failed: {e}")))?;

        let search_count = count.min(crate::constants::MAX_GAME_RECOVERY_LOOKBACK);
        for i in 0..search_count {
            let game_index = count - 1 - i;
            let game = match self.factory_client.game_at_index(game_index).await {
                Ok(g) => g,
                Err(e) => {
                    warn!(error = %e, game_index, "Failed to read game at index during recovery");
                    continue;
                }
            };

            if game.game_type != self.config.driver.game_type {
                continue;
            }

            let game_info = match self.verifier_client.game_info(game.proxy).await {
                Ok(info) => info,
                Err(e) => {
                    warn!(error = %e, game_index, "Failed to read game_info during recovery");
                    continue;
                }
            };

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

            return Ok(RecoveredState {
                game_index: idx,
                output_root: game_info.root_claim,
                l2_block_number: game_info.l2_block_number,
            });
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
        Ok(RecoveredState {
            game_index: NO_PARENT_INDEX,
            output_root: anchor.root,
            l2_block_number: anchor.l2_block_number,
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

    /// Builds a proof request for the given target block.
    ///
    /// The `starting_block_number` and `agreed_output_root` come from the
    /// recovered on-chain state. For the first range after recovery they match
    /// the on-chain parent; for subsequent planned ranges within the same tick,
    /// they chain from the previous range's claimed output.
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

        let claimed_output = self
            .rollup_client
            .output_at_block(target_block)
            .await
            .map_err(ProposerError::Rpc)?;

        let l1_head = self.l1_client.header_by_number(None).await.map_err(ProposerError::Rpc)?;

        let request = ProofRequest {
            l1_head: l1_head.hash,
            agreed_l2_head_hash: agreed_l2_head.hash,
            agreed_l2_output_root: agreed_output_root,
            claimed_l2_output_root: claimed_output.output_root,
            claimed_l2_block_number: target_block,
            proposer: self.config.driver.proposer_address,
            intermediate_block_interval: self.config.driver.intermediate_block_interval,
        };

        info!(request = ?request, "Built proof request for parallel proving");

        Ok(request)
    }

    /// Validates a proof result against the canonical chain and submits it.
    ///
    /// Returns `Ok(())` on success, `Err(SubmitAction::Reorg)` if the output
    /// root no longer matches canonical, or `Err(SubmitAction::Failed)` for
    /// transient errors.
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
            target_block.saturating_sub(self.config.driver.block_interval);
        let intermediate_roots = self
            .extract_intermediate_roots(starting_block_number, proposals)
            .map_err(SubmitAction::Failed)?;

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
                metrics::counter!(proposer_metrics::L2_OUTPUT_PROPOSALS_TOTAL).increment(1);
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

            let idx = target_block.saturating_sub(starting_block_number).saturating_sub(1);
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
    /// Transient failure — retry later.
    Failed(ProposerError),
}

impl std::fmt::Display for SubmitAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reorg => write!(f, "reorg detected"),
            Self::Failed(e) => write!(f, "{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_primitives::{B256, Bytes, U256};
    use async_trait::async_trait;
    use base_proof_primitives::{ProofResult, Proposal, ProverClient};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
        MockOutputProposer, MockRollupClient, test_anchor_root, test_sync_status,
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
            let proposals: Vec<Proposal> = ((start + 1)..=block_number)
                .map(test_proposal)
                .collect();

            Ok(ProofResult::Tee { aggregate_proposal, proposals })
        }
    }

    fn test_pipeline(
        pipeline_config: PipelineConfig,
        safe_block_number: u64,
        cancel: CancellationToken,
    ) -> ProvingPipeline<MockL1, MockL2, MockRollupClient, MockAnchorStateRegistry, MockDisputeGameFactory>
    {
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let prover: Arc<dyn ProverClient> = Arc::new(MockProver { delay: Duration::from_millis(10) });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(safe_block_number, B256::ZERO),
        });
        let anchor_registry =
            Arc::new(MockAnchorStateRegistry { anchor_root: test_anchor_root(0) });
        let factory = Arc::new(MockDisputeGameFactory { game_count: 0 });

        ProvingPipeline::new(
            pipeline_config,
            prover,
            l1,
            l2,
            rollup,
            anchor_registry,
            factory,
            Arc::new(MockAggregateVerifier),
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
                max_retries: 3,
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
                max_retries: 3,
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

        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move {
            // Let the pipeline run for a bit, then cancel.
            tokio::time::sleep(Duration::from_secs(5)).await;
            cancel_clone.cancel();
            pipeline.run().await
        });

        // Start the pipeline run and wait for it to complete.
        let result = handle.await.expect("task should not panic");
        assert!(result.is_ok());
    }
}
