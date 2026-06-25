//! Proof request construction and dispatch helpers for proposer TEE proofs.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256};
use base_optimism_rpc::{L1BlockRef, SyncStatus};
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

/// Static parameters needed to build and dispatch proposer proof requests.
#[derive(Debug, Clone, Copy)]
pub struct ProofDispatcherConfig {
    /// Whether requests may target safe, non-finalized L2 blocks.
    pub allow_non_finalized: bool,
    /// Number of L2 blocks between proof targets.
    pub block_interval: u64,
    /// Address of the proposer that will submit the proof onchain.
    pub proposer_address: Address,
    /// Number of L2 blocks between intermediate output root checkpoints.
    pub intermediate_block_interval: u64,
    /// Expected TEE enclave image hash.
    pub tee_image_hash: B256,
}

impl From<&DriverConfig> for ProofDispatcherConfig {
    fn from(config: &DriverConfig) -> Self {
        Self {
            allow_non_finalized: config.allow_non_finalized,
            block_interval: config.block_interval,
            proposer_address: config.proposer_address,
            intermediate_block_interval: config.intermediate_block_interval,
            tee_image_hash: config.tee_image_hash,
        }
    }
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

    /// Builds a proof request for `target_block` using `recovered` as the agreed parent.
    async fn build_request(
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
        let (l1_head_source, l1_head) = Self::select_l1_head_for_target(
            target_block,
            &sync_status,
            self.config.allow_non_finalized,
        )?;
        let l1_header =
            self.l1_client.header_by_hash(l1_head.hash).await.map_err(ProposerError::Rpc)?;
        if l1_header.hash != l1_head.hash || l1_header.number != l1_head.number {
            return Err(ProposerError::Internal(format!(
                "selected {l1_head_source} L1 head {}:{} does not match L1 RPC header {}:{}",
                l1_head.number, l1_head.hash, l1_header.number, l1_header.hash
            )));
        }

        info!(
            from_block = recovered.l2_block_number,
            to_block = target_block,
            allow_non_finalized = self.config.allow_non_finalized,
            l1_head_source = l1_head_source,
            l1_head_number = l1_header.number,
            l1_head_hash = %l1_header.hash,
            agreed_l2_head_hash = %agreed_l2_head.hash,
            agreed_l2_output_root = %recovered.output_root,
            claimed_l2_output_root = %claimed_l2_output_root,
            "Built proof request"
        );

        Ok(ProofRequest {
            l1_head: l1_header.hash,
            agreed_l2_head_hash: agreed_l2_head.hash,
            agreed_l2_output_root: recovered.output_root,
            claimed_l2_output_root,
            claimed_l2_block_number: target_block,
            proposer: self.config.proposer_address,
            intermediate_block_interval: self.config.intermediate_block_interval,
            l1_head_number: l1_header.number,
            image_hash: self.config.tee_image_hash,
        })
    }

