//! Proving pipeline for the proposer.
//!
//! The [`ProvingPipeline`] runs two cooperative tasks per session: a
//! dispatcher loop that walks forward from the latest on-chain game and sends
//! proof requests up to the safe head, and a collector loop that polls and
//! submits completed proofs in order. Submit failures restart both tasks from
//! chain-derived state.
//!
//! # Iteration
//!
//! ```text
//! ┌──────────┐     ┌──────────────────┐
//! │ RECOVER  │ ──▶ │ DISPATCH LOOP    │ ──▶ prover service
//! │ (cached) │     └──────────────────┘
//! │          │     ┌──────────────────┐
//! │          │ ──▶ │ COLLECT LOOP     │ ──▶ L1 submitter
//! └──────────┘     └──────────────────┘
//! ```
//!
//! Normal sessions remain root-derived so a restarted proposer can rediscover
//! work. Discard retries use retry-specific sessions because the prover service
//! intentionally replays `Succeeded` sessions for the root-derived id.

#[cfg(test)]
use std::collections::HashMap;
use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
#[cfg(test)]
use alloy_primitives::B256;
use base_proof_contracts::{
    AggregateVerifierClient, AnchorStateRegistryClient, DisputeGameFactoryClient,
};
#[cfg(test)]
use base_proof_primitives::ProofResult;
use base_proof_rpc::{L1Provider, L2Provider, RollupProvider};
use base_prover_service_client::ProofRequesterProvider;
use eyre::Result;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument, warn};

use crate::{
    Metrics,
    driver::DriverConfig,
    output_proposer::OutputProposer,
    proof_collector::{
        ProofCollector, ProofCollectorOrchestrator, ProofCollectorRuntimeConfig,
        ProofCollectorState, ProofCollectorTickResult,
    },
    proof_dispatcher::{
        ProofDispatcher, ProofDispatcherConfig, ProofDispatcherRuntimeConfig, ProofDispatcherState,
    },
    proof_recovery::{ProofRecovery, ProofRecoveryCache, ProofRecoveryConfig},
    proof_submitter::{ProofSubmitter, ProofSubmitterConfig},
};
#[cfg(test)]
use crate::{
    driver::RecoveredState, error::ProposerError, proof_collector::ProofSubmitEffect,
    proof_dispatcher::ProofDispatchOutcome, proof_submitter::SubmitAction,
};

/// Configuration for the proving pipeline.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Maximum retries for a single target block before dropping the cached
    /// recovery. Only proof failures and dispatch RPC errors count against
    /// this budget; transient submit and poll errors do not.
    pub max_retries: u32,
    /// Maximum number of concurrent RPC calls during the recovery scan.
    pub recovery_scan_concurrency: usize,
    /// Optional maximum duration for a single inline submit (validation + L1
    /// transaction). When exceeded, the pipeline restarts without counting
    /// against the retry budget. `None` disables the outer pipeline timeout.
    pub submit_timeout: Option<Duration>,
    /// Base driver configuration.
    pub driver: DriverConfig,
    /// Optional address of the `TEEProverRegistry` contract on L1.
    /// When set, the pipeline validates signers via `isValidSigner` before submission.
    pub tee_prover_registry_address: Option<Address>,
}

/// Cached result from the last successful recovery.
type CachedRecovery = ProofRecoveryCache;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PipelineSessionExit {
    Cancelled,
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskKind {
    Dispatcher,
    Collector,
}

impl TaskKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Dispatcher => "dispatcher",
            Self::Collector => "collector",
        }
    }
}

#[cfg(test)]
type DispatchOutcome = ProofDispatchOutcome;

#[cfg(test)]
type SubmitEffect = ProofSubmitEffect;

#[cfg(test)]
struct DiscardRetryState<'a> {
    counts: &'a mut HashMap<u64, u32>,
    sessions: &'a mut HashMap<u64, String>,
    count_dispatch_failure: bool,
}

struct CollectorTickContext<'a> {
    cancel: &'a CancellationToken,
    restart_tx: &'a mpsc::Sender<String>,
}

