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
    allow_non_finalized: bool,
    intermediate_block_interval: u64,
    tee_image_hash: B256,
}

impl std::fmt::Debug for ProofDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofDispatcher")
            .field("proposer_address", &self.proposer_address)
            .field("allow_non_finalized", &self.allow_non_finalized)
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
            allow_non_finalized: config.allow_non_finalized,
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

    /// Builds a proof request for `target_block`.
    pub async fn build_request(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        claimed_l2_output_root: B256,
    ) -> Result<ProofRequest, ProposerError> {
        let (sync_status, agreed_l2_head) = tokio::try_join!(
            self.rollup_client.sync_status(),
            self.l2_client.header_by_number(BlockNumberOrTag::Number(recovered.l2_block_number)),
        )
        .map_err(ProposerError::Rpc)?;
        let (l1_head_source, l1_head, l2_coverage) =
            Self::select_l1_head_for_target(target_block, &sync_status, self.allow_non_finalized)?;
        let l1_header =
            self.l1_client.header_by_hash(l1_head.hash).await.map_err(ProposerError::Rpc)?;
        if l1_header.hash != l1_head.hash || l1_header.number != l1_head.number {
            return Err(ProposerError::Internal(format!(
                "selected {l1_head_source} L1 head {}:{} does not match L1 RPC header {}:{}",
                l1_head.number, l1_head.hash, l1_header.number, l1_header.hash
            )));
        }

        info!(
            target_block,
            from_block = recovered.l2_block_number,
            allow_non_finalized = self.allow_non_finalized,
            l1_head_source = l1_head_source,
            l1_head_number = l1_header.number,
            l1_head_hash = %l1_header.hash,
            l2_coverage_block = l2_coverage.number,
            l2_coverage_hash = %l2_coverage.hash,
            "Built proof request"
        );

        Ok(ProofRequest {
            l1_head: l1_header.hash,
            agreed_l2_head_hash: agreed_l2_head.hash,
            agreed_l2_output_root: recovered.output_root,
            claimed_l2_output_root,
            claimed_l2_block_number: target_block,
            proposer: self.proposer_address,
            intermediate_block_interval: self.intermediate_block_interval,
            l1_head_number: l1_header.number,
            image_hash: self.tee_image_hash,
        })
    }

    fn select_l1_head_for_target(
        target_block: u64,
        sync_status: &SyncStatus,
        allow_non_finalized: bool,
    ) -> Result<(&'static str, L1BlockRef, L2BlockRef), ProposerError> {
        let selected = if target_block <= sync_status.finalized_l2.number {
            ("finalized", sync_status.finalized_l1, sync_status.finalized_l2)
        } else if !allow_non_finalized {
            return Err(ProposerError::Internal(format!(
                "target block {target_block} is above rollup finalized head {}",
                sync_status.finalized_l2.number
            )));
        } else if target_block <= sync_status.safe_l2.number {
            ("safe", sync_status.safe_l1, sync_status.safe_l2)
        } else {
            return Err(ProposerError::Internal(format!(
                "target block {target_block} is above rollup safe head {}",
                sync_status.safe_l2.number
            )));
        };

        let (l1_head_source, l1_head, l2_coverage) = selected;
        if l1_head.number < l2_coverage.l1origin.number {
            return Err(ProposerError::Internal(format!(
                "selected {l1_head_source} L1 head {} is below {l1_head_source} L2 origin {}",
                l1_head.number, l2_coverage.l1origin.number
            )));
        }

        Ok(selected)
    }

    /// Builds and dispatches a fresh root-derived request.
    pub async fn dispatch(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        claimed_l2_output_root: B256,
    ) -> bool {
        let request =
            match self.build_request(target_block, recovered, claimed_l2_output_root).await {
                Ok(request) => request,
                Err(error) => {
                    Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_BUILD_FAILED)
                        .increment(1);
                    warn!(
                        target_block,
                        error = %error,
                        "Failed to build proof request, will retry next iteration"
                    );
                    return false;
                }
            };
        let l1_head_number = request.l1_head_number;

        let request = ProposerProofAdapter::tee_prove_block_range_request(request);
        let expected_session_id = request.proof.session_id.clone();

        match self.proof_requester.prove_block_range(request).await {
            Ok(response) if response.session_id == expected_session_id => {}
            Err(e) if e.is_l1_head_conflict_for_session(&expected_session_id) => {
                debug!(
                    session_id = %expected_session_id,
                    "prover-service already has this TEE proof session with a different l1_head"
                );
            }
            result => {
                let error = match result {
                    Ok(response) => ProposerError::Prover(format!(
                        "prover service returned mismatched session_id: expected {expected_session_id}, got {}",
                        response.session_id
                    )),
                    Err(error) => ProposerError::Prover(error.to_string()),
                };
                Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_FAILED).increment(1);
                Metrics::errors_total(error.metric_label()).increment(1);

                warn!(target_block, error = %error, "Proof dispatch failed, will retry next tick");
                return false;
            }
        }

        Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_ACCEPTED).increment(1);
        info!(
            target_block,
            session_id = %expected_session_id,
            from_block = recovered.l2_block_number,
            l1_head_number,
            "Proof request accepted by prover service"
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::atomic::Ordering};

    use base_optimism_rpc::SyncStatus;

    use super::*;
    use crate::test_utils::{
        MockL1, MockL2, MockProofRequester, MockRollupClient, test_l1_block_ref, test_l1_header,
        test_l2_block_ref,
    };

    fn dispatcher() -> (ProofDispatcher, Arc<MockProofRequester>) {
        dispatcher_with_sync(sync_status_with_distinct_heads(600, 600), false)
    }

    fn dispatcher_with_sync(
        sync_status: SyncStatus,
        allow_non_finalized: bool,
    ) -> (ProofDispatcher, Arc<MockProofRequester>) {
        let requester = Arc::new(MockProofRequester::default());
        let config = DriverConfig {
            proposer_address: Address::repeat_byte(0x04),
            allow_non_finalized,
            intermediate_block_interval: 300,
            tee_image_hash: B256::repeat_byte(0x05),
            ..Default::default()
        };
        (
            ProofDispatcher::new(
                requester.clone(),
                Arc::new(MockL1::with_headers(
                    sync_status.finalized_l1.number,
                    headers_for_sync_status(&sync_status),
                )),
                Arc::new(MockL2 { block_not_found: false, canonical_hash: None }),
                Arc::new(MockRollupClient {
                    sync_status,
                    output_roots: HashMap::new(),
                    max_safe_block: None,
                }),
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
    async fn build_request_uses_finalized_l1_head_for_finalized_target() {
        let (dispatcher, _) = dispatcher_with_sync(sync_status_with_distinct_heads(300, 600), true);

        let request =
            dispatcher.build_request(200, &recovered(), B256::repeat_byte(0xaa)).await.unwrap();

        assert_eq!(request.l1_head, B256::repeat_byte(0xf1));
        assert_eq!(request.l1_head_number, 10);
    }

    #[tokio::test]
    async fn build_request_uses_safe_l1_head_for_safe_target() {
        let (dispatcher, _) = dispatcher_with_sync(sync_status_with_distinct_heads(300, 600), true);

        let request =
            dispatcher.build_request(400, &recovered(), B256::repeat_byte(0xaa)).await.unwrap();

        assert_eq!(request.l1_head, B256::repeat_byte(0x5a));
        assert_eq!(request.l1_head_number, 20);
    }

    #[tokio::test]
    async fn build_request_rejects_safe_target_when_non_finalized_disallowed() {
        let (dispatcher, _) =
            dispatcher_with_sync(sync_status_with_distinct_heads(300, 600), false);

        let err =
            dispatcher.build_request(400, &recovered(), B256::repeat_byte(0xaa)).await.unwrap_err();

        assert!(err.to_string().contains("above rollup finalized head"));
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

        let requests = requester.requests.lock().unwrap();
        let session_id = ProposerProofAdapter::tee_session_id_for_root(B256::repeat_byte(200));
        assert_eq!(requests.len(), 3);
        assert!(requests.contains_key(&session_id));
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
