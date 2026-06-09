//! Proof request construction and dispatch helpers for proposer TEE proofs.

use std::sync::Arc;

use alloy_primitives::{Address, B256};
use base_proof_primitives::ProofRequest;
use base_proof_rpc::{L1Provider, L2Provider};
use base_prover_service_client::ProofRequesterProvider;
use tracing::info;

use crate::{
    driver::RecoveredState,
    error::ProposerError,
    proof_adapter::{DispatchedProof, ProofRequesterDispatcher, ProposerProofAdapter},
    proof_collector::ProofCollector,
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
#[derive(Clone)]
pub struct ProofDispatcher<L1, L2>
where
    L1: L1Provider,
    L2: L2Provider,
{
    dispatcher: ProofRequesterDispatcher,
    l1_client: Arc<L1>,
    l2_client: Arc<L2>,
    config: ProofDispatcherConfig,
}

impl<L1, L2> std::fmt::Debug for ProofDispatcher<L1, L2>
where
    L1: L1Provider,
    L2: L2Provider,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofDispatcher")
            .field("dispatcher", &self.dispatcher)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl<L1, L2> ProofDispatcher<L1, L2>
where
    L1: L1Provider + 'static,
    L2: L2Provider + 'static,
{
    /// Creates an AWS Nitro TEE proof dispatcher.
    pub fn aws_nitro(
        proof_requester: Arc<dyn ProofRequesterProvider>,
        l1_client: Arc<L1>,
        l2_client: Arc<L2>,
        config: ProofDispatcherConfig,
    ) -> Self {
        Self {
            dispatcher: ProofRequesterDispatcher::aws_nitro(proof_requester),
            l1_client,
            l2_client,
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
        let (l1_head_result, agreed_head_result) = tokio::join!(
            async { self.l1_client.header_by_number(None).await.map_err(ProposerError::Rpc) },
            async {
                self.l2_client
                    .header_by_number(Some(recovered.l2_block_number))
                    .await
                    .map_err(ProposerError::Rpc)
            },
        );

        let l1_head = l1_head_result?;
        let agreed_l2_head = agreed_head_result?;

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
        let request =
            match self.build_request(target_block, recovered, claimed_l2_output_root).await {
                Ok(request) => request,
                Err(error) => return ProofDispatchAttempt::BuildFailed(error),
            };

        match self.dispatcher.dispatch_tee(request).await {
            Ok(dispatched) => ProofDispatchAttempt::Accepted(dispatched),
            Err(error) => ProofDispatchAttempt::DispatchFailed(error),
        }
    }

    /// Builds and dispatches a retry-specific proof request for a discarded proof.
    pub async fn dispatch_discard_retry(
        &self,
        target_block: u64,
        recovered: &RecoveredState,
        claimed_l2_output_root: B256,
        collector: &ProofCollector<impl base_proof_rpc::RollupProvider + 'static>,
        attempt: u32,
    ) -> ProofDispatchAttempt {
        let request =
            match self.build_request(target_block, recovered, claimed_l2_output_root).await {
                Ok(request) => request,
                Err(error) => return ProofDispatchAttempt::BuildFailed(error),
            };
        let session_id = ProposerProofAdapter::tee_discard_retry_session_id(
            &request,
            collector.tee_kind(),
            attempt,
        );

        match self.dispatcher.dispatch_tee_with_session_id(request, session_id).await {
            Ok(dispatched) => ProofDispatchAttempt::Accepted(dispatched),
            Err(error) => ProofDispatchAttempt::DispatchFailed(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::test_utils::{
        MockL1, MockL2, MockProofRequester, MockRollupClient, test_sync_status,
    };

    fn dispatcher() -> (ProofDispatcher<MockL1, MockL2>, Arc<MockProofRequester>) {
        let requester = Arc::new(MockProofRequester::default());
        let l1 = Arc::new(MockL1 { latest_block_number: 1000 });
        let l2 = Arc::new(MockL2 { block_not_found: true, canonical_hash: None });
        let dispatcher = ProofDispatcher::aws_nitro(
            Arc::clone(&requester) as Arc<dyn ProofRequesterProvider>,
            l1,
            l2,
            ProofDispatcherConfig {
                proposer_address: Address::repeat_byte(0x04),
                intermediate_block_interval: 300,
                tee_image_hash: B256::repeat_byte(0x05),
            },
        );
        (dispatcher, requester)
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

        assert!(requester.requests.lock().unwrap().contains_key(&dispatched.session_id));
    }

    #[tokio::test]
    async fn dispatch_discard_retry_uses_retry_specific_session() {
        let (dispatcher, requester) = dispatcher();
        let rollup = Arc::new(MockRollupClient {
            sync_status: test_sync_status(200, B256::ZERO),
            output_roots: HashMap::new(),
            max_safe_block: None,
        });
        let collector = ProofCollector::target_poller_aws_nitro(
            Arc::clone(&requester) as Arc<dyn ProofRequesterProvider>,
            rollup,
        );
        let claimed_root = B256::repeat_byte(0xaa);

        let outcome =
            dispatcher.dispatch_discard_retry(200, &recovered(), claimed_root, &collector, 1).await;
        let ProofDispatchAttempt::Accepted(dispatched) = outcome else {
            panic!("expected accepted dispatch")
        };

        assert_ne!(
            dispatched.session_id,
            ProposerProofAdapter::tee_session_id_for_root(claimed_root, collector.tee_kind())
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
