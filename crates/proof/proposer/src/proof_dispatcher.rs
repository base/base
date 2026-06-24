//! Proof request construction and dispatch helpers for proposer TEE proofs.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256};
use base_optimism_rpc::{L1BlockRef, L2BlockRef, SyncStatus};
use base_proof_primitives::ProofRequest;
use base_proof_rpc::{L1Provider, L2Provider, RollupProvider};
use base_prover_service_client::ProofRequesterProvider;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    Metrics, driver::RecoveredState, error::ProposerError, proof_adapter::ProposerProofAdapter,
    proof_target::ProofTarget,
};

/// Static parameters needed to build proposer proof requests.
#[derive(Debug, Clone, Copy)]
pub struct ProofDispatcherConfig {
    /// Address of the proposer that will submit the proof onchain.
    pub proposer_address: Address,
    /// Whether requests may target safe, non-finalized L2 blocks.
    pub allow_non_finalized: bool,
    /// Number of L2 blocks between intermediate output root checkpoints.
    pub intermediate_block_interval: u64,
    /// Expected TEE enclave image hash.
    pub tee_image_hash: B256,
}

/// Mutable dispatcher-side orchestration state.
#[derive(Debug, Default)]
pub struct ProofDispatcherState {
    /// Recovery source and latest block the dispatcher has sent proof requests through.
    pub cursor: Option<(RecoveredState, RecoveredState)>,
    /// Active proof/dispatch retry count.
    pub retry: Option<(u64, u32)>,
}

/// Builds and dispatches proposer TEE proof requests.
pub struct ProofDispatcher {
    proof_requester: Arc<dyn ProofRequesterProvider>,
    l1_client: Arc<dyn L1Provider>,
    l2_client: Arc<dyn L2Provider>,
    rollup_client: Arc<dyn RollupProvider>,
    config: ProofDispatcherConfig,
}

impl std::fmt::Debug for ProofDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofDispatcher").field("config", &self.config).finish_non_exhaustive()
    }
}

impl ProofDispatcher {
    /// Creates a proof dispatcher.
    pub fn new(
        proof_requester: Arc<dyn ProofRequesterProvider>,
        l1_client: Arc<dyn L1Provider>,
        l2_client: Arc<dyn L2Provider>,
        rollup_client: Arc<dyn RollupProvider>,
        config: ProofDispatcherConfig,
    ) -> Self {
        Self { proof_requester, l1_client, l2_client, rollup_client, config }
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
        if matches!(state.retry, Some((target, _)) if target <= recovered.l2_block_number) {
            state.retry = None;
        }

        let mut current = state
            .cursor
            .filter(|(source, _)| *source == recovered)
            .map_or(recovered, |(_, cursor)| cursor);
        let drop_recovery_cache = loop {
            if cancel.is_cancelled() {
                break false;
            }

            let Some(target_block) =
                ProofTarget::next_block(current.l2_block_number, block_interval)
            else {
                break false;
            };
            if target_block > safe_head {
                debug!(
                    current_block = current.l2_block_number,
                    target_block,
                    safe_head,
                    "Safe head below dispatch target, waiting for L2 head to advance"
                );
                break false;
            }

            let Some(claimed_l2_output_root) = ProofTarget::canonical_output_root(
                self.rollup_client.as_ref(),
                target_block,
                "dispatcher",
            )
            .await
            else {
                break false;
            };

            match self
                .dispatch_with_retry(
                    target_block,
                    &current,
                    claimed_l2_output_root,
                    state,
                    max_retries,
                )
                .await
            {
                Ok(true) => {
                    current.l2_block_number = target_block;
                    current.output_root = claimed_l2_output_root;
                    state.cursor = Some((recovered, current));
                }
                Err(()) => break true,
                Ok(false) => break false,
            }
        };

        Metrics::pipeline_retries().set(state.retry.map_or(0, |(_, count)| count) as f64);
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
    ) -> Result<bool, ()> {
        let (l1_head, agreed_l2_head) = match tokio::try_join!(
            self.l1_client.header_by_number(BlockNumberOrTag::Finalized),
            self.l2_client.header_by_number(BlockNumberOrTag::Number(recovered.l2_block_number)),
        ) {
            Ok(headers) => headers,
            Err(error) => {
                Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_BUILD_FAILED).increment(1);
                let error = ProposerError::Rpc(error);
                warn!(
                    target_block,
                    error = %error,
                    "Failed to build proof request, will retry next iteration"
                );
                return Ok(false);
            }
        };

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