/// The proving pipeline.
///
/// Runs concurrent dispatcher and collector tasks per [`Self::run`] session.
/// Submit failures restart both tasks from on-chain state; cancellation stops
/// them cleanly.
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
    proof_dispatcher: ProofDispatcher<L1, L2, R>,
    proof_collector: ProofCollector<R>,
    proof_submitter: ProofSubmitter<L1, R>,
    proof_recovery: ProofRecovery<R, ASR, F>,
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
            proof_recovery: self.proof_recovery.clone(),
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
    /// Creates a new proving pipeline.
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
        let proof_collector = ProofCollector::target_poller_aws_nitro(
            Arc::clone(&proof_requester),
            Arc::clone(&rollup_client),
        );
        let proof_dispatcher = ProofDispatcher::aws_nitro(
            Arc::clone(&proof_requester),
            Arc::clone(&l1_client),
            Arc::clone(&l2_client),
            Arc::clone(&rollup_client),
            ProofDispatcherConfig {
                proposer_address: config.driver.proposer_address,
                intermediate_block_interval: config.driver.intermediate_block_interval,
                tee_image_hash: config.driver.tee_image_hash,
            },
        );
        let proof_submitter = ProofSubmitter::new(
            Arc::clone(&output_proposer),
            Arc::clone(&rollup_client),
            Arc::clone(&l1_client),
            Arc::clone(&factory_client) as Arc<dyn DisputeGameFactoryClient>,
            Arc::clone(&verifier_client),
            ProofSubmitterConfig {
                proposer_address: config.driver.proposer_address,
                game_type: config.driver.game_type,
                block_interval: config.driver.block_interval,
                intermediate_block_interval: config.driver.intermediate_block_interval,
                tee_image_hash: config.driver.tee_image_hash,
                tee_prover_registry_address: config.tee_prover_registry_address,
                output_fetch_concurrency: config.recovery_scan_concurrency,
            },
        );
        let proof_recovery = ProofRecovery::new(
            ProofRecoveryConfig {
                block_interval: config.driver.block_interval,
                intermediate_block_interval: config.driver.intermediate_block_interval,
                game_type: config.driver.game_type,
                allow_non_finalized: config.driver.allow_non_finalized,
                anchor_state_registry_address: config.driver.anchor_state_registry_address,
                scan_concurrency: config.recovery_scan_concurrency,
            },
            Arc::clone(&rollup_client),
            Arc::clone(&anchor_registry),
            Arc::clone(&factory_client),
        );

        Self {
            config,
            proof_requester: Arc::clone(&proof_requester),
            proof_dispatcher,
            proof_collector,
            proof_submitter,
            proof_recovery,
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

    fn collector_orchestrator(
        &self,
    ) -> ProofCollectorOrchestrator<L1, L2, R, ProofRecovery<R, ASR, F>> {
        ProofCollectorOrchestrator::new(
            self.proof_collector.clone(),
            self.proof_dispatcher.clone(),
            self.proof_submitter.clone(),
            Arc::new(self.proof_recovery.clone()),
            ProofCollectorRuntimeConfig {
                block_interval: self.config.driver.block_interval,
                max_retries: self.config.max_retries,
                submit_timeout: self.config.submit_timeout,
            },
        )
    }

    /// Runs the proving pipeline until cancelled.
    ///
    /// Each session starts a dispatcher task and a collector task. The
    /// dispatcher can run ahead up to the safe head, while the collector
    /// submits proofs in order. Submit failures restart both tasks from a
    /// fresh recovery walk.
    pub async fn run(&self) -> Result<()> {
        info!(
            block_interval = self.config.driver.block_interval,
            poll_interval_secs = self.config.driver.poll_interval.as_secs(),
            submit_timeout_secs = ?self.config.submit_timeout.map(|timeout| timeout.as_secs()),
            "Starting proving pipeline"
        );

        loop {
            match self.run_session().await? {
                PipelineSessionExit::Cancelled => break,
                PipelineSessionExit::Restart => {
                    info!("Restarting proving pipeline after submit failure");
                }
            }
        }

        info!("Proving pipeline stopped");
        Ok(())
    }

    async fn run_session(&self) -> Result<PipelineSessionExit> {
        let session_cancel = self.cancel.child_token();
        let (restart_tx, mut restart_rx) = mpsc::channel(1);

        let mut dispatcher =
            spawn_loop(self.clone(), session_cancel.clone(), |pipeline, cancel| async move {
                pipeline.dispatcher_loop(cancel).await
            });
        let mut collector =
            spawn_loop(self.clone(), session_cancel.clone(), |pipeline, cancel| async move {
                pipeline.collector_loop(cancel, restart_tx).await
            });
        let mut dispatcher_done = false;
        let mut collector_done = false;

        let exit = tokio::select! {
            biased;
            () = self.cancel.cancelled() => PipelineSessionExit::Cancelled,
            result = &mut dispatcher => {
                dispatcher_done = true;
                handle_task_result(TaskKind::Dispatcher, result);
                PipelineSessionExit::Restart
            }
            result = &mut collector => {
                collector_done = true;
                handle_task_result(TaskKind::Collector, result);
                PipelineSessionExit::Restart
            }
            reason = restart_rx.recv() => {
                let reason = reason.unwrap_or_else(|| "collector restart channel closed".to_owned());
                warn!(reason = %reason, "Restarting pipeline session");
                PipelineSessionExit::Restart
            }
        };

        session_cancel.cancel();
        if !dispatcher_done {
            await_loop(TaskKind::Dispatcher, dispatcher).await;
        }
        if !collector_done {
            await_loop(TaskKind::Collector, collector).await;
        }
        Ok(exit)
    }

    /// Runs the proof dispatcher loop.
    ///
    /// The dispatcher recovers the latest on-chain game, then dispatches every
    /// missing block interval up to the safe head. Its cursor is intentionally
    /// independent from the collector cursor: it tracks how far proof requests
    /// have been sent, not how far proofs have landed on-chain.
    #[instrument(skip_all)]
    async fn dispatcher_loop(&self, cancel: CancellationToken) -> Result<()> {
        let mut cache: Option<CachedRecovery> = None;
        let mut state = ProofDispatcherState::new();

        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                () = self.dispatcher_tick(&mut cache, &mut state, &cancel) => {}
            }

            sleep_or_cancel(self.config.driver.poll_interval, &cancel).await;
            if cancel.is_cancelled() {
                break;
            }
        }

        Ok(())
    }

    async fn dispatcher_tick(
        &self,
        cache: &mut Option<CachedRecovery>,
        state: &mut ProofDispatcherState,
        cancel: &CancellationToken,
    ) {
        let _tick_timer = base_metrics::timed!(Metrics::tick_duration_seconds());

        let (recovered, safe_head) = match self.proof_recovery.try_recover_and_plan(cache).await {
            Some(pair) => pair,
            None => {
                Metrics::pipeline_retries().set(state.retry_counts.values().sum::<u32>() as f64);
                return;
            }
        };

        Metrics::safe_head().set(safe_head as f64);
        Metrics::last_proposed_block().set(recovered.l2_block_number as f64);

        let result = self
            .proof_dispatcher
            .tick(
                state,
                recovered,
                safe_head,
                ProofDispatcherRuntimeConfig {
                    block_interval: self.config.driver.block_interval,
                    max_retries: self.config.max_retries,
                },
                cancel,
            )
            .await;
        if result.drop_recovery_cache {
            *cache = None;
        }
    }

    /// Runs the proof collector loop.
    ///
    /// The collector submits proofs in order. Any non-success submit outcome
    /// that invalidates the current submit attempt asks the driver to restart
    /// both loops from a fresh forward walk.
    #[instrument(skip_all)]
    async fn collector_loop(
        &self,
        cancel: CancellationToken,
        restart_tx: mpsc::Sender<String>,
    ) -> Result<()> {
        let mut cache: Option<CachedRecovery> = None;
        let mut state = ProofCollectorState::new();

        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                () = self.collector_tick(
                    &mut cache,
                    &mut state,
                    CollectorTickContext { cancel: &cancel, restart_tx: &restart_tx },
                ) => {}
            }

            sleep_or_cancel(self.config.driver.poll_interval, &cancel).await;
            if cancel.is_cancelled() {
                break;
            }
        }

        Ok(())
    }

    async fn collector_tick(
        &self,
        cache: &mut Option<CachedRecovery>,
        state: &mut ProofCollectorState,
        context: CollectorTickContext<'_>,
    ) {
        let _tick_timer = base_metrics::timed!(Metrics::collector_tick_duration_seconds());

        let (recovered, safe_head) = match self.proof_recovery.try_recover_and_plan(cache).await {
            Some(pair) => pair,
            None => {
                Metrics::pipeline_retries().set(state.retry_counts.values().sum::<u32>() as f64);
                return;
            }
        };

        let result = self
            .collector_orchestrator()
            .tick(state, cache, recovered, safe_head, context.cancel)
            .await;
        if result == ProofCollectorTickResult::Restart {
            request_restart(context.restart_tx, "submit failure").await;
        }
    }

    /// Validates and submits the proof inline against the `submit_timeout`
    /// budget.
    ///
    /// On success, advances `last_proposed_block`, drops the per-target retry
    /// counter, and refreshes the recovery cache
    /// incrementally. Submit failures are transient by default — they do not
    /// count against the per-target retry budget — except `RootMismatch` and
    /// `Failed { is_invalid_parent_game }`, which drop the cached recovery
    /// so the next iteration re-walks the chain.
    #[cfg(test)]
    async fn submit_inline(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        proof: ProofResult,
        retry_counts: &mut HashMap<u64, u32>,
        cache: &mut Option<CachedRecovery>,
        cancel: &CancellationToken,
    ) -> SubmitEffect {
        let mut collector_state = ProofCollectorState {
            retry_counts: std::mem::take(retry_counts),
            ..Default::default()
        };
        let effect = self
            .collector_orchestrator()
            .submit_inline(target_block, recovered, proof, &mut collector_state, cache, cancel)
            .await;
        *retry_counts = collector_state.retry_counts;
        effect
    }

    /// Builds and dispatches a fresh `prove_block_range` request for
    /// `target_block`.
    ///
    /// Request-build failures (transient L1/L2 RPC errors while assembling
    /// the request) are logged and skipped without bumping the per-target
    /// retry budget — they never reached the prover service, so the
    /// proof-failure retry policy does not apply. Dispatcher errors (the
    /// prover service rejected an otherwise valid request) count against the
    /// budget unless this dispatch is an immediate re-dispatch after an
    /// already-counted failed session.
    #[cfg(test)]
    async fn dispatch_for(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        claimed_l2_output_root: B256,
        retry_counts: &mut HashMap<u64, u32>,
        cache: &mut Option<CachedRecovery>,
        count_dispatch_failure: bool,
    ) -> DispatchOutcome {
        let mut dispatcher_state = ProofDispatcherState {
            recovered: None,
            cursor: None,
            retry_counts: std::mem::take(retry_counts),
        };
        let outcome = self
            .proof_dispatcher
            .dispatch_with_retry(
                target_block,
                recovered,
                claimed_l2_output_root,
                &mut dispatcher_state,
                self.config.max_retries,
                count_dispatch_failure,
            )
            .await;
        *retry_counts = dispatcher_state.retry_counts;
        if outcome == DispatchOutcome::RetryExhausted {
            *cache = None;
        }
        outcome
    }

    #[cfg(test)]
    async fn dispatch_discard_retry(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        claimed_l2_output_root: B256,
        retry_counts: &mut HashMap<u64, u32>,
        cache: &mut Option<CachedRecovery>,
        discard_retry: DiscardRetryState<'_>,
    ) {
        let mut collector_state = ProofCollectorState {
            retry_counts: std::mem::take(retry_counts),
            discard_retry_counts: std::mem::take(discard_retry.counts),
            retry_sessions: std::mem::take(discard_retry.sessions),
            ..Default::default()
        };
        self.collector_orchestrator()
            .dispatch_discard_retry(
                target_block,
                recovered,
                claimed_l2_output_root,
                &mut collector_state,
                cache,
                discard_retry.count_dispatch_failure,
            )
            .await;
        *retry_counts = collector_state.retry_counts;
        *discard_retry.counts = collector_state.discard_retry_counts;
        *discard_retry.sessions = collector_state.retry_sessions;
    }

    /// Records a proof failure for `target` and applies the retry policy.
    ///
    /// Increments `proof_retries_total` and the per-target counter. When the
    /// counter reaches `max_retries`, drops the cached recovery so the next
    /// iteration performs a full forward walk.
    #[cfg(test)]
    fn handle_proof_failure(
        &self,
        target: u64,
        error: ProposerError,
        retry_counts: &mut HashMap<u64, u32>,
        cache: &mut Option<CachedRecovery>,
    ) -> bool {
        let mut collector_state = ProofCollectorState {
            retry_counts: std::mem::take(retry_counts),
            ..Default::default()
        };
        let should_retry =
            collector_state.handle_proof_failure(target, error, self.config.max_retries, cache);
        *retry_counts = collector_state.retry_counts;
        should_retry
    }

    /// Validates the proof and submits it to L1 by delegating to the
    /// [`ProofSubmitter`].
    ///
    /// Kept on the pipeline as a thin wrapper so the inline submit path in
    /// [`Self::submit_inline`] (and existing tests) can continue to call a
    /// single entry point. This method itself does NOT apply
    /// `submit_timeout`; the timeout is applied by [`Self::submit_inline`].
    #[cfg(test)]
    async fn validate_and_submit(
        &self,
        proof_result: &ProofResult,
        target_block: u64,
        parent_address: Address,
    ) -> Result<(), SubmitAction> {
        self.proof_submitter.submit(proof_result, target_block, parent_address).await
    }
}

