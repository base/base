//! Proof request construction and dispatch helpers for proposer TEE proofs.

use std::sync::Arc;

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256};
use base_optimism_rpc::{L1BlockRef, SyncStatus};
use base_proof_primitives::ProofRequest;
use base_proof_rpc::{L1Provider, L2Provider, RollupProvider};
use base_prover_service_client::ProofRequesterProvider;
use base_prover_service_protocol::{
    DeleteProofRequest, GetProofRequest, PROOF_REQUEST_CANCELLED_MESSAGE, ProofStatus,
};
use tracing::{debug, info, instrument, warn};

use crate::{
    Metrics, driver::RecoveredState, error::ProposerError, proof_adapter::ProposerProofAdapter,
    proof_target::ProofTarget, proposal_intervals::IntervalResolver,
};

/// Builds and dispatches proposer TEE proof requests.
pub struct ProofDispatcher {
    proof_requester: Arc<dyn ProofRequesterProvider>,
    l1_client: Arc<dyn L1Provider>,
    l2_client: Arc<dyn L2Provider>,
    rollup_client: Arc<dyn RollupProvider>,
    intervals: Arc<IntervalResolver>,
    proposer_address: Address,
}

impl std::fmt::Debug for ProofDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofDispatcher")
            .field("proposer_address", &self.proposer_address)
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
        intervals: Arc<IntervalResolver>,
        proposer_address: Address,
    ) -> Self {
        Self { proof_requester, l1_client, l2_client, rollup_client, intervals, proposer_address }
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
        let l1_head = Self::select_l1_head_for_target(target_block, &sync_status)?;
        let l1_header =
            self.l1_client.header_by_hash(l1_head.hash).await.map_err(ProposerError::Rpc)?;
        if l1_header.hash != l1_head.hash || l1_header.number != l1_head.number {
            return Err(ProposerError::Internal(format!(
                "selected finalized L1 head {}:{} does not match L1 RPC header {}:{}",
                l1_head.number, l1_head.hash, l1_header.number, l1_header.hash
            )));
        }

        info!(
            from_block = recovered.l2_block_number,
            to_block = target_block,
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
            proposer: self.proposer_address,
            l1_head_number: l1_header.number,
            schedule_l2_block_number: None,
        })
    }

    fn select_l1_head_for_target(
        target_block: u64,
        sync_status: &SyncStatus,
    ) -> Result<L1BlockRef, ProposerError> {
        if target_block > sync_status.finalized_l2.number {
            return Err(ProposerError::Internal(format!(
                "target block {target_block} is above rollup finalized head {}",
                sync_status.finalized_l2.number
            )));
        }

        let l1_head = sync_status.finalized_l1;
        let l1_origin = sync_status.finalized_l2.l1origin.number;
        if l1_head.number < l1_origin {
            return Err(ProposerError::Internal(format!(
                "selected finalized L1 head {} is below finalized L2 origin {l1_origin}",
                l1_head.number
            )));
        }

        Ok(l1_head)
    }

    #[instrument(name = "proposer.dispatch_request", skip_all, fields(session_id), err(Display))]
    async fn dispatch_request(&self, request: ProofRequest) -> Result<String, ProposerError> {
        let session_id =
            ProposerProofAdapter::tee_session_id_for_root(request.claimed_l2_output_root);
        tracing::Span::current().record("session_id", session_id.as_str());
        let mut reset_attempted = false;

        loop {
            let rpc_request = ProposerProofAdapter::tee_prove_block_range_request_with_session_id(
                request.clone(),
                session_id.clone(),
            );
            match self.proof_requester.prove_block_range(rpc_request).await {
                Ok(response) if response.session_id == session_id => return Ok(response.session_id),
                Ok(response) => {
                    return Err(ProposerError::Prover(format!(
                        "prover service returned mismatched session_id: expected {session_id}, got {}",
                        response.session_id
                    )));
                }
                Err(e) if e.is_l1_head_conflict_for_session(&session_id) => {
                    debug!(
                        session_id = %session_id,
                        "prover-service already has this TEE proof session with a different l1_head"
                    );
                    return Ok(session_id);
                }
                Err(e) if e.is_retry_exhausted() && !reset_attempted => {
                    // Failed sessions otherwise hold the dispatcher behind the collector,
                    // forcing the entire exhausted backlog to be reproved serially.
                    match self
                        .proof_requester
                        .get_proof(GetProofRequest { session_id: session_id.clone() })
                        .await
                    {
                        Ok(response) if response.status == ProofStatus::Failed => {
                            if response.error_message.as_deref()
                                == Some(PROOF_REQUEST_CANCELLED_MESSAGE)
                            {
                                return Err(ProposerError::Prover(e.to_string()));
                            }
                            self.proof_requester
                                .delete_proof_request(DeleteProofRequest {
                                    session_id: session_id.clone(),
                                })
                                .await
                                .map_err(|error| ProposerError::Prover(error.to_string()))?;
                            info!(
                                session_id = %session_id,
                                "Deleted exhausted proof request for redispatch"
                            );
                        }
                        // Another recovery path may already have requeued or completed it.
                        Ok(_) => return Ok(session_id),
                        // The collector may have deleted it between dispatch and inspection.
                        Err(error) if error.is_not_found() => {}
                        Err(error) => return Err(ProposerError::Prover(error.to_string())),
                    }
                    // At most one reset and resubmission per dispatch. A second terminal
                    // rejection backs off through the normal dispatcher tick interval.
                    reset_attempted = true;
                }
                Err(e) => return Err(ProposerError::Prover(e.to_string())),
            }
        }
    }

    /// Dispatches every target from the current dispatcher cursor up to `finalized_head`.
    pub async fn tick(&self, current: &mut RecoveredState, finalized_head: u64) {
        loop {
            // Resolved per target: the interval changes at the Denim activation
            // block, so the cursor's own starting block picks the cadence.
            let intervals = match self.intervals.for_starting_block(current.l2_block_number).await {
                Ok(intervals) => intervals,
                Err(error) => {
                    Metrics::errors_total(error.metric_label()).increment(1);
                    warn!(
                        starting_block = current.l2_block_number,
                        error = %error,
                        "Failed to resolve proposal intervals, stopping tick at current cursor"
                    );
                    break;
                }
            };

            let Some(target_block) =
                ProofTarget::next_block(current.l2_block_number, intervals.block_interval)
            else {
                break;
            };

            if target_block > finalized_head {
                debug!(
                    current_block = current.l2_block_number,
                    target_block,
                    finalized_head,
                    "Finalized head below dispatch target, waiting for L2 head to advance"
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
    use async_trait::async_trait;
    use base_prover_service_client::ProverServiceClientError;
    use base_prover_service_protocol::{
        DeleteProofsByTeeSignerRequest, GetProofResponse, ListProofsRequest, ListProofsResponse,
        PROOF_REQUEST_NOT_FOUND_MESSAGE, ProveBlockRangeRequest, ProveBlockRangeResponse,
    };
    use jsonrpsee::{core::client::Error as JsonRpcClientError, types::ErrorObjectOwned};
    use mockall::Sequence;

    use super::*;
    use crate::test_utils::{
        MockL1, MockL2, MockProofRequester, MockRollupClient, test_fixed_interval_resolver,
        test_l1_block_ref, test_l1_header, test_l2_block_ref, test_sync_status,
    };

    mockall::mock! {
        Requester {}

        #[async_trait]
        impl ProofRequesterProvider for Requester {
            async fn prove_block_range(
                &self,
                request: ProveBlockRangeRequest,
            ) -> Result<ProveBlockRangeResponse, ProverServiceClientError>;
            async fn get_proof(
                &self,
                request: GetProofRequest,
            ) -> Result<GetProofResponse, ProverServiceClientError>;
            async fn delete_proof_request(
                &self,
                request: DeleteProofRequest,
            ) -> Result<(), ProverServiceClientError>;
            async fn delete_proofs_by_tee_signer(
                &self,
                request: DeleteProofsByTeeSignerRequest,
            ) -> Result<u64, ProverServiceClientError>;
            async fn delete_proofs_by_unregistered_tee_signers(
                &self,
                request: base_prover_service_protocol::DeleteProofsByUnregisteredTeeSignersRequest,
            ) -> Result<u64, ProverServiceClientError>;
            async fn list_proofs(
                &self,
                request: ListProofsRequest,
            ) -> Result<ListProofsResponse, ProverServiceClientError>;
        }
    }

    fn rpc_error(code: i32, message: &str) -> ProverServiceClientError {
        JsonRpcClientError::Call(ErrorObjectOwned::owned(code, message, None::<()>)).into()
    }

    fn exhausted_error() -> ProverServiceClientError {
        rpc_error(
            ProverServiceClientError::ERROR_RESOURCE_EXHAUSTED,
            "proof request retry budget exhausted",
        )
    }

    fn cursor() -> RecoveredState {
        RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::repeat_byte(0x03),
            l2_block_number: 100,
        }
    }

    fn dispatcher(requester: Arc<dyn ProofRequesterProvider>) -> ProofDispatcher {
        let sync_status = test_sync_status(10_000, B256::ZERO);
        ProofDispatcher::new(
            requester,
            Arc::new(MockL1::new(sync_status.finalized_l1.number)),
            Arc::new(MockL2),
            Arc::new(MockRollupClient {
                sync_status,
                output_roots: Default::default(),
                max_safe_block: None,
            }),
            test_fixed_interval_resolver(100, 100),
            Address::ZERO,
        )
    }

    fn sync_status_with_finalized_head(finalized_l2_number: u64) -> SyncStatus {
        let mut sync_status = test_sync_status(finalized_l2_number, B256::repeat_byte(0x52));
        sync_status.finalized_l1 = test_l1_block_ref(10);
        sync_status.finalized_l1.hash = B256::repeat_byte(0xf1);
        sync_status.finalized_l2 = test_l2_block_ref(finalized_l2_number, B256::repeat_byte(0xf2));
        sync_status.finalized_l2.l1origin.number = sync_status.finalized_l1.number;
        sync_status
    }

    #[test]
    fn select_l1_head_selects_finalized_head_or_rejects_target() {
        let sync_status = sync_status_with_finalized_head(300);

        let finalized_l1 = ProofDispatcher::select_l1_head_for_target(200, &sync_status).unwrap();
        assert_eq!(finalized_l1.hash, B256::repeat_byte(0xf1));
        assert_eq!(finalized_l1.number, 10);

        let err = ProofDispatcher::select_l1_head_for_target(400, &sync_status).unwrap_err();
        assert!(err.to_string().contains("above rollup finalized head"));
    }

    #[test]
    fn select_l1_head_rejects_l2_origin_beyond_finalized_l1_head() {
        let mut sync_status = sync_status_with_finalized_head(300);
        sync_status.finalized_l2.l1origin.number = sync_status.finalized_l1.number + 1;

        let err = ProofDispatcher::select_l1_head_for_target(200, &sync_status)
            .expect_err("finalized L1 must cover finalized L2 origin");

        assert!(err.to_string().contains("below finalized L2 origin"));
    }

    #[tokio::test]
    async fn build_request_rejects_l1_rpc_header_mismatch() {
        let sync_status = sync_status_with_finalized_head(300);
        // Map the finalized L1 head hash to a header with a mismatched number to
        // force the RPC-header mismatch path.
        let headers = HashMap::from([(
            sync_status.finalized_l1.hash,
            test_l1_header(sync_status.finalized_l1.hash, sync_status.finalized_l1.number + 1),
        )]);
        let dispatcher = ProofDispatcher::new(
            Arc::new(MockProofRequester::default()),
            Arc::new(MockL1 {
                latest_block_number: sync_status.finalized_l1.number,
                headers_by_hash: headers,
            }),
            Arc::new(MockL2),
            Arc::new(MockRollupClient {
                sync_status,
                output_roots: Default::default(),
                max_safe_block: None,
            }),
            test_fixed_interval_resolver(100, 100),
            Address::repeat_byte(0x04),
        );
        let recovered = RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::repeat_byte(0x03),
            l2_block_number: 100,
        };

        let err = dispatcher
            .build_request(200, &recovered, B256::repeat_byte(0xaa))
            .await
            .expect_err("L1 RPC header must match rollup-selected L1 head");

        assert!(err.to_string().contains("does not match L1 RPC header"));
    }

    #[tokio::test]
    async fn dispatch_request_rejects_mismatched_session_id() {
        let requester =
            Arc::new(MockProofRequester { return_wrong_session_id: true, ..Default::default() });
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
        let requester =
            Arc::new(MockProofRequester { reject_l1_head_conflict: true, ..Default::default() });
        let dispatcher = dispatcher(requester);

        let request =
            ProofRequest { claimed_l2_output_root: B256::repeat_byte(0xaa), ..Default::default() };
        dispatcher.dispatch_request(request).await.expect("dispatch should accept");
    }

    #[tokio::test]
    async fn tick_dispatches_all_targets_up_to_finalized_head() {
        let requester = Arc::new(MockProofRequester::default());
        let dispatcher = dispatcher(Arc::<MockProofRequester>::clone(&requester));
        let mut current = RecoveredState {
            parent_address: Address::ZERO,
            output_root: B256::repeat_byte(0x03),
            l2_block_number: 100,
        };

        dispatcher.tick(&mut current, 400).await;

        assert_eq!(requester.requests.lock().unwrap().len(), 3);
        assert_eq!(current.l2_block_number, 400);
    }

    #[tokio::test]
    async fn tick_requeues_multiple_exhausted_targets_without_waiting_for_collector() {
        let mut requester = MockRequester::new();
        let mut sequence = Sequence::new();
        let mut expected_parent = cursor().output_root;

        for target in [200, 300, 400] {
            let root = B256::repeat_byte(target as u8);
            let session_id = ProposerProofAdapter::tee_session_id_for_root(root);
            for attempt in 0..2 {
                let expected_session_id = session_id.clone();
                requester
                    .expect_prove_block_range()
                    .withf(move |request| {
                        let base_prover_service_protocol::ProofRequestKind::Tee(tee) =
                            &request.proof.request
                        else {
                            return false;
                        };
                        request.proof.session_id == expected_session_id
                            && tee.proof.claimed_l2_block_number == target
                            && tee.proof.agreed_l2_output_root == expected_parent
                            && tee.proof.l1_head_number == 1000
                            && request.retry_failed
                    })
                    .times(1)
                    .in_sequence(&mut sequence)
                    .returning(move |request| {
                        if attempt == 0 {
                            Err(exhausted_error())
                        } else {
                            Ok(ProveBlockRangeResponse { session_id: request.proof.session_id })
                        }
                    });
                if attempt == 0 {
                    let expected_session_id = session_id.clone();
                    requester
                        .expect_get_proof()
                        .withf(move |request| request.session_id == expected_session_id)
                        .times(1)
                        .in_sequence(&mut sequence)
                        .returning(|_| {
                            Ok(GetProofResponse {
                                status: ProofStatus::Failed,
                                error_message: Some(
                                    "Worker claim expired after exhausting 5 attempts".into(),
                                ),
                                result: None,
                            })
                        });
                    let expected_session_id = session_id.clone();
                    requester
                        .expect_delete_proof_request()
                        .withf(move |request| request.session_id == expected_session_id)
                        .times(1)
                        .in_sequence(&mut sequence)
                        .returning(|_| Ok(()));
                }
            }
            expected_parent = root;
        }

        let mut current = cursor();
        dispatcher(Arc::new(requester)).tick(&mut current, 400).await;

        assert_eq!(current.l2_block_number, 400);
        assert_eq!(current.output_root, expected_parent);
    }

    #[tokio::test]
    async fn tick_preserves_sessions_already_recovered_by_another_caller() {
        for status in [ProofStatus::Queued, ProofStatus::Running, ProofStatus::Succeeded] {
            let mut requester = MockRequester::new();
            requester.expect_prove_block_range().times(1).returning(|_| Err(exhausted_error()));
            requester.expect_get_proof().times(1).returning(move |_| {
                Ok(GetProofResponse { status, error_message: None, result: None })
            });
            requester.expect_delete_proof_request().never();

            let mut current = cursor();
            dispatcher(Arc::new(requester)).tick(&mut current, 200).await;
            assert_eq!(current.l2_block_number, 200);
        }
    }

    #[tokio::test]
    async fn tick_does_not_reset_cancelled_sessions_or_other_rpc_failures() {
        for code in [
            ProverServiceClientError::ERROR_RESOURCE_EXHAUSTED,
            ProverServiceClientError::ERROR_FAILED_PRECONDITION,
            ProverServiceClientError::ERROR_UNAVAILABLE,
        ] {
            let mut requester = MockRequester::new();
            requester
                .expect_prove_block_range()
                .times(1)
                .returning(move |_| Err(rpc_error(code, "rejected")));
            if code == ProverServiceClientError::ERROR_RESOURCE_EXHAUSTED {
                requester.expect_get_proof().times(1).returning(|_| {
                    Ok(GetProofResponse {
                        status: ProofStatus::Failed,
                        error_message: Some(PROOF_REQUEST_CANCELLED_MESSAGE.into()),
                        result: None,
                    })
                });
            } else {
                requester.expect_get_proof().never();
            }
            requester.expect_delete_proof_request().never();

            let mut current = cursor();
            dispatcher(Arc::new(requester)).tick(&mut current, 200).await;
            assert_eq!(current.l2_block_number, 100);
        }
    }

    #[tokio::test]
    async fn tick_stays_at_current_cursor_when_inspection_or_deletion_fails() {
        for reject_inspection in [true, false] {
            let mut requester = MockRequester::new();
            requester.expect_prove_block_range().times(1).returning(|_| Err(exhausted_error()));
            requester.expect_get_proof().times(1).returning(move |_| {
                if reject_inspection {
                    Err(ProverServiceClientError::from(JsonRpcClientError::RequestTimeout))
                } else {
                    Ok(GetProofResponse {
                        status: ProofStatus::Failed,
                        error_message: None,
                        result: None,
                    })
                }
            });
            if reject_inspection {
                requester.expect_delete_proof_request().never();
            } else {
                requester.expect_delete_proof_request().times(1).returning(|_| {
                    Err(ProverServiceClientError::from(JsonRpcClientError::RequestTimeout))
                });
            }

            let mut current = cursor();
            dispatcher(Arc::new(requester)).tick(&mut current, 200).await;
            assert_eq!(current.l2_block_number, 100);
        }
    }

    #[tokio::test]
    async fn tick_retries_once_if_collector_already_deleted_exhausted_session() {
        let mut requester = MockRequester::new();
        let mut sequence = Sequence::new();
        requester
            .expect_prove_block_range()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|_| Err(exhausted_error()));
        requester.expect_get_proof().times(1).in_sequence(&mut sequence).returning(|_| {
            Err(rpc_error(
                ProverServiceClientError::ERROR_NOT_FOUND,
                PROOF_REQUEST_NOT_FOUND_MESSAGE,
            ))
        });
        requester.expect_delete_proof_request().never();
        requester.expect_prove_block_range().times(1).in_sequence(&mut sequence).returning(
            |request| Ok(ProveBlockRangeResponse { session_id: request.proof.session_id }),
        );

        let mut current = cursor();
        dispatcher(Arc::new(requester)).tick(&mut current, 200).await;
        assert_eq!(current.l2_block_number, 200);
    }

    #[tokio::test]
    async fn tick_limits_exhausted_session_recovery_to_one_reset_per_dispatch() {
        let mut requester = MockRequester::new();
        requester.expect_prove_block_range().times(2).returning(|_| Err(exhausted_error()));
        requester.expect_get_proof().times(1).returning(|_| {
            Ok(GetProofResponse { status: ProofStatus::Failed, error_message: None, result: None })
        });
        requester.expect_delete_proof_request().times(1).returning(|_| Ok(()));

        let mut current = cursor();
        dispatcher(Arc::new(requester)).tick(&mut current, 200).await;
        assert_eq!(current.l2_block_number, 100);
    }
}
