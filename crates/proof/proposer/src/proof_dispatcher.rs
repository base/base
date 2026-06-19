//! Proof request construction and dispatch helpers for proposer TEE proofs.

use std::{collections::HashMap, sync::Arc};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256};
use base_optimism_rpc::{L1BlockRef, L2BlockRef, SyncStatus};
use base_proof_primitives::ProofRequest;
use base_proof_rpc::{L1Provider, L2Provider, RollupProvider};
use base_prover_service_client::ProofRequesterProvider;
use base_prover_service_protocol::TeeKind;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    Metrics,
    driver::RecoveredState,
    error::ProposerError,
    proof_adapter::{DispatchedProof, ProofRequesterDispatcher, ProposerProofAdapter},
};

/// Static parameters needed to build proposer proof requests.
#[derive(Debug, Clone, Copy)]
pub struct ProofDispatcherConfig {
    /// Address of the proposer that will submit the proof on-chain.
    pub proposer_address: Address,
    /// Number of L2 blocks between intermediate output root checkpoints.
    pub intermediate_block_interval: u64,
    /// Expected TEE enclave image hash.
    pub tee_image_hash: B256,
}

/// Runtime parameters for dispatcher orchestration.
#[derive(Debug, Clone, Copy)]
pub struct ProofDispatcherRuntimeConfig {
    /// Number of L2 blocks between output proposals.
    pub block_interval: u64,
    /// Maximum dispatch/proof failures before asking the caller to drop recovery state.
    pub max_retries: u32,
}

/// Mutable dispatcher-side orchestration state.
#[derive(Debug, Default)]
pub struct ProofDispatcherState {
    /// Recovered chain state that the current cursor was derived from.
    pub recovered: Option<RecoveredState>,
    /// Latest block the dispatcher has sent proof requests through.
    pub cursor: Option<RecoveredState>,
    /// Per-target proof/dispatch retry counts.
    pub retry_counts: HashMap<u64, u32>,
}

/// Result of a dispatcher tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofDispatcherTickResult {
    /// True when the retry policy was exhausted and recovery should be refreshed.
    pub drop_recovery_cache: bool,
}

/// Outcome of a single target dispatch attempt after retry accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofDispatchOutcome {
    /// The request was accepted by prover-service.
    Accepted,
    /// The request failed and exhausted the retry budget.
    RetryExhausted,
    /// The request was skipped because it could not be built or dispatched.
    Skipped,
}