fn spawn_loop<L1, L2, R, ASR, F, Fut>(
    pipeline: ProvingPipeline<L1, L2, R, ASR, F>,
    cancel: CancellationToken,
    f: impl FnOnce(ProvingPipeline<L1, L2, R, ASR, F>, CancellationToken) -> Fut,
) -> JoinHandle<Result<()>>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
    ASR: AnchorStateRegistryClient + 'static,
    F: DisputeGameFactoryClient + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    tokio::spawn(f(pipeline, cancel))
}

async fn await_loop(kind: TaskKind, handle: JoinHandle<Result<()>>) {
    handle_task_result(kind, handle.await);
}

fn handle_task_result(
    kind: TaskKind,
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) {
    match result {
        Ok(Ok(())) => {
            warn!(task = kind.label(), "Pipeline task exited unexpectedly, restarting session");
        }
        Ok(Err(error)) => {
            warn!(task = kind.label(), error = %error, "Pipeline task failed, restarting session");
        }
        Err(error) => {
            warn!(task = kind.label(), error = %error, "Pipeline task panicked, restarting session");
        }
    }
}

async fn sleep_or_cancel(duration: Duration, cancel: &CancellationToken) {
    tokio::select! {
        biased;
        () = cancel.cancelled() => {}
        () = tokio::time::sleep(duration) => {}
    }
}

