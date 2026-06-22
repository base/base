//! Proof request construction and dispatch helpers for proposer TEE proofs.

use std::{collections::HashMap, sync::Arc};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256};
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
    /// Address of the proposer that will submit the proof onchain.
    pub proposer_address: Address,
    /// Number of L2 blocks between intermediate output root checkpoints.
    pub intermediate_block_interval: u64,
    /// Expected TEE enclave image hash.
    pub tee_image_hash: B256,
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

/// Builds and dispatches proposer TEE proof requests.
///
/// This type intentionally holds only shared clients and static config. Mutable
/// cursor and retry state belongs in [`ProofDispatcherState`] so cloned
/// dispatchers do not diverge.
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
        }
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
        let (l1_head, agreed_l2_head) = tokio::try_join!(
            async {
                self.l1_client
                    .header_by_number(BlockNumberOrTag::Finalized)
                    .await
                    .map_err(ProposerError::Rpc)
            },
            async {
                self.l2_client
                    .header_by_number(BlockNumberOrTag::Number(recovered.l2_block_number))
                    .await
                    .map_err(ProposerError::Rpc)
            },
        )?;

        let request = ProofRequest {
            l1_head: l1_head.hash,
            agreed_l2_head_hash: agreed_l2_head.hash,
            agreed_l2_output_root: recovered.output_root,
            claimed_l2_output_root,
            claimed_l2_block_number: target_block,
            proposer: self.config.proposer_address,
            intermediate_block_interval: self.config.intermediate_block_interval,
            l1_head_number: l1_head.number,
            image_hash: self.config.tee_image_hash,
        };

        info!(
            from_block = recovered.l2_block_number,
            to_block = target_block,
            l1_head_number = l1_head.number,
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
        block_interval: u64,
        max_retries: u32,
        cancel: &CancellationToken,
    ) -> bool {
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
                Self::next_target_block(current.l2_block_number, block_interval)
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
                    max_retries,
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
        drop_recovery_cache
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
        MockL1, MockL2, MockProofRequester, MockRollupClient, test_sync_status,
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
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: false, canonical_hash: None });
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(0, B256::ZERO),
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

    fn recovered() -> RecoveredState {
        RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::repeat_byte(0x03),
            l2_block_number: 100,
        }
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

        let result = dispatcher.tick(&mut state, recovered(), 400, 100, 3, &cancel).await;

        assert!(!result);
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

        let result = dispatcher.tick(&mut state, recovered(), 200, 100, 3, &cancel).await;

        assert!(!result);
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