        let request = ProposerProofAdapter::tee_prove_block_range_request(request);
        let expected_session_id = request.proof.session_id.clone();

        let session_id = match self.proof_requester.prove_block_range(request).await {
            Ok(response) if response.session_id == expected_session_id => Ok(response.session_id),
            Err(e) if e.is_l1_head_conflict_for_session(&expected_session_id) => {
                debug!(
                    session_id = %expected_session_id,
                    "prover-service already has this TEE proof session with a different l1_head"
                );
                Ok(expected_session_id)
            }
            Ok(response) => Err(ProposerError::Prover(format!(
                "prover service returned mismatched session_id: expected {expected_session_id}, got {}",
                response.session_id
            ))),
            Err(error) => Err(ProposerError::Prover(error.to_string())),
        };
        let session_id = match session_id {
            Ok(session_id) => session_id,
            Err(error) => {
                Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_FAILED).increment(1);
                Metrics::errors_total(error.metric_label()).increment(1);
                Metrics::proof_retries_total().increment(1);

                let count = match state.retry {
                    Some((retry_target, count)) if retry_target == target_block => count + 1,
                    _ => 1,
                };
                state.retry = Some((target_block, count));

                if count >= max_retries {
                    error!(
                        target_block,
                        attempts = count,
                        error = %error,
                        "Proof failed after max retries, dropping cached recovery"
                    );
                    state.retry = None;
                    state.cursor = None;
                    return Err(());
                }

                warn!(
                    target_block,
                    attempt = count,
                    error = %error,
                    "Proof failed, re-dispatching"
                );
                return Ok(false);
            }
        };

        debug!(session_id = %session_id, "dispatched TEE proof request");
        Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_ACCEPTED).increment(1);
        info!(
            target_block,
            session_id = %session_id,
            from_block = recovered.l2_block_number,
            "Proof request accepted by prover service"
        );
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::test_utils::{MockL1, MockL2, MockProofRequester, MockRollupClient};

    fn dispatcher() -> (ProofDispatcher, Arc<MockProofRequester>) {
        let requester = Arc::new(MockProofRequester::default());
        (
            ProofDispatcher::new(
                requester.clone(),
                Arc::new(MockL1 { latest_block_number: 1000, ..Default::default() }),
                Arc::new(MockL2 { block_not_found: false, canonical_hash: None }),
                Arc::new(MockRollupClient::default()),
                ProofDispatcherConfig {
                    proposer_address: Address::repeat_byte(0x04),
                    intermediate_block_interval: 300,
                    tee_image_hash: B256::repeat_byte(0x05),
                },
            ),
            requester,
        )
    }

    fn headers_for_sync_status(
        sync_status: &SyncStatus,
    ) -> HashMap<B256, alloy_rpc_types_eth::Header> {
        let mut headers = HashMap::new();
        for l1_head in [sync_status.finalized_l1, sync_status.safe_l1] {
            headers.insert(l1_head.hash, test_l1_header(l1_head.hash, l1_head.number));
        }
        headers
    }

    fn sync_status_with_distinct_heads(finalized_l2: u64, safe_l2: u64) -> SyncStatus {
        let mut finalized_l1 = test_l1_block_ref(10);
        finalized_l1.hash = B256::repeat_byte(0xf1);
        let mut safe_l1 = test_l1_block_ref(20);
        safe_l1.hash = B256::repeat_byte(0x5a);
        let mut finalized_l2 = test_l2_block_ref(finalized_l2, B256::repeat_byte(0xf2));
        finalized_l2.l1origin.hash = finalized_l1.hash;
        finalized_l2.l1origin.number = finalized_l1.number;
        let mut safe_l2 = test_l2_block_ref(safe_l2, B256::repeat_byte(0x52));
        safe_l2.l1origin.hash = safe_l1.hash;
        safe_l2.l1origin.number = safe_l1.number;

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
    async fn dispatch_with_retry_sends_root_derived_session() {
        let (dispatcher, requester) = dispatcher();
        let claimed_root = B256::repeat_byte(0xaa);
        let mut state = ProofDispatcherState::default();

        let outcome =
            dispatcher.dispatch_with_retry(200, &recovered(), claimed_root, &mut state, 3).await;
        let session_id = ProposerProofAdapter::tee_session_id_for_root(claimed_root);

        assert_eq!(outcome, Ok(true));
        assert!(requester.requests.lock().unwrap().contains_key(&session_id));
        assert_eq!(state.retry, None);
    }

    #[tokio::test]
    async fn dispatch_with_retry_rejects_mismatched_session_id() {
        let (dispatcher, requester) = dispatcher();
        *requester.accepted_session_id.lock().unwrap() = Some("wrong-session".to_owned());
        let mut state = ProofDispatcherState::default();

        let outcome = dispatcher
            .dispatch_with_retry(200, &recovered(), B256::repeat_byte(0xaa), &mut state, 2)
            .await;

        assert_eq!(outcome, Ok(false));
        assert_eq!(state.retry, Some((200, 1)));
    }

    #[tokio::test]
    async fn dispatch_with_retry_accepts_existing_l1_head_conflict() {
        let (dispatcher, requester) = dispatcher();
        requester.l1_head_conflict.store(true, Ordering::SeqCst);
        let claimed_root = B256::repeat_byte(0xaa);
        let mut state = ProofDispatcherState::default();

        let outcome =
            dispatcher.dispatch_with_retry(200, &recovered(), claimed_root, &mut state, 3).await;

        assert_eq!(outcome, Ok(true));
        assert_eq!(state.retry, None);
    }

    #[tokio::test]
    async fn tick_dispatches_all_targets_up_to_safe_head() {
        let (dispatcher, requester) = dispatcher();
        let mut state = ProofDispatcherState::default();
        let cancel = CancellationToken::new();

        let result = dispatcher.tick(&mut state, recovered(), 400, 100, 3, &cancel).await;

        assert!(!result);
        assert_eq!(requester.requests.lock().unwrap().len(), 3);
        assert_eq!(state.cursor.map(|(_, cursor)| cursor.l2_block_number), Some(400));
        assert_eq!(state.retry, None);
    }

    #[tokio::test]
    async fn tick_resets_cursor_when_recovery_rewinds() {
        let (dispatcher, requester) = dispatcher();
        let cancel = CancellationToken::new();
        let mut state = ProofDispatcherState {
            cursor: Some((
                RecoveredState {
                    parent_address: Address::repeat_byte(0x01),
                    output_root: B256::repeat_byte(0x01),
                    l2_block_number: 300,
                },
                RecoveredState {
                    parent_address: Address::repeat_byte(0x02),
                    output_root: B256::repeat_byte(0x02),
                    l2_block_number: 500,
                },
            )),
            retry: None,
        };

        let result = dispatcher.tick(&mut state, recovered(), 200, 100, 3, &cancel).await;

        assert!(!result);
        assert_eq!(state.cursor.map(|(source, _)| source), Some(recovered()));
        assert_eq!(state.cursor.map(|(_, cursor)| cursor.l2_block_number), Some(200));
        assert_eq!(requester.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn dispatch_with_retry_clears_cursor_on_retry_exhaustion() {
        let (dispatcher, requester) = dispatcher();
        *requester.accepted_session_id.lock().unwrap() = Some("wrong-session".to_owned());
        let mut state = ProofDispatcherState {
            cursor: Some((
                recovered(),
                RecoveredState {
                    parent_address: Address::ZERO,
                    output_root: B256::repeat_byte(0x09),
                    l2_block_number: 300,
                },
            )),
            retry: Some((200, 1)),
        };

        let outcome = dispatcher
            .dispatch_with_retry(200, &recovered(), B256::repeat_byte(0xaa), &mut state, 2)
            .await;

        assert_eq!(outcome, Err(()));
        assert!(state.cursor.is_none());
        assert_eq!(state.retry, None);
    }
}
