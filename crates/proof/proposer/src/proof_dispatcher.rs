//! Proof request construction and dispatch helpers for proposer TEE proofs.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256};
use base_optimism_rpc::{L1BlockRef, L2BlockRef, SyncStatus};
use base_proof_primitives::ProofRequest;
use base_proof_rpc::{L1Provider, L2Provider, RollupProvider};
use base_prover_service_client::ProofRequesterProvider;
use tracing::{debug, info, warn};

use crate::{
    Metrics,
    driver::{DriverConfig, RecoveredState},
    error::ProposerError,
    proof_adapter::ProposerProofAdapter,
    proof_target::ProofTarget,
};

/// Builds and dispatches proposer TEE proof requests.
pub struct ProofDispatcher {
    proof_requester: Arc<dyn ProofRequesterProvider>,
    l1_client: Arc<dyn L1Provider>,
    l2_client: Arc<dyn L2Provider>,
    rollup_client: Arc<dyn RollupProvider>,
    proposer_address: Address,
    intermediate_block_interval: u64,
    tee_image_hash: B256,
}

impl std::fmt::Debug for ProofDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofDispatcher")
            .field("proposer_address", &self.proposer_address)
            .field("intermediate_block_interval", &self.intermediate_block_interval)
            .field("tee_image_hash", &self.tee_image_hash)
            .finish_non_exhaustive()
    }
}

impl ProofDispatcher {
    /// Creates a proof dispatcher.
    pub fn new(
        proof_requester: Arc<dyn ProofRequesterProvider>,
        l1_client: Arc<dyn L1Provider>,
        l2_client: Arc<dyn L2Provider>,
        rollup_client: Arc<dyn RollupProvider>,
        config: &DriverConfig,
    ) -> Self {
        Self {
            proof_requester,
            l1_client,
            l2_client,
            rollup_client,
            proposer_address: config.proposer_address,
            intermediate_block_interval: config.intermediate_block_interval,
            tee_image_hash: config.tee_image_hash,
        }
    }

    /// Dispatches every target from the current dispatcher cursor up to `safe_head`.
    pub async fn tick(
        &self,
        cursor: &mut Option<(RecoveredState, RecoveredState)>,
        recovered: RecoveredState,
        safe_head: u64,
        block_interval: u64,
    ) {
        let mut current = cursor
            .filter(|(source, _)| *source == recovered)
            .map_or(recovered, |(_, cursor)| cursor);

        loop {
            let Some(target_block) =
                ProofTarget::next_block(current.l2_block_number, block_interval)
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

            let Some(claimed_l2_output_root) = ProofTarget::canonical_output_root(
                self.rollup_client.as_ref(),
                target_block,
                "dispatcher",
            )
            .await
            else {
                break;
            };

            if self.dispatch(target_block, &current, claimed_l2_output_root).await {
                current.l2_block_number = target_block;
                current.output_root = claimed_l2_output_root;
                *cursor = Some((recovered, current));
            } else {
                break;
            }
        }
    }

    /// Builds and dispatches a fresh root-derived request.
    pub async fn dispatch(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        claimed_l2_output_root: B256,
    ) -> bool {
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
                return false;
            }
        };

        let request = ProofRequest {
            l1_head: l1_head.hash,
            agreed_l2_head_hash: agreed_l2_head.hash,
            agreed_l2_output_root: recovered.output_root,
            claimed_l2_output_root,
            claimed_l2_block_number: target_block,
            proposer: self.proposer_address,
            intermediate_block_interval: self.intermediate_block_interval,
            l1_head_number: l1_head.number,
            image_hash: self.tee_image_hash,
        };

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

                warn!(target_block, error = %error, "Proof dispatch failed, will retry next tick");
                return false;
            }
        };

        Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_ACCEPTED).increment(1);
        info!(
            target_block,
            session_id = %session_id,
            from_block = recovered.l2_block_number,
            l1_head_number = l1_head.number,
            "Proof request accepted by prover service"
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::test_utils::{MockL1, MockL2, MockProofRequester, MockRollupClient};

    fn dispatcher() -> (ProofDispatcher, Arc<MockProofRequester>) {
        let requester = Arc::new(MockProofRequester::default());
        let config = DriverConfig {
            proposer_address: Address::repeat_byte(0x04),
            intermediate_block_interval: 300,
            tee_image_hash: B256::repeat_byte(0x05),
            ..Default::default()
        };
        (
            ProofDispatcher::new(
                requester.clone(),
                Arc::new(MockL1 { latest_block_number: 1000, ..Default::default() }),
                Arc::new(MockL2 { block_not_found: false, canonical_hash: None }),
                Arc::new(MockRollupClient::default()),
                &config,
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
    async fn dispatch_sends_root_derived_session() {
        let (dispatcher, requester) = dispatcher();
        let claimed_root = B256::repeat_byte(0xaa);

        let outcome = dispatcher.dispatch(200, &recovered(), claimed_root).await;
        let session_id = ProposerProofAdapter::tee_session_id_for_root(claimed_root);

        assert!(outcome);
        assert!(requester.requests.lock().unwrap().contains_key(&session_id));
    }

    #[tokio::test]
    async fn dispatch_rejects_mismatched_session_id() {
        let (dispatcher, requester) = dispatcher();
        *requester.accepted_session_id.lock().unwrap() = Some("wrong-session".to_owned());

        let outcome = dispatcher.dispatch(200, &recovered(), B256::repeat_byte(0xaa)).await;

        assert!(!outcome);
    }

    #[tokio::test]
    async fn dispatch_accepts_existing_l1_head_conflict() {
        let (dispatcher, requester) = dispatcher();
        requester.l1_head_conflict.store(true, Ordering::SeqCst);
        let claimed_root = B256::repeat_byte(0xaa);

        let outcome = dispatcher.dispatch(200, &recovered(), claimed_root).await;

        assert!(outcome);
    }

    #[tokio::test]
    async fn tick_dispatches_all_targets_up_to_safe_head() {
        let (dispatcher, requester) = dispatcher();
        let mut cursor = None;

        dispatcher.tick(&mut cursor, recovered(), 400, 100).await;

        assert_eq!(requester.requests.lock().unwrap().len(), 3);
        assert_eq!(cursor.map(|(_, cursor)| cursor.l2_block_number), Some(400));
    }

    #[tokio::test]
    async fn tick_resets_cursor_when_recovery_rewinds() {
        let (dispatcher, requester) = dispatcher();
        let mut cursor = Some((
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
        ));

        dispatcher.tick(&mut cursor, recovered(), 200, 100).await;

        assert_eq!(cursor.map(|(source, _)| source), Some(recovered()));
        assert_eq!(cursor.map(|(_, cursor)| cursor.l2_block_number), Some(200));
        assert_eq!(requester.requests.lock().unwrap().len(), 1);
    }
}