/// Outcome of attempting to dispatch a proof request.
#[derive(Debug)]
pub enum ProofDispatchAttempt {
    /// The request was accepted by prover-service.
    Accepted(DispatchedProof),
    /// The request could not be built from local RPC data.
    BuildFailed(ProposerError),
    /// The request reached prover-service but dispatch failed.
    DispatchFailed(ProposerError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum L1HeadSource {
    Finalized,
    Safe,
}

impl L1HeadSource {
    const fn label(self) -> &'static str {
        match self {
            Self::Finalized => "finalized",
            Self::Safe => "safe",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ProofHeadSelection {
    l1: L1BlockRef,
    l2: L2BlockRef,
    source: L1HeadSource,
}

impl ProofHeadSelection {
    fn for_target(
        target_block: u64,
        sync_status: &SyncStatus,
        allow_non_finalized: bool,
    ) -> Result<Self, ProposerError> {
        if target_block <= sync_status.finalized_l2.number {
            return Ok(Self {
                l1: sync_status.finalized_l1,
                l2: sync_status.finalized_l2,
                source: L1HeadSource::Finalized,
            });
        }

        if !allow_non_finalized {
            return Err(ProposerError::Internal(format!(
                "target block {target_block} is above rollup finalized head {}",
                sync_status.finalized_l2.number
            )));
        }

        if target_block <= sync_status.safe_l2.number {
            return Ok(Self {
                l1: sync_status.safe_l1,
                l2: sync_status.safe_l2,
                source: L1HeadSource::Safe,
            });
        }

        Err(ProposerError::Internal(format!(
            "target block {target_block} is above rollup safe head {}",
            sync_status.safe_l2.number
        )))
    }
}

/// Builds and dispatches proposer TEE proof requests.
pub struct ProofDispatcher<L1, L2, R>
where
    L1: L1Provider,
    L2: L2Provider,
    R: RollupProvider,
{
    dispatcher: ProofRequesterDispatcher,
    l1_client: Arc<L1>,
    l2_client: Arc<L2>,
    rollup_client: Arc<R>,
    config: ProofDispatcherConfig,
    allow_non_finalized: bool,
}

impl<L1, L2, R> std::fmt::Debug for ProofDispatcher<L1, L2, R>
where
    L1: L1Provider,
    L2: L2Provider,
    R: RollupProvider,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofDispatcher")
            .field("dispatcher", &self.dispatcher)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl<L1, L2, R> Clone for ProofDispatcher<L1, L2, R>
where
    L1: L1Provider,
    L2: L2Provider,
    R: RollupProvider,
{
    fn clone(&self) -> Self {
        Self {
            dispatcher: self.dispatcher.clone(),
            l1_client: Arc::clone(&self.l1_client),
            l2_client: Arc::clone(&self.l2_client),
            rollup_client: Arc::clone(&self.rollup_client),
            config: self.config,
            allow_non_finalized: self.allow_non_finalized,
        }
    }
}

impl<L1, L2, R> ProofDispatcher<L1, L2, R>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
    R: RollupProvider + 'static,
{
    /// Creates an AWS Nitro TEE proof dispatcher.
    pub fn aws_nitro(
        proof_requester: Arc<dyn ProofRequesterProvider>,
        l1_client: Arc<L1>,
        l2_client: Arc<L2>,
        rollup_client: Arc<R>,
        config: ProofDispatcherConfig,
    ) -> Self {
        Self {
            dispatcher: ProofRequesterDispatcher::aws_nitro(proof_requester),
            l1_client,
            l2_client,
            rollup_client,
            config,
            allow_non_finalized: false,
        }
    }

    /// Allows dispatching proof requests for safe L2 targets above the finalized L2 head.
    pub const fn with_allow_non_finalized(mut self, allow_non_finalized: bool) -> Self {
        self.allow_non_finalized = allow_non_finalized;
        self
    }

    /// Returns the inner prover-service dispatcher.
    pub const fn requester_dispatcher(&self) -> &ProofRequesterDispatcher {
        &self.dispatcher
    }

    /// Builds a proof request for `target_block` using `recovered` as the agreed parent.
    pub async fn build_request(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        claimed_l2_output_root: B256,
    ) -> Result<ProofRequest, ProposerError> {
        let (sync_status, agreed_l2_head) = tokio::try_join!(
            async { self.rollup_client.sync_status().await.map_err(ProposerError::Rpc) },
            async {
                self.l2_client
                    .header_by_number(BlockNumberOrTag::Number(recovered.l2_block_number))
                    .await
                    .map_err(ProposerError::Rpc)
            },
        )?;
        let head_selection =
            ProofHeadSelection::for_target(target_block, &sync_status, self.allow_non_finalized)?;
        self.l1_client.header_by_hash(head_selection.l1.hash).await.map_err(ProposerError::Rpc)?;

        let request = ProofRequest {
            l1_head: head_selection.l1.hash,
            agreed_l2_head_hash: agreed_l2_head.hash,
            agreed_l2_output_root: recovered.output_root,
            claimed_l2_output_root,
            claimed_l2_block_number: target_block,
            proposer: self.config.proposer_address,
            intermediate_block_interval: self.config.intermediate_block_interval,
            l1_head_number: head_selection.l1.number,
            image_hash: self.config.tee_image_hash,
        };

        info!(
            from_block = recovered.l2_block_number,
            to_block = target_block,
            allow_non_finalized = self.allow_non_finalized,
            l1_head_source = head_selection.source.label(),
            l1_head_number = head_selection.l1.number,
            l1_head_hash = %head_selection.l1.hash,
            l2_coverage_block = head_selection.l2.number,
            l2_coverage_hash = %head_selection.l2.hash,
            finalized_l1_number = sync_status.finalized_l1.number,
            finalized_l1_hash = %sync_status.finalized_l1.hash,
            finalized_l2_number = sync_status.finalized_l2.number,
            finalized_l2_hash = %sync_status.finalized_l2.hash,
            safe_l1_number = sync_status.safe_l1.number,
            safe_l1_hash = %sync_status.safe_l1.hash,
            safe_l2_number = sync_status.safe_l2.number,
            safe_l2_hash = %sync_status.safe_l2.hash,
            agreed_l2_head_hash = %agreed_l2_head.hash,
            agreed_l2_output_root = %recovered.output_root,
            claimed_l2_output_root = %claimed_l2_output_root,
            "Built proof request"
        );

        Ok(request)
    }

    /// Builds and dispatches a root-derived proof request for `target_block`.
    pub async fn dispatch_for(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        claimed_l2_output_root: B256,
    ) -> ProofDispatchAttempt {
        let expected_session_id = ProposerProofAdapter::tee_session_id_for_root(
            claimed_l2_output_root,
            self.dispatcher.tee_kind(),
        );
        let request =
            match self.build_request(target_block, recovered, claimed_l2_output_root).await {
                Ok(request) => request,
                Err(error) => return ProofDispatchAttempt::BuildFailed(error),
            };

        match self.dispatcher.dispatch_tee(request).await {
            Ok(dispatched) if dispatched.session_id == expected_session_id => {
                ProofDispatchAttempt::Accepted(dispatched)
            }
            Ok(dispatched) => ProofDispatchAttempt::DispatchFailed(ProposerError::Prover(format!(
                "prover service returned mismatched session_id: expected {}, got {}",
                expected_session_id, dispatched.session_id
            ))),
            Err(error) => ProofDispatchAttempt::DispatchFailed(error),
        }
    }

    /// Builds and dispatches a retry-specific proof request for a discarded proof.
    pub async fn dispatch_discard_retry(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        claimed_l2_output_root: B256,
        tee_kind: TeeKind,
        attempt: u32,
    ) -> ProofDispatchAttempt {
        let request =
            match self.build_request(target_block, recovered, claimed_l2_output_root).await {
                Ok(request) => request,
                Err(error) => return ProofDispatchAttempt::BuildFailed(error),
            };
        let session_id =
            ProposerProofAdapter::tee_discard_retry_session_id(&request, tee_kind, attempt);
        let expected_session_id = session_id.clone();

        match self.dispatcher.dispatch_tee_with_session_id(request, session_id).await {
            Ok(dispatched) if dispatched.session_id == expected_session_id => {
                ProofDispatchAttempt::Accepted(dispatched)
            }
            Ok(dispatched) => ProofDispatchAttempt::DispatchFailed(ProposerError::Prover(format!(
                "prover service returned mismatched session_id: expected {}, got {}",
                expected_session_id, dispatched.session_id
            ))),
            Err(error) => ProofDispatchAttempt::DispatchFailed(error),
        }
    }

    /// Dispatches every target from the current dispatcher cursor up to `safe_head`.
    pub async fn tick(
        &self,
        state: &mut ProofDispatcherState,
        recovered: RecoveredState,
        safe_head: u64,
        runtime: ProofDispatcherRuntimeConfig,
        cancel: &CancellationToken,
    ) -> ProofDispatcherTickResult {
        state.retry_counts.retain(|&target, _| target > recovered.l2_block_number);

        if state.recovered != Some(recovered) || state.cursor.is_none() {
            state.recovered = Some(recovered);
            state.cursor = Some(recovered);
        }

        let mut current = state.cursor.expect("dispatcher cursor initialized from recovery");
        let mut drop_recovery_cache = false;

        loop {
            if cancel.is_cancelled() {
                break;
            }

            let Some(target_block) =
                Self::next_target_block(current.l2_block_number, runtime.block_interval)
            else {
                break;
            };
            if target_block > safe_head {
                debug!(
                    current_block = current.l2_block_number,
                    target_block,
                    safe_head,
                    "Safe head below dispatch target, waiting for L2 head to advance"
                );
                break;
            }

            let Some(claimed_l2_output_root) = self.canonical_output_root(target_block).await
            else {
                break;
            };

            match self
                .dispatch_with_retry(
                    target_block,
                    &current,
                    claimed_l2_output_root,
                    state,
                    runtime.max_retries,
                    true,
                )
                .await
            {
                ProofDispatchOutcome::Accepted => {
                    current.l2_block_number = target_block;
                    current.output_root = claimed_l2_output_root;
                    state.cursor = Some(current);
                }
                ProofDispatchOutcome::RetryExhausted => {
                    drop_recovery_cache = true;
                    break;
                }
                ProofDispatchOutcome::Skipped => break,
            }
        }

        Metrics::pipeline_retries().set(state.retry_counts.values().sum::<u32>() as f64);
        ProofDispatcherTickResult { drop_recovery_cache }
    }

    /// Builds and dispatches a fresh root-derived request with retry accounting.
    pub async fn dispatch_with_retry(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        claimed_l2_output_root: B256,
        state: &mut ProofDispatcherState,
        max_retries: u32,
        count_dispatch_failure: bool,
    ) -> ProofDispatchOutcome {
        match self.dispatch_for(target_block, recovered, claimed_l2_output_root).await {
            ProofDispatchAttempt::Accepted(dispatched) => {
                info!(
                    target_block,
                    session_id = %dispatched.session_id,
                    from_block = recovered.l2_block_number,
                    "Proof request accepted by prover service"
                );
                Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_ACCEPTED).increment(1);
                ProofDispatchOutcome::Accepted
            }
            ProofDispatchAttempt::BuildFailed(error) => {
                warn!(
                    target_block,
                    error = %error,
                    "Failed to build proof request, will retry next iteration"
                );
                Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_BUILD_FAILED).increment(1);
                ProofDispatchOutcome::Skipped
            }
            ProofDispatchAttempt::DispatchFailed(error) => {
                Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_FAILED).increment(1);
                if count_dispatch_failure {
                    if state.handle_proof_failure(target_block, error, max_retries) {
                        ProofDispatchOutcome::Skipped
                    } else {
                        ProofDispatchOutcome::RetryExhausted
                    }
                } else {
                    warn!(
                        target_block,
                        error = %error,
                        "Immediate re-dispatch failed after failed proof session"
                    );
                    ProofDispatchOutcome::Skipped
                }
            }
        }
    }

    /// Fetches the canonical output root for a dispatch target.
    pub async fn canonical_output_root(&self, target_block: u64) -> Option<B256> {
        match self.rollup_client.output_at_block(target_block).await {
            Ok(output) => Some(output.output_root),
            Err(e) => {
                warn!(
                    target_block,
                    error = %e,
                    "Failed to fetch canonical output root for dispatch target"
                );
                None
            }
        }
    }

    /// Computes the next dispatch target from a current block and interval.
    pub fn next_target_block(current_block: u64, block_interval: u64) -> Option<u64> {
        if block_interval == 0 {
            error!("Block interval must be non-zero");
            return None;
        }

        current_block.checked_add(block_interval).map_or_else(
            || {
                error!(current_block, block_interval, "Overflow computing next target block");
                None
            },
            Some,
        )
    }
}

impl ProofDispatcherState {
    /// Creates empty dispatcher state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a proof/dispatch failure and returns whether retrying is allowed.
    pub fn handle_proof_failure(
        &mut self,
        target: u64,
        error: ProposerError,
        max_retries: u32,
    ) -> bool {
        Metrics::errors_total(error.metric_label()).increment(1);
        Metrics::proof_retries_total().increment(1);

        let count = self.retry_counts.entry(target).or_insert(0);
        *count += 1;
        if *count >= max_retries {
            error!(
                target_block = target,
                attempts = *count,
                error = %error,
                "Proof failed after max retries, dropping cached recovery"
            );
            self.retry_counts.remove(&target);
            self.recovered = None;
            self.cursor = None;
            false
        } else {
            warn!(
                target_block = target,
                attempt = *count,
                error = %error,
                "Proof failed, re-dispatching"
            );
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use async_trait::async_trait;
    use base_prover_service_client::ProverServiceClientError;
    use base_prover_service_protocol::{
        GetProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse,
        ProveBlockRangeRequest, ProveBlockRangeResponse,
    };

    use super::*;
    use crate::test_utils::{
        MockL1, MockL2, MockProofRequester, MockRollupClient, test_l1_block_ref, test_l2_block_ref,
        test_sync_status,
    };

    #[derive(Debug)]
    struct MismatchedProofRequester {
        session_id: String,
    }

    #[async_trait]
    impl ProofRequesterProvider for MismatchedProofRequester {
        async fn prove_block_range(
            &self,
            _request: ProveBlockRangeRequest,
        ) -> Result<ProveBlockRangeResponse, ProverServiceClientError> {
            Ok(ProveBlockRangeResponse { session_id: self.session_id.clone() })
        }

        async fn get_proof(
            &self,
            _request: GetProofRequest,
        ) -> Result<GetProofResponse, ProverServiceClientError> {
            unimplemented!("dispatcher tests do not poll proofs")
        }

        async fn list_proofs(
            &self,
            _request: ListProofsRequest,
        ) -> Result<ListProofsResponse, ProverServiceClientError> {
            unimplemented!("dispatcher tests do not list proofs")
        }
    }

    fn dispatcher() -> (ProofDispatcher<MockL1, MockL2, MockRollupClient>, Arc<MockProofRequester>)
    {
        let requester = Arc::new(MockProofRequester::default());
        let dispatcher =
            dispatcher_for_requester(Arc::clone(&requester) as Arc<dyn ProofRequesterProvider>);
        (dispatcher, requester)
    }

    fn dispatcher_for_requester(
        requester: Arc<dyn ProofRequesterProvider>,
    ) -> ProofDispatcher<MockL1, MockL2, MockRollupClient> {
        dispatcher_for_requester_and_sync(requester, test_sync_status(10_000, B256::ZERO))
    }

    fn dispatcher_for_requester_and_sync(
        requester: Arc<dyn ProofRequesterProvider>,
        sync_status: SyncStatus,
    ) -> ProofDispatcher<MockL1, MockL2, MockRollupClient> {
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: false, canonical_hash: None });
        let rollup = Arc::new(MockRollupClient {
            sync_status,
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        ProofDispatcher::aws_nitro(
            requester,
            l1,
            l2,
            rollup,
            ProofDispatcherConfig {
                proposer_address: Address::repeat_byte(0x04),
                intermediate_block_interval: 300,
                tee_image_hash: B256::repeat_byte(0x05),
            },
        )
    }

    fn sync_status_with_distinct_heads(finalized_l2: u64, safe_l2: u64) -> SyncStatus {
        let mut finalized_l1 = test_l1_block_ref(10);
        finalized_l1.hash = B256::repeat_byte(0xf1);
        let mut safe_l1 = test_l1_block_ref(20);
        safe_l1.hash = B256::repeat_byte(0x5a);
        let finalized_l2 = test_l2_block_ref(finalized_l2, B256::repeat_byte(0xf2));
        let safe_l2 = test_l2_block_ref(safe_l2, B256::repeat_byte(0x52));

        SyncStatus {
            current_l1: safe_l1,
            current_l1_finalized: Some(finalized_l1),
            head_l1: safe_l1,
            safe_l1,
            finalized_l1,
            unsafe_l2: safe_l2,
            safe_l2,
            finalized_l2,
            pending_safe_l2: None,
        }
    }

    fn recovered() -> RecoveredState {
        RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::repeat_byte(0x03),
            l2_block_number: 100,
        }
    }

    #[tokio::test]
    async fn build_request_uses_finalized_l1_head_for_finalized_target() {
        let requester: Arc<dyn ProofRequesterProvider> = Arc::new(MockProofRequester::default());
        let dispatcher =
            dispatcher_for_requester_and_sync(requester, sync_status_with_distinct_heads(300, 600))
                .with_allow_non_finalized(true);

        let request = dispatcher
            .build_request(200, &recovered(), B256::repeat_byte(0xaa))
            .await
            .expect("request should build for finalized target");

        assert_eq!(request.l1_head, B256::repeat_byte(0xf1));
        assert_eq!(request.l1_head_number, 10);
    }

    #[tokio::test]
    async fn build_request_uses_safe_l1_head_for_safe_target() {
        let requester: Arc<dyn ProofRequesterProvider> = Arc::new(MockProofRequester::default());
        let dispatcher =
            dispatcher_for_requester_and_sync(requester, sync_status_with_distinct_heads(300, 600))
                .with_allow_non_finalized(true);

        let request = dispatcher
            .build_request(400, &recovered(), B256::repeat_byte(0xaa))
            .await
            .expect("request should build for safe target");

        assert_eq!(request.l1_head, B256::repeat_byte(0x5a));
        assert_eq!(request.l1_head_number, 20);
    }

    #[tokio::test]
    async fn build_request_rejects_target_above_safe_l2() {
        let requester: Arc<dyn ProofRequesterProvider> = Arc::new(MockProofRequester::default());
        let dispatcher =
            dispatcher_for_requester_and_sync(requester, sync_status_with_distinct_heads(300, 600))
                .with_allow_non_finalized(true);

        let err = dispatcher
            .build_request(700, &recovered(), B256::repeat_byte(0xaa))
            .await
            .expect_err("target above safe L2 should not build");

        assert!(err.to_string().contains("above rollup safe head"));
    }

    #[tokio::test]
    async fn build_request_rejects_safe_target_when_non_finalized_disallowed() {
        let requester: Arc<dyn ProofRequesterProvider> = Arc::new(MockProofRequester::default());
        let dispatcher =
            dispatcher_for_requester_and_sync(requester, sync_status_with_distinct_heads(300, 600));

        let err = dispatcher
            .build_request(400, &recovered(), B256::repeat_byte(0xaa))
            .await
            .expect_err("non-finalized target should not build when disabled");

        assert!(err.to_string().contains("above rollup finalized head"));
    }

    #[tokio::test]
    async fn dispatch_for_sends_root_derived_session() {
        let (dispatcher, requester) = dispatcher();
        let claimed_root = B256::repeat_byte(0xaa);

        let outcome = dispatcher.dispatch_for(200, &recovered(), claimed_root).await;
        let ProofDispatchAttempt::Accepted(dispatched) = outcome else {
            panic!("expected accepted dispatch")
        };

        assert_eq!(
            dispatched.session_id,
            ProposerProofAdapter::tee_session_id_for_root(claimed_root, TeeKind::AwsNitro)
        );
        assert!(requester.requests.lock().unwrap().contains_key(&dispatched.session_id));
    }

    #[tokio::test]
    async fn dispatch_for_rejects_mismatched_session_id() {
        let dispatcher = dispatcher_for_requester(Arc::new(MismatchedProofRequester {
            session_id: "wrong-session".to_owned(),
        }));

        let outcome = dispatcher.dispatch_for(200, &recovered(), B256::repeat_byte(0xaa)).await;

        let ProofDispatchAttempt::DispatchFailed(ProposerError::Prover(message)) = outcome else {
            panic!("expected mismatched session id to fail dispatch")
        };
        assert!(message.contains("mismatched session_id"));
    }

    #[tokio::test]
    async fn tick_dispatches_all_targets_up_to_safe_head() {
        let (dispatcher, requester) = dispatcher();
        let mut state = ProofDispatcherState::new();
        let cancel = CancellationToken::new();

        let result = dispatcher
            .tick(
                &mut state,
                recovered(),
                400,
                ProofDispatcherRuntimeConfig { block_interval: 100, max_retries: 3 },
                &cancel,
            )
            .await;

        assert!(!result.drop_recovery_cache);
        assert_eq!(requester.requests.lock().unwrap().len(), 3);
        assert_eq!(state.cursor.map(|cursor| cursor.l2_block_number), Some(400));
        assert!(state.retry_counts.is_empty());
    }

    #[tokio::test]
    async fn tick_resets_cursor_when_recovery_rewinds() {
        let (dispatcher, requester) = dispatcher();
        let cancel = CancellationToken::new();
        let mut state = ProofDispatcherState {
            recovered: Some(RecoveredState {
                parent_address: Address::repeat_byte(0x01),
                output_root: B256::repeat_byte(0x01),
                l2_block_number: 300,
            }),
            cursor: Some(RecoveredState {
                parent_address: Address::repeat_byte(0x02),
                output_root: B256::repeat_byte(0x02),
                l2_block_number: 500,
            }),
            retry_counts: HashMap::new(),
        };

        let result = dispatcher
            .tick(
                &mut state,
                recovered(),
                200,
                ProofDispatcherRuntimeConfig { block_interval: 100, max_retries: 3 },
                &cancel,
            )
            .await;

        assert!(!result.drop_recovery_cache);
        assert_eq!(state.recovered, Some(recovered()));
        assert_eq!(state.cursor.map(|cursor| cursor.l2_block_number), Some(200));
        assert_eq!(requester.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn next_target_block_returns_none_for_zero_interval() {
        assert_eq!(
            ProofDispatcher::<MockL1, MockL2, MockRollupClient>::next_target_block(100, 0),
            None
        );
    }

    #[test]
    fn handle_proof_failure_clears_cursor_on_retry_exhaustion() {
        let mut state = ProofDispatcherState::new();
        state.cursor = Some(RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::repeat_byte(0x09),
            l2_block_number: 300,
        });
        state.retry_counts.insert(200, 1);

        let should_retry = state.handle_proof_failure(200, ProposerError::Prover("boom".into()), 2);

        assert!(!should_retry);
        assert!(state.recovered.is_none());
        assert!(state.cursor.is_none());
        assert!(!state.retry_counts.contains_key(&200));
    }

    #[tokio::test]
    async fn dispatch_discard_retry_uses_retry_specific_session() {
        let (dispatcher, _requester) = dispatcher();
        let claimed_root = B256::repeat_byte(0xaa);

        let outcome = dispatcher
            .dispatch_discard_retry(200, &recovered(), claimed_root, TeeKind::AwsNitro, 1)
            .await;
        let ProofDispatchAttempt::Accepted(dispatched) = outcome else {
            panic!("expected accepted dispatch")
        };

        assert_ne!(
            dispatched.session_id,
            ProposerProofAdapter::tee_session_id_for_root(claimed_root, TeeKind::AwsNitro)
        );
    }

    #[test]
    fn config_is_copyable() {
        let config = ProofDispatcherConfig {
            proposer_address: Address::ZERO,
            intermediate_block_interval: 1,
            tee_image_hash: B256::ZERO,
        };
        let _copy = config;
    }
}