    fn select_l1_head_for_target(
        target_block: u64,
        sync_status: &SyncStatus,
        allow_non_finalized: bool,
    ) -> Result<(&'static str, L1BlockRef), ProposerError> {
        let (l1_head_source, l1_head, l2_coverage) =
            if target_block <= sync_status.finalized_l2.number {
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

        if l1_head.number < l2_coverage.l1origin.number {
            return Err(ProposerError::Internal(format!(
                "selected {l1_head_source} L1 head {} is below {l1_head_source} L2 origin {}",
                l1_head.number, l2_coverage.l1origin.number
            )));
        }

        Ok((l1_head_source, l1_head))
    }

    async fn dispatch_request(&self, request: ProofRequest) -> Result<String, ProposerError> {
        let request = ProposerProofAdapter::tee_prove_block_range_request(request);
        let session_id = request.proof.session_id.clone();
        match self.proof_requester.prove_block_range(request).await {
            Ok(response) if response.session_id == session_id => Ok(response.session_id),
            Ok(response) => Err(ProposerError::Prover(format!(
                "prover service returned mismatched session_id: expected {session_id}, got {}",
                response.session_id
            ))),
            Err(e) if e.is_l1_head_conflict_for_session(&session_id) => {
                debug!(
                    session_id = %session_id,
                    "prover-service already has this TEE proof session with a different l1_head"
                );
                Ok(session_id)
            }
            Err(e) => Err(ProposerError::Prover(e.to_string())),
        }
    }

    /// Dispatches every target from the current dispatcher cursor up to `safe_head`.
    pub async fn tick(&self, current: &mut RecoveredState, safe_head: u64) {
        loop {
            let Some(target_block) =
                ProofTarget::next_block(current.l2_block_number, self.config.block_interval)
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

            let Some(claimed_l2_output_root) =
                ProofTarget::canonical_output_root(self.rollup_client.as_ref(), target_block).await
            else {
                break;
            };

            let request =
                match self.build_request(target_block, current, claimed_l2_output_root).await {
                    Ok(request) => request,
                    Err(error) => {
                        Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_BUILD_FAILED)
                            .increment(1);
                        warn!(
                            target_block,
                            error = %error,
                            "Failed to build proof request, will retry next iteration"
                        );
                        break;
                    }
                };

            match self.dispatch_request(request).await {
                Ok(session_id) => {
                    Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_ACCEPTED).increment(1);
                    info!(
                        target_block,
                        session_id = %session_id,
                        from_block = current.l2_block_number,
                        "Proof request accepted by prover service"
                    );
                    current.l2_block_number = target_block;
                    current.output_root = claimed_l2_output_root;
                }
                Err(error) => {
                    Metrics::proof_dispatch_total(Metrics::DISPATCH_OUTCOME_FAILED).increment(1);
                    Metrics::errors_total(error.metric_label()).increment(1);

                    warn!(
                        target_block,
                        error = %error,
                        "Proof dispatch failed, stopping tick at current cursor"
                    );
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_primitives::Address;

    use super::*;
    use crate::test_utils::{
        MockL1, MockL2, MockProofRequester, MockRollupClient, test_l1_block_ref, test_l1_header,
        test_l2_block_ref, test_sync_status,
    };

    fn dispatcher(requester: Arc<MockProofRequester>) -> ProofDispatcher {
        let sync_status = test_sync_status(10_000, B256::ZERO);
        ProofDispatcher::new(
            requester,
            Arc::new(MockL1::new(sync_status.finalized_l1.number)),
            Arc::new(MockL2 { block_not_found: false, canonical_hash: None }),
            Arc::new(MockRollupClient {
                sync_status,
                output_roots: Default::default(),
                max_safe_block: None,
            }),
            ProofDispatcherConfig::from(&DriverConfig {
                block_interval: 100,
                ..Default::default()
            }),
        )
    }

    fn sync_status_with_distinct_heads(
        finalized_l2_number: u64,
        safe_l2_number: u64,
    ) -> SyncStatus {
        let mut sync_status = test_sync_status(safe_l2_number, B256::repeat_byte(0x52));
        sync_status.finalized_l1 = test_l1_block_ref(10);
        sync_status.finalized_l1.hash = B256::repeat_byte(0xf1);
        sync_status.safe_l1 = test_l1_block_ref(20);
        sync_status.safe_l1.hash = B256::repeat_byte(0x5a);
        sync_status.finalized_l2 = test_l2_block_ref(finalized_l2_number, B256::repeat_byte(0xf2));
        sync_status.finalized_l2.l1origin.number = sync_status.finalized_l1.number;
        sync_status.safe_l2.l1origin.number = sync_status.safe_l1.number;
        sync_status
    }

    fn headers_for_sync_status(
        sync_status: &SyncStatus,
    ) -> HashMap<B256, alloy_rpc_types_eth::Header> {
        [sync_status.finalized_l1, sync_status.safe_l1]
            .into_iter()
            .map(|l1_head| (l1_head.hash, test_l1_header(l1_head.hash, l1_head.number)))
            .collect()
    }

    #[test]
    fn select_l1_head_selects_expected_head_or_rejects_target() {
        let sync_status = sync_status_with_distinct_heads(300, 600);
        let (source, finalized_l1) =
            ProofDispatcher::select_l1_head_for_target(200, &sync_status, true).unwrap();
        assert_eq!(source, "finalized");
        assert_eq!(finalized_l1.hash, B256::repeat_byte(0xf1));
        assert_eq!(finalized_l1.number, 10);

        let (source, safe_l1) =
            ProofDispatcher::select_l1_head_for_target(400, &sync_status, true).unwrap();
        assert_eq!(source, "safe");
        assert_eq!(safe_l1.hash, B256::repeat_byte(0x5a));
        assert_eq!(safe_l1.number, 20);

        let err = ProofDispatcher::select_l1_head_for_target(700, &sync_status, true).unwrap_err();
        assert!(err.to_string().contains("above rollup safe head"));

        let err = ProofDispatcher::select_l1_head_for_target(400, &sync_status, false).unwrap_err();
        assert!(err.to_string().contains("above rollup finalized head"));
    }

    #[test]
    fn select_l1_head_rejects_l2_coverage_beyond_selected_l1_head() {
        let mut sync_status = sync_status_with_distinct_heads(300, 600);
        sync_status.safe_l2.l1origin.number = sync_status.safe_l1.number + 1;

        let err = ProofDispatcher::select_l1_head_for_target(400, &sync_status, true)
            .expect_err("safe L1 must cover selected safe L2 origin");

        assert!(err.to_string().contains("below safe L2 origin"));
    }

    #[tokio::test]
    async fn build_request_rejects_l1_rpc_header_mismatch() {
        let sync_status = sync_status_with_distinct_heads(300, 600);
        let mut headers = headers_for_sync_status(&sync_status);
        headers.insert(
            sync_status.safe_l1.hash,
            test_l1_header(sync_status.safe_l1.hash, sync_status.safe_l1.number + 1),
        );
        let dispatcher = ProofDispatcher::new(
            Arc::new(MockProofRequester::default()),
            Arc::new(MockL1::with_headers(sync_status.finalized_l1.number, headers)),
            Arc::new(MockL2 { block_not_found: false, canonical_hash: None }),
            Arc::new(MockRollupClient {
                sync_status,
                output_roots: Default::default(),
                max_safe_block: None,
            }),
            ProofDispatcherConfig {
                allow_non_finalized: true,
                block_interval: 100,
                proposer_address: Address::repeat_byte(0x04),
                intermediate_block_interval: 300,
                tee_image_hash: B256::repeat_byte(0x05),
            },
        );
        let recovered = RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::repeat_byte(0x03),
            l2_block_number: 100,
        };

        let err = dispatcher
            .build_request(400, &recovered, B256::repeat_byte(0xaa))
            .await
            .expect_err("L1 RPC header must match rollup-selected L1 head");

        assert!(err.to_string().contains("does not match L1 RPC header"));
    }

    #[tokio::test]
    async fn dispatch_request_rejects_mismatched_session_id() {
        let requester = Arc::new(MockProofRequester::default());
        *requester.accepted_session_id.lock().unwrap() = Some("wrong-session".to_owned());
        let dispatcher = dispatcher(requester);

        let request =
            ProofRequest { claimed_l2_output_root: B256::repeat_byte(0xaa), ..Default::default() };
        let error = dispatcher.dispatch_request(request).await.expect_err("dispatch should fail");

        let ProposerError::Prover(message) = error else {
            panic!("expected mismatched session id to fail dispatch")
        };
        assert!(message.contains("mismatched session_id"));
    }

    #[tokio::test]
    async fn dispatch_request_accepts_existing_l1_head_conflict() {
        let requester = Arc::new(MockProofRequester::default());
        requester.reject_l1_head_conflict.store(true, std::sync::atomic::Ordering::SeqCst);
        let dispatcher = dispatcher(requester);

        let request =
            ProofRequest { claimed_l2_output_root: B256::repeat_byte(0xaa), ..Default::default() };
        dispatcher.dispatch_request(request).await.expect("dispatch should accept");
    }

    #[tokio::test]
    async fn tick_dispatches_all_targets_up_to_safe_head() {
        let requester = Arc::new(MockProofRequester::default());
        let dispatcher = dispatcher(Arc::clone(&requester));
        let mut current = RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::repeat_byte(0x03),
            l2_block_number: 100,
        };

        dispatcher.tick(&mut current, 400).await;

        assert_eq!(requester.requests.lock().unwrap().len(), 3);
        assert_eq!(current.l2_block_number, 400);
    }
}