async fn request_restart(restart_tx: &mpsc::Sender<String>, reason: &str) {
    if restart_tx.send(reason.to_owned()).await.is_err() {
        warn!(reason, "Failed to send pipeline restart request");
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };

    use alloy_primitives::{Address, B256};
    use async_trait::async_trait;
    use base_proof_primitives::{ProofResult, Proposal};
    use base_proof_rpc::RpcError;
    use base_prover_service_client::ProverServiceClientError;
    use base_prover_service_protocol::{
        GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse, ProofStatus,
        ProveBlockRangeRequest, ProveBlockRangeResponse,
    };
    #[cfg(feature = "metrics")]
    use metrics_util::{
        CompositeKey, MetricKind,
        debugging::{DebugValue, DebuggingRecorder, Snapshotter},
    };
    use rstest::rstest;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        proof_adapter::ProposerProofAdapter,
        test_utils::{
            MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
            MockOutputProposer, MockProofRequester, MockRollupClient, test_anchor_root,
            test_proposal, test_sync_status,
        },
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

    // ---- Pipeline builders ----

    /// Type alias to reduce repetition in builder return types.
    type TestPipeline = ProvingPipeline<
        MockL1,
        MockL2,
        MockRollupClient,
        MockAnchorStateRegistry,
        MockDisputeGameFactory,
    >;

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
                submit_timeout: Some(std::time::Duration::from_secs(60)),
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

    // ---- Pipeline loops: dispatch / collect / submit / retry ----

    /// Builds a pipeline tailored for dispatcher / collector / submit tests.
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
                submit_timeout: Some(submit_timeout),
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

    /// Prover-service requester that reports any polled session as failed and
    /// rejects every dispatch. Used to verify that failed-session recovery does
    /// not double-count a same-tick re-dispatch failure.
    #[derive(Debug, Default)]
    struct FailedThenRejectDispatchRequester;

    #[async_trait]
    impl ProofRequesterProvider for FailedThenRejectDispatchRequester {
        async fn prove_block_range(
            &self,
            _: ProveBlockRangeRequest,
        ) -> Result<ProveBlockRangeResponse, ProverServiceClientError> {
            Err(ProverServiceClientError::Timeout("simulated dispatch timeout".into()))
        }

        async fn get_proof(
            &self,
            _: GetProofRequest,
        ) -> Result<GetProofResponse, ProverServiceClientError> {
            Ok(GetProofResponse {
                status: ProofStatus::Failed,
                error_message: Some("simulated failed session".to_owned()),
                result: None,
            })
        }

        async fn list_proofs(
            &self,
            _: ListProofsRequest,
        ) -> Result<ListProofsResponse, ProverServiceClientError> {
            unimplemented!("tests do not list proofs")
        }
    }

    #[derive(Debug)]
    struct AdvancingGameFactory {
        submitted_games: Arc<AtomicU64>,
    }

    #[async_trait]
    impl DisputeGameFactoryClient for AdvancingGameFactory {
        async fn game_count(&self) -> Result<u64, base_proof_contracts::ContractError> {
            Ok(self.submitted_games.load(Ordering::SeqCst))
        }

        async fn game_at_index(
            &self,
            index: u64,
        ) -> Result<base_proof_contracts::GameAtIndex, base_proof_contracts::ContractError>
        {
            Ok(base_proof_contracts::GameAtIndex {
                game_type: TEST_GAME_TYPE,
                timestamp: 0,
                proxy: proxy_addr(index),
            })
        }

        async fn init_bonds(
            &self,
            _: u32,
        ) -> Result<alloy_primitives::U256, base_proof_contracts::ContractError> {
            Ok(alloy_primitives::U256::ZERO)
        }

        async fn game_impls(&self, _: u32) -> Result<Address, base_proof_contracts::ContractError> {
            Ok(Address::ZERO)
        }

        async fn games(
            &self,
            _: u32,
            root_claim: B256,
            _: alloy_primitives::Bytes,
        ) -> Result<Address, base_proof_contracts::ContractError> {
            let block = root_claim.as_slice()[0] as u64;
            let index = block / SUBMIT_BLOCK_INTERVAL;
            if index > 0 && index <= self.submitted_games.load(Ordering::SeqCst) {
                Ok(proxy_addr(index - 1))
            } else {
                Ok(Address::ZERO)
            }
        }
    }

    #[derive(Debug)]
    struct AdvancingOutputProposer {
        submitted_games: Arc<AtomicU64>,
    }

    #[async_trait]
    impl OutputProposer for AdvancingOutputProposer {
        async fn propose_output(
            &self,
            _: &Proposal,
            _: Address,
            _: &[B256],
        ) -> Result<(), ProposerError> {
            self.submitted_games.fetch_add(1, Ordering::SeqCst);
            Ok(())
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

    /// When `safe_head < target_block`, the dispatcher returns without
    /// dispatching and leaves retry counters untouched.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_dispatcher_tick_skips_when_safe_head_below_next_target() {
        // safe_head=0, target = 0 + SUBMIT_BLOCK_INTERVAL = 4 > 0 → skip.
        let (pipeline, proof_requester, cancel) = step_pipeline_default(0);

        let mut cache: Option<CachedRecovery> = None;
        let mut dispatch_state = ProofDispatcherState::new();
        pipeline.dispatcher_tick(&mut cache, &mut dispatch_state, &cancel).await;

        assert!(
            proof_requester.requests.lock().unwrap().is_empty(),
            "no proof should have been dispatched while safe head is behind target"
        );
        assert!(dispatch_state.retry_counts.is_empty(), "retry counters should be untouched");
    }

    /// The dispatcher sends proof requests up to the safe head instead of
    /// limiting itself to one in-flight proof.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_dispatcher_tick_dispatches_all_targets_up_to_safe_head() {
        let safe_head = SUBMIT_BLOCK_INTERVAL * 3;
        let (pipeline, proof_requester, cancel) = step_pipeline_default(safe_head);

        let mut cache: Option<CachedRecovery> = None;
        let mut dispatch_state = ProofDispatcherState::new();
        pipeline.dispatcher_tick(&mut cache, &mut dispatch_state, &cancel).await;

        let requests = proof_requester.requests.lock().unwrap();
        assert_eq!(requests.len(), 3, "dispatcher should dispatch every interval up to safe head");
        assert!(
            dispatch_state.retry_counts.is_empty(),
            "successful dispatch should not bump the retry counter"
        );
    }

    /// A terminal prover-service `Failed` status is sticky until the proposer
    /// calls `prove_block_range` again for the same session id. The collector
    /// must therefore re-dispatch immediately instead of only incrementing a
    /// local retry counter and polling the same failed row again.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_collector_tick_redispatches_failed_session() {
        let (pipeline, proof_requester, cancel) = step_pipeline_default(SUBMIT_BLOCK_INTERVAL);
        let mut dispatch_cache: Option<CachedRecovery> = None;
        let mut dispatch_state = ProofDispatcherState::new();
        pipeline.dispatcher_tick(&mut dispatch_cache, &mut dispatch_state, &cancel).await;

        let session_id = ProposerProofAdapter::tee_session_id_for_root(
            B256::repeat_byte(SUBMIT_BLOCK_INTERVAL as u8),
            pipeline.proof_collector.tee_kind(),
        );
        proof_requester
            .failed_sessions
            .lock()
            .unwrap()
            .insert(session_id, "simulated prover failure".to_owned());
        let prove_count_before =
            proof_requester.prove_count.load(std::sync::atomic::Ordering::SeqCst);

        let mut collect_cache: Option<CachedRecovery> = None;
        let mut collect_state = ProofCollectorState::new();
        let (restart_tx, _restart_rx) = mpsc::channel(1);

        pipeline
            .collector_tick(
                &mut collect_cache,
                &mut collect_state,
                CollectorTickContext { cancel: &cancel, restart_tx: &restart_tx },
            )
            .await;

        assert!(
            proof_requester.prove_count.load(std::sync::atomic::Ordering::SeqCst)
                > prove_count_before,
            "collector should re-dispatch failed prover-service sessions"
        );
        assert_eq!(
            collect_state.retry_counts.get(&SUBMIT_BLOCK_INTERVAL).copied(),
            Some(1),
            "failed session should consume one retry attempt"
        );
    }

    /// A failed prover-service session consumes one retry attempt. If the
    /// immediate re-dispatch also fails, that dispatch failure is logged but
    /// does not consume a second retry in the same collector tick.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_failed_session_redispatch_failure_does_not_double_count_retry() {
        let proof_requester = Arc::new(FailedThenRejectDispatchRequester);
        let cancel = CancellationToken::new();
        let pipeline = ProvingPipeline::new(
            PipelineConfig {
                submit_timeout: Some(Duration::from_secs(60)),
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
            proof_requester,
            Arc::new(MockL1 { latest_block_number: TEST_L1_BLOCK_NUMBER }),
            Arc::new(MockL2 { block_not_found: true, canonical_hash: None }),
            Arc::new(MockRollupClient {
                sync_status: test_sync_status(SUBMIT_BLOCK_INTERVAL, B256::ZERO),
                output_roots: HashMap::new(),
                max_safe_block: None,
            }),
            Arc::new(MockAnchorStateRegistry {
                anchor_root: test_anchor_root(TEST_ANCHOR_BLOCK),
                anchor_game: Address::ZERO,
            }),
            Arc::new(MockDisputeGameFactory::with_games(vec![])),
            Arc::new(MockAggregateVerifier::default()),
            Arc::new(MockOutputProposer),
            cancel.clone(),
        );

        let mut cache: Option<CachedRecovery> = None;
        let mut collect_state = ProofCollectorState::new();
        let (restart_tx, _restart_rx) = mpsc::channel(1);

        pipeline
            .collector_tick(
                &mut cache,
                &mut collect_state,
                CollectorTickContext { cancel: &cancel, restart_tx: &restart_tx },
            )
            .await;

        assert_eq!(
            collect_state.retry_counts.get(&SUBMIT_BLOCK_INTERVAL).copied(),
            Some(1),
            "failed session plus same-tick dispatch failure should consume one retry"
        );
    }

    /// Submit failures ask the driver to restart both dispatcher and collector
    /// tasks from chain-derived state.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_collector_tick_requests_restart_on_submit_failure() {
        let (pipeline, proof_requester, cancel) = step_pipeline_full(
            HashMap::new(),
            SUBMIT_BLOCK_INTERVAL,
            3,
            Duration::from_secs(60),
            Arc::new(TransientFailOutputProposer),
        );
        let mut dispatch_cache: Option<CachedRecovery> = None;
        let mut dispatch_state = ProofDispatcherState::new();
        pipeline.dispatcher_tick(&mut dispatch_cache, &mut dispatch_state, &cancel).await;

        assert_eq!(
            proof_requester.requests.lock().unwrap().len(),
            1,
            "test setup should dispatch one ready proof"
        );

        let mut collect_cache: Option<CachedRecovery> = None;
        let mut collect_state = ProofCollectorState::new();
        let (restart_tx, mut restart_rx) = mpsc::channel(1);

        pipeline
            .collector_tick(
                &mut collect_cache,
                &mut collect_state,
                CollectorTickContext { cancel: &cancel, restart_tx: &restart_tx },
            )
            .await;

        assert_eq!(
            restart_rx.try_recv().expect("collector should request restart"),
            "submit failure"
        );
    }

    /// When the dispatcher has already produced a backlog of ready proofs, the
    /// collector should submit each sequentially ready target in one tick
    /// instead of sleeping `poll_interval` between successful submissions.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_collector_tick_drains_ready_backlog_after_success() {
        let submitted_games = Arc::new(AtomicU64::new(0));
        let proof_requester = Arc::new(MockProofRequester::default());
        let cancel = CancellationToken::new();
        let l1 = Arc::new(MockL1 { latest_block_number: TEST_L1_BLOCK_NUMBER });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(SUBMIT_BLOCK_INTERVAL * 3, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let anchor_registry = Arc::new(MockAnchorStateRegistry {
            anchor_root: test_anchor_root(TEST_ANCHOR_BLOCK),
            anchor_game: Address::ZERO,
        });
        let factory =
            Arc::new(AdvancingGameFactory { submitted_games: Arc::clone(&submitted_games) });
        let output_proposer =
            Arc::new(AdvancingOutputProposer { submitted_games: Arc::clone(&submitted_games) });

        let pipeline = ProvingPipeline::new(
            PipelineConfig {
                submit_timeout: Some(Duration::from_secs(60)),
                max_retries: 3,
                recovery_scan_concurrency: 8,
                tee_prover_registry_address: None,
                driver: DriverConfig {
                    game_type: TEST_GAME_TYPE,
                    block_interval: SUBMIT_BLOCK_INTERVAL,
                    intermediate_block_interval: SUBMIT_BLOCK_INTERVAL,
                    poll_interval: Duration::from_millis(10),
                    ..Default::default()
                },
            },
            Arc::clone(&proof_requester) as Arc<dyn ProofRequesterProvider>,
            l1,
            l2,
            rollup,
            anchor_registry,
            factory,
            Arc::new(MockAggregateVerifier::default()),
            output_proposer,
            cancel.clone(),
        );

        let mut dispatch_cache: Option<CachedRecovery> = None;
        let mut dispatch_state = ProofDispatcherState::new();
        pipeline.dispatcher_tick(&mut dispatch_cache, &mut dispatch_state, &cancel).await;

        assert_eq!(
            proof_requester.requests.lock().unwrap().len(),
            3,
            "test setup should dispatch three ready proofs"
        );

        let mut collect_cache: Option<CachedRecovery> = None;
        let mut collect_state = ProofCollectorState::new();
        let (restart_tx, mut restart_rx) = mpsc::channel(1);

        pipeline
            .collector_tick(
                &mut collect_cache,
                &mut collect_state,
                CollectorTickContext { cancel: &cancel, restart_tx: &restart_tx },
            )
            .await;

        assert_eq!(
            submitted_games.load(Ordering::SeqCst),
            3,
            "collector should submit every ready proof without waiting another poll tick"
        );
        assert!(restart_rx.try_recv().is_err(), "successful backlog drain should not restart");
    }

    /// `submit_inline` with a `RootMismatch` outcome drops the cached
    /// recovery but leaves retry counters untouched (transient submit
    /// failures never count against the per-target retry budget).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_submit_inline_root_mismatch_clears_cache_only() {
        // Force a final-root mismatch by overriding the canonical root for
        // the target block.
        let output_roots = HashMap::from([(SUBMIT_BLOCK_INTERVAL, B256::repeat_byte(0xFF))]);
        let (pipeline, _proof_requester, cancel) = step_pipeline_full(
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

        let effect = pipeline
            .submit_inline(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                proof,
                &mut retry_counts,
                &mut cache,
                &cancel,
            )
            .await;

        assert!(cache.is_none(), "RootMismatch should drop the recovery cache");
        assert_eq!(effect, SubmitEffect::Restart, "RootMismatch should restart the session");
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
        let (pipeline, _proof_requester, cancel) = step_pipeline_full(
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

        let effect = pipeline
            .submit_inline(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                proof,
                &mut retry_counts,
                &mut cache,
                &cancel,
            )
            .await;

        assert!(cache.is_none(), "InvalidParentGame should drop the recovery cache");
        assert_eq!(effect, SubmitEffect::Restart, "InvalidParentGame should restart the session");
        assert!(retry_counts.is_empty(), "submit failures should not bump retry counters");
    }

    /// Other transient submit failures preserve both the cache and retry
    /// counters, but restart both loops from a fresh recovery walk.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_submit_inline_transient_failure_preserves_cache() {
        let (pipeline, _proof_requester, cancel) = step_pipeline_full(
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

        let effect = pipeline
            .submit_inline(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                proof,
                &mut retry_counts,
                &mut cache,
                &cancel,
            )
            .await;

        assert!(cache.is_some(), "transient submit failures should preserve the recovery cache");
        assert_eq!(effect, SubmitEffect::Restart, "transient submit failures should restart");
        assert!(
            retry_counts.is_empty(),
            "transient submit failures should not bump retry counters"
        );
    }

    /// When `submit_inline` exceeds `submit_timeout`, neither the cache
    /// nor retry counters are mutated, but the pipeline session restarts.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_submit_inline_timeout_does_not_count_against_retries() {
        let submit_timeout = Duration::from_millis(50);
        let (pipeline, _proof_requester, cancel) = step_pipeline_full(
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

        let effect = pipeline
            .submit_inline(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                proof,
                &mut retry_counts,
                &mut cache,
                &cancel,
            )
            .await;

        assert!(cache.is_some(), "submit timeout should preserve the recovery cache");
        assert!(retry_counts.is_empty(), "submit timeout should not bump retry counters");
        assert_eq!(effect, SubmitEffect::Restart, "submit timeout should restart the session");
    }

    /// Cancellation aborts the inline submit wait immediately and restarts the
    /// session without mutating retry accounting.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_submit_inline_cancelled_does_not_count_against_retries() {
        let (pipeline, _proof_requester, cancel) = step_pipeline_full(
            HashMap::new(),
            SUBMIT_BLOCK_INTERVAL,
            3,
            Duration::from_secs(60),
            Arc::new(MockOutputProposer),
        );

        let recovered = anchor_recovered_state();
        let proof = submit_proof_result(SUBMIT_BLOCK_INTERVAL);
        let mut cache = Some(CachedRecovery { game_count: 0, state: recovered });
        let mut retry_counts: HashMap<u64, u32> = HashMap::new();
        cancel.cancel();

        let effect = pipeline
            .submit_inline(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                proof,
                &mut retry_counts,
                &mut cache,
                &cancel,
            )
            .await;

        assert!(cache.is_some(), "cancelled submit should preserve the recovery cache");
        assert!(retry_counts.is_empty(), "cancelled submit should not bump retry counters");
        assert_eq!(effect, SubmitEffect::Restart, "cancelled submit should restart the session");
    }

    /// On a successful submission `submit_inline` advances
    /// `last_proposed_block`; `last_collected_block` is advanced when the
    /// collector observes a ready proof.
    #[cfg(feature = "metrics")]
    #[test]
    fn test_submit_inline_advances_last_proposed_block_on_success() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        with_recorder(|snap| {
            rt.block_on(async {
                let (pipeline, _proof_requester, cancel) = step_pipeline_full(
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
                pipeline
                    .submit_inline(
                        SUBMIT_BLOCK_INTERVAL,
                        &recovered,
                        proof,
                        &mut retry_counts,
                        &mut cache,
                        &cancel,
                    )
                    .await;
            });

            let snapshot = snap.snapshot().into_vec();
            match find_metric(&snapshot, MetricKind::Gauge, "base_proposer.last_proposed_block") {
                Some(DebugValue::Gauge(value)) => {
                    assert_eq!(
                        value.into_inner(),
                        SUBMIT_BLOCK_INTERVAL as f64,
                        "last_proposed_block should advance to target block on success",
                    );
                }
                other => panic!("expected last_proposed_block gauge, got {other:?}"),
            }
        });
    }

    /// The collector advances `last_collected_block` when it polls a proof as
    /// ready, before attempting L1 submission.
    #[cfg(feature = "metrics")]
    #[test]
    fn test_collector_tick_advances_last_collected_block_on_ready() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        with_recorder(|snap| {
            rt.block_on(async {
                let (pipeline, proof_requester, cancel) =
                    step_pipeline_default(SUBMIT_BLOCK_INTERVAL);
                let mut dispatch_cache: Option<CachedRecovery> = None;
                let mut dispatch_state = ProofDispatcherState::new();
                pipeline.dispatcher_tick(&mut dispatch_cache, &mut dispatch_state, &cancel).await;

                assert_eq!(
                    proof_requester.requests.lock().unwrap().len(),
                    1,
                    "test setup should dispatch one ready proof"
                );

                let mut collect_cache: Option<CachedRecovery> = None;
                let mut collect_state = ProofCollectorState::new();
                let (restart_tx, _restart_rx) = mpsc::channel(1);
                pipeline
                    .collector_tick(
                        &mut collect_cache,
                        &mut collect_state,
                        CollectorTickContext { cancel: &cancel, restart_tx: &restart_tx },
                    )
                    .await;
            });

            let snapshot = snap.snapshot().into_vec();
            match find_metric(&snapshot, MetricKind::Gauge, "base_proposer.last_collected_block") {
                Some(DebugValue::Gauge(value)) => {
                    assert_eq!(
                        value.into_inner(),
                        SUBMIT_BLOCK_INTERVAL as f64,
                        "last_collected_block should advance when proof is ready",
                    );
                }
                other => panic!("expected last_collected_block gauge, got {other:?}"),
            }
        });
    }

    /// Submit timeouts are observable through a dedicated counter.
    #[cfg(feature = "metrics")]
    #[test]
    fn test_submit_inline_timeout_increments_submit_timeouts_total() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        with_recorder(|snap| {
            rt.block_on(async {
                let submit_timeout = Duration::from_millis(50);
                let (pipeline, _proof_requester, cancel) = step_pipeline_full(
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
                pipeline
                    .submit_inline(
                        SUBMIT_BLOCK_INTERVAL,
                        &recovered,
                        proof,
                        &mut retry_counts,
                        &mut cache,
                        &cancel,
                    )
                    .await;
            });

            let snapshot = snap.snapshot().into_vec();
            match find_metric(&snapshot, MetricKind::Counter, "base_proposer.submit_timeouts_total")
            {
                Some(DebugValue::Counter(value)) => {
                    assert_eq!(*value, 1, "submit_timeouts_total should increment once");
                }
                other => panic!("expected submit_timeouts_total counter, got {other:?}"),
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
                let (pipeline, _proof_requester, cancel) = step_pipeline_full(
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
                pipeline
                    .submit_inline(
                        SUBMIT_BLOCK_INTERVAL,
                        &recovered,
                        proof,
                        &mut retry_counts,
                        &mut cache,
                        &cancel,
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
        let (pipeline, _proof_requester, cancel) = step_pipeline_full(
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

        let effect = pipeline
            .submit_inline(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                proof,
                &mut retry_counts,
                &mut cache,
                &cancel,
            )
            .await;

        assert!(
            !retry_counts.contains_key(&SUBMIT_BLOCK_INTERVAL),
            "successful submit should clear the per-target retry counter"
        );
        assert!(cache.is_some(), "successful submit should refresh the cache");
        assert!(
            matches!(effect, SubmitEffect::Submitted { .. }),
            "successful submit should be Submitted"
        );
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
            _: alloy_eips::BlockNumberOrTag,
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
            _: alloy_eips::BlockNumberOrTag,
        ) -> base_proof_rpc::RpcResult<alloy_primitives::Bytes> {
            unimplemented!()
        }
        async fn call_contract(
            &self,
            _: Address,
            _: alloy_primitives::Bytes,
            _: alloy_eips::BlockNumberOrTag,
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
                submit_timeout: Some(Duration::from_secs(60)),
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

        let outcome = pipeline
            .dispatch_for(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                B256::repeat_byte(SUBMIT_BLOCK_INTERVAL as u8),
                &mut retry_counts,
                &mut cache,
                true,
            )
            .await;

        assert!(
            proof_requester.requests.lock().unwrap().is_empty(),
            "build failure should not reach the prover service"
        );
        assert!(retry_counts.is_empty(), "build failures must not bump per-target retry counters");
        assert!(cache.is_some(), "build failures must not drop the recovery cache");
        assert_eq!(outcome, DispatchOutcome::Skipped, "build failure should skip dispatch");
    }

    /// `submit_inline` with a `Discard` outcome (e.g. `L1OriginTooOld`)
    /// returns a re-dispatch effect instead of marking the target skipped.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_submit_inline_discard_requests_redispatch() {
        let (pipeline, _proof_requester, cancel) = step_pipeline_full(
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

        let effect = pipeline
            .submit_inline(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                proof,
                &mut retry_counts,
                &mut cache,
                &cancel,
            )
            .await;

        assert_eq!(
            effect,
            SubmitEffect::Redispatch {
                claimed_l2_output_root: B256::repeat_byte(SUBMIT_BLOCK_INTERVAL as u8),
            },
            "Discard outcome should request a fresh proof dispatch"
        );
        assert!(retry_counts.is_empty(), "Discard must not bump per-target retry counters");
        assert!(cache.is_some(), "Discard must not drop the recovery cache");
    }

    /// Discard retries use retry-specific session ids so a previously
    /// `Succeeded` root-derived session does not force the collector to reuse
    /// the same discarded proof forever.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_dispatch_discard_retry_uses_retry_session() {
        let (pipeline, proof_requester, _cancel) = step_pipeline_default(SUBMIT_BLOCK_INTERVAL);
        let recovered = anchor_recovered_state();
        let mut cache = Some(CachedRecovery { game_count: 0, state: recovered });
        let mut retry_counts: HashMap<u64, u32> = HashMap::new();
        let mut discard_retry_counts: HashMap<u64, u32> = HashMap::new();
        let mut retry_sessions: HashMap<u64, String> = HashMap::new();

        pipeline
            .dispatch_discard_retry(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                B256::repeat_byte(SUBMIT_BLOCK_INTERVAL as u8),
                &mut retry_counts,
                &mut cache,
                DiscardRetryState {
                    counts: &mut discard_retry_counts,
                    sessions: &mut retry_sessions,
                    count_dispatch_failure: true,
                },
            )
            .await;

        let requests = proof_requester.requests.lock().unwrap();
        let retry_session = retry_sessions
            .get(&SUBMIT_BLOCK_INTERVAL)
            .expect("discard retry should store retry session id");
        assert!(requests.contains_key(retry_session), "retry session should be dispatched");
        assert_ne!(
            retry_session,
            &ProposerProofAdapter::tee_session_id_for_root(
                B256::repeat_byte(SUBMIT_BLOCK_INTERVAL as u8),
                pipeline.proof_collector.tee_kind(),
            ),
            "discard retries must not reuse root-derived session id"
        );
    }

    /// Discard retries are capped so persistent discard reasons do not create
    /// unbounded prover-service sessions for one target.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_dispatch_discard_retry_exhaustion_drops_cache() {
        let (pipeline, proof_requester, _cancel) = step_pipeline_full(
            HashMap::new(),
            SUBMIT_BLOCK_INTERVAL,
            2,
            Duration::from_secs(60),
            Arc::new(MockOutputProposer),
        );
        let recovered = anchor_recovered_state();
        let mut cache = Some(CachedRecovery { game_count: 0, state: recovered });
        let mut retry_counts: HashMap<u64, u32> = HashMap::new();
        let mut discard_retry_counts: HashMap<u64, u32> =
            HashMap::from([(SUBMIT_BLOCK_INTERVAL, 2)]);
        let mut retry_sessions: HashMap<u64, String> =
            HashMap::from([(SUBMIT_BLOCK_INTERVAL, "stale-session".to_owned())]);

        pipeline
            .dispatch_discard_retry(
                SUBMIT_BLOCK_INTERVAL,
                &recovered,
                B256::repeat_byte(SUBMIT_BLOCK_INTERVAL as u8),
                &mut retry_counts,
                &mut cache,
                DiscardRetryState {
                    counts: &mut discard_retry_counts,
                    sessions: &mut retry_sessions,
                    count_dispatch_failure: true,
                },
            )
            .await;

        assert!(cache.is_none(), "discard exhaustion should drop the recovery cache");
        assert!(discard_retry_counts.is_empty(), "discard exhaustion should clear retry count");
        assert!(retry_sessions.is_empty(), "discard exhaustion should clear retry session");
        assert!(
            proof_requester.requests.lock().unwrap().is_empty(),
            "discard exhaustion should not dispatch another session"
        );
    }

    /// If a retry-specific session disappears from prover service, the
    /// collector should dispatch a fresh retry instead of polling the missing
    /// session forever.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_collector_tick_redispatches_missing_retry_session() {
        let (pipeline, proof_requester, cancel) = step_pipeline_default(SUBMIT_BLOCK_INTERVAL);

        let mut cache: Option<CachedRecovery> = None;
        let mut collect_state = ProofCollectorState::new();
        collect_state
            .retry_sessions
            .insert(SUBMIT_BLOCK_INTERVAL, "missing-retry-session".to_owned());
        let (restart_tx, _restart_rx) = mpsc::channel(1);

        pipeline
            .collector_tick(
                &mut cache,
                &mut collect_state,
                CollectorTickContext { cancel: &cancel, restart_tx: &restart_tx },
            )
            .await;

        let retry_session = collect_state
            .retry_sessions
            .get(&SUBMIT_BLOCK_INTERVAL)
            .expect("missing retry session should be replaced");
        assert_ne!(retry_session, "missing-retry-session");
        assert!(
            proof_requester.requests.lock().unwrap().contains_key(retry_session),
            "collector should dispatch the replacement retry session"
        );
        assert_eq!(
            collect_state.discard_retry_counts.get(&SUBMIT_BLOCK_INTERVAL).copied(),
            Some(1),
            "replacement retry should consume one discard retry attempt"
        );
    }

    /// A task panic is treated as a session restart instead of being
    /// propagated out of the pipeline runner.
    #[tokio::test(flavor = "current_thread")]
    async fn test_handle_task_result_treats_panic_as_restartable() {
        let handle = tokio::spawn(async { panic!("simulated task panic") });
        let result = handle.await;
        assert!(result.is_err(), "test setup should produce a JoinError");
        handle_task_result(TaskKind::Dispatcher, result.map(|()| Ok(())));
    }
}
