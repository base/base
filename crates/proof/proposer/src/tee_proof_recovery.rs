//! Proactive recovery of completed proofs from unregistered TEE signers.

use std::{collections::HashSet, sync::Arc, time::Duration};

use alloy_primitives::Address;
use async_trait::async_trait;
use base_proof_contracts::{AggregateVerifierContractClient, DisputeGameFactoryClient};
use base_proof_rpc::RollupProvider;
use base_prover_service_client::{ProofRequesterProvider, ProverServiceClientError};
use base_prover_service_protocol::{DeleteProofsByTeeSignerRequest, GetProofRequest, ProofStatus};
use jsonrpsee::{core::client::Error as JsonRpcClientError, types::ErrorCode};
use tracing::{info, warn};

use crate::{IntervalResolver, Metrics, ProofTarget, ProposerError, ProposerProofAdapter};

/// Reads the currently registered signers from the registry used for new games.
///
/// Registration, rather than the registry's current image hash, determines whether
/// `TEEVerifier` will reject a proof with `InvalidSigner`. Fork-specific image-hash
/// validation remains with the verifier during submission.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait TeeSignerRegistry: Send + Sync {
    /// Returns a fresh registration snapshot from the currently active registry.
    ///
    /// Cached membership may only preserve proofs; destructive callers must refresh
    /// an absent signer's registration before deleting its proofs.
    async fn registered_signers(&self) -> Result<Vec<Address>, ProposerError>;
}

/// Discovers the TEE registry through the factory's current game implementation.
pub struct OnchainTeeSignerRegistry {
    factory: Arc<dyn DisputeGameFactoryClient>,
    verifier: Arc<AggregateVerifierContractClient>,
    game_type: u32,
}

impl std::fmt::Debug for OnchainTeeSignerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnchainTeeSignerRegistry")
            .field("game_type", &self.game_type)
            .finish_non_exhaustive()
    }
}

impl OnchainTeeSignerRegistry {
    /// Creates a registry reader that observes implementation and registry upgrades.
    pub const fn new(
        factory: Arc<dyn DisputeGameFactoryClient>,
        verifier: Arc<AggregateVerifierContractClient>,
        game_type: u32,
    ) -> Self {
        Self { factory, verifier, game_type }
    }
}

#[async_trait]
impl TeeSignerRegistry for OnchainTeeSignerRegistry {
    async fn registered_signers(&self) -> Result<Vec<Address>, ProposerError> {
        let implementation = self
            .factory
            .game_impls(self.game_type)
            .await
            .map_err(|error| ProposerError::Contract(error.to_string()))?;
        if implementation.is_zero() {
            return Err(ProposerError::Contract("game implementation address is zero".into()));
        }
        self.verifier
            .registered_tee_signers(implementation)
            .await
            .map_err(|error| ProposerError::Contract(error.to_string()))
    }
}

/// Scans dispatched, unproposed ranges independently of ordered proof submission.
///
/// Only completed TEE proofs with an unregistered, nonzero signer are batch-deleted.
/// The prover service's signer deletion API preserves queued/running jobs. Registered
/// membership may be cached within a tick, but absence is confirmed with a fresh
/// snapshot immediately before deletion to observe registrations and registry upgrades
/// during the sweep. The L1 read and deletion RPC are not atomic.
pub struct TeeProofRecovery {
    requester: Arc<dyn ProofRequesterProvider>,
    rollup: Arc<dyn RollupProvider>,
    intervals: Arc<IntervalResolver>,
    registry: Arc<dyn TeeSignerRegistry>,
    scan_timeout: Duration,
}

impl std::fmt::Debug for TeeProofRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeeProofRecovery")
            .field("scan_timeout", &self.scan_timeout)
            .finish_non_exhaustive()
    }
}

impl TeeProofRecovery {
    /// Maximum number of proposal ranges inspected per tick, including unfinished jobs.
    pub const MAX_TARGETS_PER_TICK: usize = 64;

    /// Creates a sweeper with a wall-clock budget for the entire tick, including RPC retries.
    pub fn new(
        requester: Arc<dyn ProofRequesterProvider>,
        rollup: Arc<dyn RollupProvider>,
        intervals: Arc<IntervalResolver>,
        registry: Arc<dyn TeeSignerRegistry>,
        scan_timeout: Duration,
    ) -> Self {
        Self { requester, rollup, intervals, registry, scan_timeout }
    }

    /// Inspects a bounded portion of the dispatched backlog without waiting for the collector.
    ///
    /// `scan_cursor` carries pagination across ticks and must be reset when recovered onchain
    /// state changes. `dispatched_through` may include ranges accepted by a previous session with
    /// the same recovered state; it must not shrink during redispatch. Returns `true` when both
    /// pipeline cursors must restart so deleted ranges
    /// behind the dispatcher are resubmitted. Interrupted or ambiguous failed deletes also
    /// require a restart; standard method-not-found and invalid-params rejections do not.
    pub async fn tick(
        &self,
        recovered_block: u64,
        dispatched_through: u64,
        scan_cursor: &mut Option<u64>,
    ) -> bool {
        let mut restart_required = false;
        match tokio::time::timeout(
            self.scan_timeout,
            self.scan(recovered_block, dispatched_through, scan_cursor, &mut restart_required),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                Metrics::errors_total(error.metric_label()).increment(1);
                warn!(error = %error, "Failed to scan TEE proof backlog");
            }
            Err(_) => {
                warn!(timeout = ?self.scan_timeout, "TEE proof backlog scan timed out");
            }
        }
        restart_required
    }

    /// Scans at most [`Self::MAX_TARGETS_PER_TICK`] ranges, preserving partial recovery progress.
    ///
    /// Prefer [`Self::tick`], which bounds all RPC waits. `restart_required` is set before issuing
    /// a destructive RPC and retained if that call fails or is dropped with an unknown outcome.
    pub async fn scan(
        &self,
        recovered_block: u64,
        dispatched_through: u64,
        scan_cursor: &mut Option<u64>,
        restart_required: &mut bool,
    ) -> Result<(), ProposerError> {
        if recovered_block >= dispatched_through {
            *scan_cursor = None;
            return Ok(());
        }
        let mut starting_block = scan_cursor
            .filter(|block| *block >= recovered_block && *block < dispatched_through)
            .unwrap_or(recovered_block);
        let mut registered_signers = HashSet::new();
        let mut checked_signers = HashSet::new();

        for _ in 0..Self::MAX_TARGETS_PER_TICK {
            let intervals = self.intervals.for_starting_block(starting_block).await?;
            let Some(target_block) =
                ProofTarget::next_block(starting_block, intervals.block_interval)
            else {
                *scan_cursor = None;
                break;
            };
            if target_block > dispatched_through {
                *scan_cursor = None;
                break;
            }
            // Advance before polling: a slow or unavailable early job must not starve later
            // signers. Unfinished and failed lookups are revisited when the cursor wraps.
            starting_block = target_block;
            *scan_cursor = Some(target_block);
            let Some(root) =
                ProofTarget::canonical_output_root(self.rollup.as_ref(), target_block).await
            else {
                continue;
            };
            let session_id = ProposerProofAdapter::tee_session_id_for_root(root);
            let response = match self.requester.get_proof(GetProofRequest { session_id }).await {
                Ok(response) => response,
                Err(error) if error.is_not_found() => continue,
                Err(error) => {
                    Metrics::errors_total("prover").increment(1);
                    warn!(target_block, error = %error, "Failed to inspect TEE proof during backlog scan");
                    continue;
                }
            };
            if response.status != ProofStatus::Succeeded {
                continue;
            }
            let Some(result) = response.result else { continue };
            let Ok(proof) = ProposerProofAdapter::tee_proof_result(result) else { continue };
            if proof.tee_signer.is_zero()
                || proof.aggregate_proposal.l2_block_number != target_block
                || proof.aggregate_proposal.output_root != root
                || !checked_signers.insert(proof.tee_signer)
            {
                continue;
            }
            if registered_signers.contains(&proof.tee_signer) {
                continue;
            }
            // Proof lookups yield while registration can change. Only reuse positive
            // membership; never authorize deletion from a cached absence.
            registered_signers = self.registry.registered_signers().await?.into_iter().collect();
            if registered_signers.contains(&proof.tee_signer) {
                continue;
            }

            let already_requires_restart = *restart_required;
            *restart_required = true;
            match self
                .requester
                .delete_proofs_by_tee_signer(DeleteProofsByTeeSignerRequest {
                    tee_signer: proof.tee_signer,
                })
                .await
            {
                Ok(deleted_count) => {
                    *restart_required = already_requires_restart || deleted_count > 0;
                    info!(
                        target_block,
                        tee_signer = %proof.tee_signer,
                        deleted_count,
                        "Deleted unregistered TEE signer proof requests during backlog scan"
                    );
                }
                Err(ProverServiceClientError::RpcTransport(JsonRpcClientError::Call(ref call)))
                    if call.code() == ErrorCode::MethodNotFound.code()
                        || call.code() == ErrorCode::InvalidParams.code() =>
                {
                    // These standard rejections occur before deletion. Other server errors
                    // may follow a partial delete, so retain their unknown-outcome safeguard.
                    *restart_required = already_requires_restart;
                    Metrics::errors_total("prover").increment(1);
                    warn!(
                        target_block,
                        tee_signer = %proof.tee_signer,
                        error = %call,
                        "TEE signer cleanup rejected without deletion"
                    );
                }
                Err(error) => {
                    Metrics::errors_total("prover").increment(1);
                    warn!(
                        target_block,
                        tee_signer = %proof.tee_signer,
                        error = %error,
                        "TEE signer cleanup outcome unknown, restarting for redispatch"
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        future::pending,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use alloy_consensus::{
        Eip658Value, Receipt, ReceiptEnvelope, ReceiptWithBloom, Transaction, TxEnvelope,
    };
    use alloy_eips::eip2718::Decodable2718;
    use alloy_primitives::{B256, Bloom, Bytes, U256, hex};
    use alloy_provider::RootProvider;
    use alloy_rpc_types_eth::{Block, Header, TransactionReceipt};
    use alloy_signer_local::PrivateKeySigner;
    use alloy_sol_types::SolValue;
    use base_common_genesis::RollupConfig;
    use base_optimism_rpc::{OutputAtBlock, SyncStatus};
    use base_proof_contracts::DisputeGameFactoryContractClient;
    use base_proof_rpc::RpcResult as RollupRpcResult;
    use base_prover_service_client::{ProofRequesterClient, ProverServiceClientConfig};
    use base_prover_service_protocol::{
        DeleteProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse,
        PROOF_REQUEST_NOT_FOUND_MESSAGE, ProofRequestKind, ProofResult, ProveBlockRangeRequest,
        ProveBlockRangeResponse, TeeKind, TeeProofResult, ZkProofResult, ZkVm,
    };
    use base_retry::RetryConfig;
    use base_tx_manager::{NoopTxMetrics, SimpleTxManager, TxManagerConfig};
    use jsonrpsee::{RpcModule, core::RpcResult, server::ServerBuilder, types::ErrorObjectOwned};
    use mockall::Sequence;
    use serde_json::Value;
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        DriverConfig, ProofCollector, ProofDispatcher, ProofRecovery, ProofRecoveryConfig,
        ProofSubmitter, ProposalSubmitter, ProvingPipeline,
        test_utils::{
            MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
            MockOutputProposer, MockRollupClient, test_anchor_root, test_fixed_interval_resolver,
            test_interval_resolver, test_l2_block_ref, test_proposal, test_sync_status,
        },
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
            async fn list_proofs(
                &self,
                request: ListProofsRequest,
            ) -> Result<ListProofsResponse, ProverServiceClientError>;
        }
    }

    mockall::mock! {
        Rollup {}

        #[async_trait]
        impl RollupProvider for Rollup {
            async fn rollup_config(&self) -> RollupRpcResult<RollupConfig>;
            async fn sync_status(&self) -> RollupRpcResult<SyncStatus>;
            async fn output_at_block(&self, block_number: u64) -> RollupRpcResult<OutputAtBlock>;
            async fn fresh_output_at_block(&self, block_number: u64) -> RollupRpcResult<OutputAtBlock>;
        }
    }

    fn root(block: u64) -> B256 {
        B256::from(U256::from(block))
    }

    fn session(block: u64) -> String {
        ProposerProofAdapter::tee_session_id_for_root(root(block))
    }

    fn rollup(head: u64) -> Arc<MockRollupClient> {
        Arc::new(MockRollupClient {
            sync_status: test_sync_status(head, B256::ZERO),
            output_roots: (0..=head).map(|block| (block, root(block))).collect(),
            max_safe_block: None,
        })
    }

    fn unfinished(status: ProofStatus) -> GetProofResponse {
        GetProofResponse { status, error_message: None, result: None }
    }

    fn succeeded(block: u64, signer: Address) -> GetProofResponse {
        GetProofResponse {
            status: ProofStatus::Succeeded,
            error_message: None,
            result: Some(ProofResult::Tee(TeeProofResult {
                aggregate_proposal: base_proof_primitives::Proposal {
                    output_root: root(block),
                    ..test_proposal(block)
                },
                proposals: vec![],
                tee_kind: TeeKind::AwsNitro,
                tee_signer: signer,
            })),
        }
    }

    fn not_found() -> ProverServiceClientError {
        JsonRpcClientError::Call(ErrorObjectOwned::owned(
            ProverServiceClientError::ERROR_NOT_FOUND,
            PROOF_REQUEST_NOT_FOUND_MESSAGE,
            None::<()>,
        ))
        .into()
    }

    fn requester(proofs: impl IntoIterator<Item = (u64, GetProofResponse)>) -> MockRequester {
        let proofs = proofs
            .into_iter()
            .map(|(block, proof)| (session(block), proof))
            .collect::<HashMap<_, _>>();
        let mut requester = MockRequester::new();
        requester.expect_get_proof().returning(move |request| {
            proofs.get(&request.session_id).cloned().ok_or_else(not_found)
        });
        requester.expect_delete_proof_request().never();
        requester.expect_list_proofs().never();
        requester
    }

    fn recovery(
        requester: MockRequester,
        registry: MockTeeSignerRegistry,
        head: u64,
    ) -> TeeProofRecovery {
        TeeProofRecovery::new(
            Arc::new(requester),
            rollup(head),
            test_fixed_interval_resolver(100, 100),
            Arc::new(registry),
            Duration::from_secs(1),
        )
    }

    fn test_pipeline<R: RollupProvider + 'static>(
        requester: Arc<dyn ProofRequesterProvider>,
        registry: MockTeeSignerRegistry,
        rollup: Arc<R>,
        scan_timeout: Duration,
        poll_interval: Duration,
        submitter: Option<ProofSubmitter>,
    ) -> ProvingPipeline<R> {
        let intervals = test_fixed_interval_resolver(100, 100);
        let factory = Arc::new(MockDisputeGameFactory::default());
        let config = DriverConfig { poll_interval, ..Default::default() };
        let dispatcher = ProofDispatcher::new(
            Arc::clone(&requester),
            Arc::new(MockL1::new(1000)),
            Arc::new(MockL2),
            Arc::<R>::clone(&rollup),
            Arc::clone(&intervals),
            Address::ZERO,
        );
        let chain_recovery = Arc::new(ProofRecovery::new(
            ProofRecoveryConfig {
                game_type: 0,
                anchor_state_registry_address: Address::ZERO,
                scan_concurrency: 1,
            },
            Arc::<R>::clone(&rollup),
            Arc::new(MockAnchorStateRegistry {
                anchor_root: test_anchor_root(0),
                anchor_game: Address::ZERO,
            }),
            Arc::<MockDisputeGameFactory>::clone(&factory),
            Arc::clone(&intervals),
        ));
        let submitter = submitter.unwrap_or_else(|| {
            ProofSubmitter::new(
                Arc::new(MockOutputProposer::default()),
                Arc::<R>::clone(&rollup),
                factory,
                Arc::new(MockAggregateVerifier::default()),
                Arc::clone(&intervals),
                &config,
            )
        });
        let collector = ProofCollector::new(
            Arc::clone(&requester),
            Arc::clone(&rollup),
            submitter,
            Arc::clone(&intervals),
            config.submit_timeout,
        );
        let recovery =
            TeeProofRecovery::new(requester, rollup, intervals, Arc::new(registry), scan_timeout);
        ProvingPipeline::new(config, dispatcher, chain_recovery, collector)
            .with_tee_proof_recovery(recovery)
    }

    #[tokio::test]
    async fn deletes_all_unregistered_signers_ahead_of_an_unfinished_head() {
        let first = Address::repeat_byte(0x11);
        let second = Address::repeat_byte(0x22);
        let healthy = Address::repeat_byte(0x33);
        let mut requester = requester([
            (100, unfinished(ProofStatus::Queued)),
            (200, succeeded(200, first)),
            (300, succeeded(300, second)),
            (400, succeeded(400, first)),
            (500, succeeded(500, healthy)),
            (600, unfinished(ProofStatus::Running)),
            (700, unfinished(ProofStatus::Failed)),
            (800, succeeded(800, Address::ZERO)),
            (900, succeeded(901, Address::repeat_byte(0x44))),
            (
                1000,
                GetProofResponse {
                    status: ProofStatus::Succeeded,
                    error_message: None,
                    result: Some(ProofResult::Compressed(ZkProofResult {
                        zk_vm: ZkVm::Sp1,
                        proof: Default::default(),
                        execution_stats: None,
                    })),
                },
            ),
        ]);
        for signer in [first, second] {
            requester
                .expect_delete_proofs_by_tee_signer()
                .withf(move |request| request.tee_signer == signer)
                .times(1)
                .returning(|_| Ok(7));
        }
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().times(2).returning(move || Ok(vec![healthy]));

        assert!(recovery(requester, registry, 1000).tick(0, 1000, &mut None).await);
    }

    #[tokio::test]
    async fn no_registry_lookup_or_deletion_for_unfinished_or_missing_proofs() {
        let mut requester = requester([
            (100, unfinished(ProofStatus::Queued)),
            (200, unfinished(ProofStatus::Running)),
            (300, unfinished(ProofStatus::Failed)),
        ]);
        requester.expect_delete_proofs_by_tee_signer().never();
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().never();
        let recovery = recovery(requester, registry, 400);

        assert!(!recovery.tick(0, 0, &mut None).await);
        assert!(!recovery.tick(0, 400, &mut None).await);
    }

    #[tokio::test]
    async fn registration_failure_preserves_completed_proofs() {
        let mut requester = requester([(100, succeeded(100, Address::repeat_byte(0x11)))]);
        requester.expect_delete_proofs_by_tee_signer().never();
        let mut registry = MockTeeSignerRegistry::new();
        registry
            .expect_registered_signers()
            .times(1)
            .returning(|| Err(ProposerError::Contract("L1 unavailable".into())));

        assert!(!recovery(requester, registry, 100).tick(0, 100, &mut None).await);
    }

    #[tokio::test]
    async fn registration_during_sweep_preserves_new_signer_proofs() {
        let healthy = Address::repeat_byte(0x11);
        let replacement = Address::repeat_byte(0x22);
        let factory = Address::repeat_byte(0xf0);
        let implementation = Address::repeat_byte(0xa1);
        let verifier = Address::repeat_byte(0xb1);
        let registry_address = Address::repeat_byte(0xc1);
        let replacement_registered = Arc::new(AtomicBool::new(false));
        let deletion_attempts = Arc::new(Mutex::new(Vec::new()));
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", server.local_addr().unwrap());
        let provider = RootProvider::new_http(endpoint.parse().unwrap());
        let mut module = RpcModule::new(());
        let registration = Arc::clone(&replacement_registered);
        module
            .register_method("eth_call", move |params, _, _| {
                let params = params.parse::<Vec<Value>>()?;
                let to = params[0]["to"].as_str().unwrap().parse::<Address>().unwrap();
                let input =
                    params[0]["input"].as_str().or_else(|| params[0]["data"].as_str()).unwrap();
                let output = if to == factory {
                    assert!(input.starts_with("0x1b685b9e"), "gameImpls(uint32) ABI");
                    implementation.abi_encode()
                } else if to == implementation {
                    assert_eq!(input, "0x1f837574", "TEE_VERIFIER() ABI");
                    verifier.abi_encode()
                } else if to == verifier {
                    assert_eq!(input, "0x18efb9c2", "TEE_PROVER_REGISTRY() ABI");
                    registry_address.abi_encode()
                } else if to == registry_address {
                    assert_eq!(input, "0x94b2822f", "getRegisteredSigners() ABI");
                    if registration.load(Ordering::SeqCst) {
                        vec![healthy, replacement].abi_encode()
                    } else {
                        vec![healthy].abi_encode()
                    }
                } else {
                    panic!("unexpected contract read: {to}");
                };
                Ok::<_, ErrorObjectOwned>(format!("0x{}", hex::encode(output)))
            })
            .unwrap();
        let registration = Arc::clone(&replacement_registered);
        module
            .register_method("prover_getProof", move |params, _, _| {
                let request: GetProofRequest = params.one()?;
                let proof = if request.session_id == session(100) {
                    succeeded(100, healthy)
                } else {
                    // Registration completes after the first snapshot, before a later
                    // proof response arrives. Repeated proofs share the new signer.
                    registration.store(true, Ordering::SeqCst);
                    let block = if request.session_id == session(200) { 200 } else { 300 };
                    succeeded(block, replacement)
                };
                Ok::<_, ErrorObjectOwned>(proof)
            })
            .unwrap();
        let attempted = Arc::clone(&deletion_attempts);
        module
            .register_method("prover_deleteProofsByTeeSigner", move |params, _, _| {
                let request: DeleteProofsByTeeSignerRequest = params.one()?;
                attempted.lock().unwrap().push(request.tee_signer);
                Ok::<_, ErrorObjectOwned>(1_u64)
            })
            .unwrap();
        let handle = server.start(module);
        let requester =
            ProofRequesterClient::connect(&ProverServiceClientConfig::new(endpoint)).unwrap();
        let registry = OnchainTeeSignerRegistry::new(
            Arc::new(DisputeGameFactoryContractClient::new(factory, provider.clone())),
            Arc::new(AggregateVerifierContractClient::new(provider)),
            0,
        );
        let recovery = TeeProofRecovery::new(
            Arc::new(requester),
            rollup(300),
            test_fixed_interval_resolver(100, 100),
            Arc::new(registry),
            Duration::from_secs(1),
        );
        let restart = recovery.tick(0, 300, &mut None).await;
        handle.stop().unwrap();
        tokio::time::timeout(Duration::from_secs(1), handle.stopped()).await.unwrap();

        assert!(replacement_registered.load(Ordering::SeqCst));
        assert!(
            deletion_attempts.lock().unwrap().is_empty(),
            "freshly registered signer proofs must not be deleted using an earlier snapshot"
        );
        assert!(!restart, "new registration must not cause an unnecessary cleanup restart");
    }

    #[tokio::test]
    async fn registration_refresh_failure_preserves_proofs_and_prior_cleanup() {
        for (deleted_count, ambiguous, restart_expected) in
            [(0, false, false), (1, false, true), (0, true, true)]
        {
            let first = Address::repeat_byte(0x11);
            let second = Address::repeat_byte(0x22);
            let mut requester =
                requester([(100, succeeded(100, first)), (200, succeeded(200, second))]);
            requester
                .expect_delete_proofs_by_tee_signer()
                .withf(move |request| request.tee_signer == first)
                .times(1)
                .returning(move |_| {
                    if ambiguous {
                        Err(JsonRpcClientError::RequestTimeout.into())
                    } else {
                        Ok(deleted_count)
                    }
                });
            requester
                .expect_delete_proofs_by_tee_signer()
                .withf(move |request| request.tee_signer == second)
                .never();
            let mut registry = MockTeeSignerRegistry::new();
            let mut sequence = Sequence::new();
            registry
                .expect_registered_signers()
                .times(1)
                .in_sequence(&mut sequence)
                .returning(|| Ok(vec![]));
            registry
                .expect_registered_signers()
                .times(1)
                .in_sequence(&mut sequence)
                .returning(|| Err(ProposerError::Contract("L1 unavailable during refresh".into())));

            assert_eq!(
                recovery(requester, registry, 200).tick(0, 200, &mut None).await,
                restart_expected
            );
        }
    }

    #[tokio::test]
    async fn zero_row_delete_backs_off_and_reregistration_is_observed_on_next_tick() {
        let signer = Address::repeat_byte(0x11);
        let mut requester = requester([(100, succeeded(100, signer))]);
        requester.expect_delete_proofs_by_tee_signer().times(1).returning(|_| Ok(0));
        let mut registry = MockTeeSignerRegistry::new();
        let mut sequence = Sequence::new();
        registry
            .expect_registered_signers()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(|| Ok(vec![]));
        registry
            .expect_registered_signers()
            .times(1)
            .in_sequence(&mut sequence)
            .returning(move || Ok(vec![signer]));
        let recovery = recovery(requester, registry, 100);
        let mut cursor = None;

        assert!(!recovery.tick(0, 100, &mut cursor).await);
        assert!(!recovery.tick(0, 100, &mut cursor).await);
    }

    #[tokio::test]
    async fn partial_cleanup_and_uncertain_deletion_still_require_redispatch() {
        let first = Address::repeat_byte(0x11);
        let second = Address::repeat_byte(0x22);
        let mut requester =
            requester([(100, succeeded(100, first)), (200, succeeded(200, second))]);
        requester
            .expect_delete_proofs_by_tee_signer()
            .withf(move |request| request.tee_signer == first)
            .times(1)
            .returning(|_| Err(JsonRpcClientError::RequestTimeout.into()));
        requester
            .expect_delete_proofs_by_tee_signer()
            .withf(move |request| request.tee_signer == second)
            .times(1)
            .returning(|_| Ok(7));
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().times(2).returning(|| Ok(vec![]));

        assert!(recovery(requester, registry, 200).tick(0, 200, &mut None).await);
    }

    #[tokio::test]
    async fn standard_delete_rejections_do_not_restart_but_preserve_prior_cleanup() {
        for code in [ErrorCode::MethodNotFound.code(), ErrorCode::InvalidParams.code()] {
            for (deleted_count, ambiguous, restart_expected) in
                [(0, false, false), (1, false, true), (0, true, true)]
            {
                let first = Address::repeat_byte(0x11);
                let second = Address::repeat_byte(0x22);
                let mut requester =
                    requester([(100, succeeded(100, first)), (200, succeeded(200, second))]);
                requester
                    .expect_delete_proofs_by_tee_signer()
                    .withf(move |request| request.tee_signer == first)
                    .times(1)
                    .returning(move |_| {
                        if ambiguous {
                            Err(JsonRpcClientError::RequestTimeout.into())
                        } else {
                            Ok(deleted_count)
                        }
                    });
                requester
                    .expect_delete_proofs_by_tee_signer()
                    .withf(move |request| request.tee_signer == second)
                    .times(1)
                    .returning(move |_| {
                        Err(JsonRpcClientError::Call(ErrorObjectOwned::owned(
                            code,
                            "request rejected",
                            None::<()>,
                        ))
                        .into())
                    });
                let mut registry = MockTeeSignerRegistry::new();
                registry.expect_registered_signers().returning(|| Ok(vec![]));
                let recovery = recovery(requester, registry, 200);
                assert_eq!(recovery.tick(0, 200, &mut None).await, restart_expected);
            }
        }
    }

    #[tokio::test]
    async fn pagination_reaches_signers_beyond_first_bounded_page() {
        let last = (TeeProofRecovery::MAX_TARGETS_PER_TICK as u64 + 1) * 100;
        let proofs = (100..last)
            .step_by(100)
            .map(|block| (block, unfinished(ProofStatus::Running)))
            .chain([(last, succeeded(last, Address::repeat_byte(0x11)))]);
        let mut requester = requester(proofs);
        requester.expect_delete_proofs_by_tee_signer().times(1).returning(|_| Ok(1));
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().times(1).returning(|| Ok(vec![]));
        let recovery = recovery(requester, registry, last);
        let mut cursor = None;

        assert!(!recovery.tick(0, last, &mut cursor).await);
        assert_eq!(cursor, Some(last - 100));
        assert!(recovery.tick(0, last, &mut cursor).await);
    }

    #[tokio::test]
    async fn scan_uses_fork_aware_target_intervals() {
        let mut requester = MockRequester::new();
        let mut sequence = Sequence::new();
        for block in [100, 200, 250, 300] {
            requester
                .expect_get_proof()
                .withf(move |request| request.session_id == session(block))
                .times(1)
                .in_sequence(&mut sequence)
                .returning(|_| Ok(unfinished(ProofStatus::Running)));
        }
        requester.expect_delete_proofs_by_tee_signer().never();
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().never();
        let recovery = TeeProofRecovery::new(
            Arc::new(requester),
            rollup(300),
            test_interval_resolver(MockAggregateVerifier {
                fast_activation_block: 200,
                fast_block_interval: 50,
                fast_intermediate_block_interval: 50,
                ..Default::default()
            }),
            Arc::new(registry),
            Duration::from_secs(1),
        );

        assert!(!recovery.tick(0, 300, &mut None).await);
    }

    #[tokio::test]
    async fn scan_deadline_bounds_slow_reads_and_preserves_unknown_delete_outcomes() {
        for stall_delete in [false, true] {
            let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", server.local_addr().unwrap());
            let delete_started = Arc::new(AtomicBool::new(false));
            let mut module = RpcModule::new(Arc::clone(&delete_started));
            module
                .register_async_method("prover_getProof", move |_, _, _| async move {
                    if stall_delete {
                        Ok(succeeded(100, Address::repeat_byte(0x11)))
                    } else {
                        pending::<RpcResult<GetProofResponse>>().await
                    }
                })
                .unwrap();
            module
                .register_async_method(
                    "prover_deleteProofsByTeeSigner",
                    |_, started, _| async move {
                        started.store(true, Ordering::SeqCst);
                        pending::<RpcResult<u64>>().await
                    },
                )
                .unwrap();
            let handle = server.start(module);
            let requester =
                ProofRequesterClient::connect(&ProverServiceClientConfig::new(endpoint)).unwrap();
            let mut registry = MockTeeSignerRegistry::new();
            if stall_delete {
                registry.expect_registered_signers().times(1).returning(|| Ok(vec![]));
            } else {
                registry.expect_registered_signers().never();
            }
            let recovery = TeeProofRecovery::new(
                Arc::new(requester),
                rollup(100),
                test_fixed_interval_resolver(100, 100),
                Arc::new(registry),
                Duration::from_millis(100),
            );
            let mut cursor = None;
            let restart =
                tokio::time::timeout(Duration::from_secs(1), recovery.tick(0, 100, &mut cursor))
                    .await
                    .expect("scan must stop waiting within its budget");

            assert_eq!(restart, stall_delete);
            assert_eq!(delete_started.load(Ordering::SeqCst), stall_delete);
            assert_eq!(cursor, Some(100));
            handle.stop().unwrap();
            tokio::time::timeout(Duration::from_secs(1), handle.stopped()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn registry_reader_follows_factory_and_registry_upgrades() {
        let factory = Address::repeat_byte(0xf0);
        let implementation = [Address::repeat_byte(0xa1), Address::repeat_byte(0xa2)];
        let verifier = [Address::repeat_byte(0xb1), Address::repeat_byte(0xb2)];
        let registry = [Address::repeat_byte(0xc1), Address::repeat_byte(0xc2)];
        let signer = [Address::repeat_byte(0xd1), Address::repeat_byte(0xd2)];
        let generation = Arc::new(AtomicUsize::new(0));
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let provider = RootProvider::new_http(
            format!("http://{}", server.local_addr().unwrap()).parse().unwrap(),
        );
        let mut module = RpcModule::new(Arc::clone(&generation));
        module
            .register_method("eth_call", move |params, generation, _| {
                let params = params.parse::<Vec<Value>>()?;
                let to = params[0]["to"].as_str().unwrap().parse::<Address>().unwrap();
                let generation = generation.load(Ordering::SeqCst);
                let input =
                    params[0]["input"].as_str().or_else(|| params[0]["data"].as_str()).unwrap();
                let output = if to == factory {
                    assert!(input.starts_with("0x1b685b9e"), "gameImpls(uint32) ABI");
                    implementation[generation].abi_encode()
                } else if to == implementation[generation] {
                    assert_eq!(input, "0x1f837574", "TEE_VERIFIER() ABI");
                    verifier[generation].abi_encode()
                } else if to == verifier[generation] {
                    assert_eq!(input, "0x18efb9c2", "TEE_PROVER_REGISTRY() ABI");
                    registry[generation].abi_encode()
                } else if to == registry[generation] {
                    assert_eq!(input, "0x94b2822f", "getRegisteredSigners() ABI");
                    vec![signer[generation]].abi_encode()
                } else {
                    panic!("unexpected contract read: {to}");
                };
                Ok::<_, ErrorObjectOwned>(format!("0x{}", hex::encode(output)))
            })
            .unwrap();
        let handle = server.start(module);
        let registry_reader = OnchainTeeSignerRegistry::new(
            Arc::new(DisputeGameFactoryContractClient::new(factory, provider.clone())),
            Arc::new(AggregateVerifierContractClient::new(provider)),
            0,
        );

        assert_eq!(registry_reader.registered_signers().await.unwrap(), vec![signer[0]]);
        generation.store(1, Ordering::SeqCst);
        assert_eq!(registry_reader.registered_signers().await.unwrap(), vec![signer[1]]);
        handle.stop().unwrap();
        handle.stopped().await;
    }

    #[tokio::test]
    async fn ambiguous_batch_delete_is_not_hidden_by_zero_rows_or_rejection_on_retry() {
        for final_code in
            [None, Some(ErrorCode::MethodNotFound.code()), Some(ErrorCode::InvalidParams.code())]
        {
            let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", server.local_addr().unwrap());
            let deleted = Arc::new(AtomicBool::new(false));
            let attempts = Arc::new(AtomicUsize::new(0));
            let mut module = RpcModule::new(());
            let proof_deleted = Arc::clone(&deleted);
            module
                .register_method("prover_getProof", move |_, _, _| {
                    if proof_deleted.load(Ordering::SeqCst) {
                        Err(ErrorObjectOwned::owned(
                            ProverServiceClientError::ERROR_NOT_FOUND,
                            PROOF_REQUEST_NOT_FOUND_MESSAGE,
                            None::<()>,
                        ))
                    } else {
                        Ok(succeeded(100, Address::repeat_byte(0x11)))
                    }
                })
                .unwrap();
            let committed = Arc::clone(&deleted);
            let calls = Arc::clone(&attempts);
            module
                .register_method("prover_deleteProofsByTeeSigner", move |_, _, _| {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        // The destructive effect commits, but its response is ambiguous.
                        committed.store(true, Ordering::SeqCst);
                        Err(ErrorObjectOwned::owned(
                            ErrorCode::InternalError.code(),
                            "response lost after commit",
                            None::<()>,
                        ))
                    } else if let Some(code) = final_code {
                        Err(ErrorObjectOwned::owned(code, "later request rejected", None::<()>))
                    } else {
                        Ok(0_u64)
                    }
                })
                .unwrap();
            let handle = server.start(module);
            let requester =
                ProofRequesterClient::connect(&ProverServiceClientConfig::new(endpoint))
                    .unwrap()
                    .with_retry_config(RetryConfig::new(
                        3,
                        Duration::from_millis(1),
                        Duration::from_millis(1),
                    ));
            let mut registry = MockTeeSignerRegistry::new();
            registry.expect_registered_signers().times(1).returning(|| Ok(vec![]));
            let recovery = TeeProofRecovery::new(
                Arc::new(requester),
                rollup(100),
                test_fixed_interval_resolver(100, 100),
                Arc::new(registry),
                Duration::from_secs(1),
            );
            assert!(
                recovery.tick(0, 100, &mut None).await,
                "a committed deletion must require redispatch even if a retry would report {final_code:?}"
            );
            assert!(deleted.load(Ordering::SeqCst));
            assert_eq!(
                attempts.load(Ordering::SeqCst),
                1,
                "the client must expose the first ambiguous outcome instead of retrying destructively"
            );
            assert!(!recovery.tick(0, 100, &mut None).await);
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
            handle.stop().unwrap();
            tokio::time::timeout(Duration::from_secs(1), handle.stopped()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn sweep_discovers_accepted_prefix_while_later_dispatch_is_blocked() {
        let blocked = CancellationToken::new();
        let cleaned = CancellationToken::new();
        let accepted = Arc::new(AtomicUsize::new(0));
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", server.local_addr().unwrap());
        let mut module = RpcModule::new(());
        let dispatch_blocked = blocked.clone();
        let progress = Arc::clone(&accepted);
        module
            .register_async_method("prover_proveBlockRange", move |params, _, _| {
                let blocked = dispatch_blocked.clone();
                let progress = Arc::clone(&progress);
                async move {
                    let request: ProveBlockRangeRequest = params.one()?;
                    let ProofRequestKind::Tee(tee) = &request.proof.request else {
                        panic!("expected TEE")
                    };
                    if tee.proof.claimed_l2_block_number == 300 {
                        blocked.cancel();
                        pending::<RpcResult<ProveBlockRangeResponse>>().await
                    } else {
                        progress.fetch_max(
                            tee.proof.claimed_l2_block_number as usize,
                            Ordering::SeqCst,
                        );
                        Ok(ProveBlockRangeResponse { session_id: request.proof.session_id })
                    }
                }
            })
            .unwrap();
        let dispatched = Arc::clone(&accepted);
        module
            .register_method("prover_getProof", move |params, _, _| {
                let request: GetProofRequest = params.one()?;
                Ok::<_, ErrorObjectOwned>(
                    if request.session_id == session(200)
                        && dispatched.load(Ordering::SeqCst) >= 200
                    {
                        succeeded(200, Address::repeat_byte(0x11))
                    } else {
                        unfinished(ProofStatus::Queued)
                    },
                )
            })
            .unwrap();
        let cleanup_observed = cleaned.clone();
        module
            .register_method("prover_deleteProofsByTeeSigner", move |params, _, _| {
                let request: DeleteProofsByTeeSignerRequest = params.one()?;
                assert_eq!(request.tee_signer, Address::repeat_byte(0x11));
                cleanup_observed.cancel();
                Ok::<_, ErrorObjectOwned>(1_u64)
            })
            .unwrap();
        let handle = server.start(module);
        let requester =
            ProofRequesterClient::connect(&ProverServiceClientConfig::new(endpoint)).unwrap();
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().returning(|| Ok(vec![]));
        let pipeline = test_pipeline(
            Arc::new(requester),
            registry,
            rollup(300),
            Duration::from_secs(5),
            Duration::from_millis(10),
            None,
        );
        let cancel = CancellationToken::new();
        let run = pipeline.run(cancel.clone());
        tokio::pin!(run);
        tokio::select! {
            () = &mut run => panic!("pipeline must run until cancelled"),
            result = tokio::time::timeout(Duration::from_secs(2), async {
                blocked.cancelled().await;
                cleaned.cancelled().await;
            }) => result.expect("block 200 must be swept before dispatch of block 300 finishes"),
        }
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            200,
            "unaccepted targets must not be published"
        );
        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(100), &mut run).await.unwrap();
        handle.stop().unwrap();
        tokio::time::timeout(Duration::from_secs(1), handle.stopped()).await.unwrap();
    }

    // This server drives the real transaction manager across publication and confirmation;
    // its responses change while the submission is in flight, rather than mocking send().
    async fn assert_cleanup_preserves_transaction_lifecycle(stall_before_publish: bool) {
        let healthy = Address::repeat_byte(0x33);
        let bad_signer = Address::repeat_byte(0x11);
        let block_hash = B256::repeat_byte(0x55);
        let publish_started = CancellationToken::new();
        let receipt_waiting = CancellationToken::new();
        let fee_bumped = CancellationToken::new();
        let release = CancellationToken::new();
        let cleaned = CancellationToken::new();
        let redispatched = CancellationToken::new();
        let receipt_delivered = Arc::new(AtomicBool::new(false));
        let premature_redispatch = Arc::new(AtomicBool::new(false));
        let nonces = Arc::new(Mutex::new(Vec::new()));
        let cleanup_stage =
            if stall_before_publish { publish_started.clone() } else { receipt_waiting.clone() };
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", server.local_addr().unwrap());
        let mut module = RpcModule::new(());
        for (method, value) in [
            ("eth_chainId", "0x1"),
            ("eth_blockNumber", "0x1"),
            ("eth_maxPriorityFeePerGas", "0x3b9aca00"),
            ("eth_estimateGas", "0x5208"),
            ("eth_getTransactionCount", "0x0"),
        ] {
            module
                .register_method(method, move |_, _, _| Ok::<_, ErrorObjectOwned>(value))
                .unwrap();
        }
        let block = Block::<B256> {
            header: Header {
                hash: block_hash,
                inner: alloy_consensus::Header {
                    number: 1,
                    base_fee_per_gas: Some(1_000_000_000),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        module
            .register_method("eth_getBlockByNumber", move |_, _, _| {
                Ok::<_, ErrorObjectOwned>(block.clone())
            })
            .unwrap();
        let publication = publish_started.clone();
        let unlock = release.clone();
        let bumped = fee_bumped.clone();
        let sent_nonces = Arc::clone(&nonces);
        let cleanup = cleaned.clone();
        module
            .register_async_method("eth_sendRawTransaction", move |params, _, _| {
                let publication = publication.clone();
                let unlock = unlock.clone();
                let bumped = bumped.clone();
                let sent_nonces = Arc::clone(&sent_nonces);
                let cleanup = cleanup.clone();
                async move {
                    let raw: Bytes = params.one()?;
                    let envelope = TxEnvelope::decode_2718(&mut raw.as_ref()).unwrap();
                    let first = {
                        let mut nonces = sent_nonces.lock().unwrap();
                        let first = nonces.is_empty();
                        nonces.push(envelope.nonce());
                        first
                    };
                    publication.cancel();
                    if stall_before_publish && first {
                        unlock.cancelled().await;
                    }
                    if !first && envelope.nonce() == 0 && cleanup.is_cancelled() {
                        bumped.cancel();
                    }
                    Ok::<_, ErrorObjectOwned>(*envelope.tx_hash())
                }
            })
            .unwrap();
        let waiting = receipt_waiting.clone();
        let unlock = release.clone();
        let delivered = Arc::clone(&receipt_delivered);
        module
            .register_method(
                "eth_getTransactionReceipt",
                move |params, _, _| -> RpcResult<Option<TransactionReceipt>> {
                    waiting.cancel();
                    if !unlock.is_cancelled() {
                        return Ok::<_, ErrorObjectOwned>(None);
                    }
                    delivered.store(true, Ordering::SeqCst);
                    Ok(Some(TransactionReceipt {
                        inner: ReceiptEnvelope::Legacy(ReceiptWithBloom {
                            receipt: Receipt {
                                status: Eip658Value::success(),
                                cumulative_gas_used: 21_000,
                                logs: vec![],
                            },
                            logs_bloom: Bloom::ZERO,
                        }),
                        transaction_hash: params.one()?,
                        transaction_index: Some(0),
                        block_hash: Some(block_hash),
                        block_number: Some(1),
                        gas_used: 21_000,
                        effective_gas_price: 1_000_000_000,
                        blob_gas_used: None,
                        blob_gas_price: None,
                        from: Address::ZERO,
                        to: Some(Address::repeat_byte(0xf0)),
                        contract_address: None,
                    }))
                },
            )
            .unwrap();
        let dispatch_count = Arc::new(AtomicUsize::new(0));
        let dispatches = Arc::clone(&dispatch_count);
        let cleanup = cleaned.clone();
        let observed = redispatched.clone();
        let delivered = Arc::clone(&receipt_delivered);
        let premature = Arc::clone(&premature_redispatch);
        module
            .register_method("prover_proveBlockRange", move |params, _, _| {
                let request: ProveBlockRangeRequest = params.one()?;
                let ProofRequestKind::Tee(tee) = &request.proof.request else {
                    panic!("expected TEE")
                };
                if tee.proof.claimed_l2_block_number == 300
                    && dispatches.fetch_add(1, Ordering::SeqCst) > 0
                    && cleanup.is_cancelled()
                {
                    premature.store(!delivered.load(Ordering::SeqCst), Ordering::SeqCst);
                    observed.cancel();
                }
                Ok::<_, ErrorObjectOwned>(ProveBlockRangeResponse {
                    session_id: request.proof.session_id,
                })
            })
            .unwrap();
        let cleanup = cleaned.clone();
        let delivered = Arc::clone(&receipt_delivered);
        module
            .register_method("prover_getProof", move |params, _, _| {
                let request: GetProofRequest = params.one()?;
                let response =
                    // Keep a second healthy proof ready: draining must stop after the
                    // current transaction, not start another submission in the same tick.
                    if (request.session_id == session(100) && !delivered.load(Ordering::SeqCst))
                        || request.session_id == session(200)
                    {
                        let block = if request.session_id == session(100) { 100 } else { 200 };
                        let mut response = succeeded(block, healthy);
                        let Some(ProofResult::Tee(proof)) = response.result.as_mut() else {
                            unreachable!()
                        };
                        proof.proposals = (block - 100 + 1..=block)
                            .map(|block| base_proof_primitives::Proposal {
                                output_root: root(block),
                                ..test_proposal(block)
                            })
                            .collect();
                        response
                    } else if request.session_id == session(300)
                        && cleanup_stage.is_cancelled()
                        && !cleanup.is_cancelled()
                    {
                        succeeded(300, bad_signer)
                    } else {
                        unfinished(ProofStatus::Queued)
                    };
                Ok::<_, ErrorObjectOwned>(response)
            })
            .unwrap();
        let cleanup = cleaned.clone();
        module
            .register_method("prover_deleteProofsByTeeSigner", move |params, _, _| {
                let request: DeleteProofsByTeeSignerRequest = params.one()?;
                assert_eq!(request.tee_signer, bad_signer);
                cleanup.cancel();
                Ok::<_, ErrorObjectOwned>(1_u64)
            })
            .unwrap();
        let handle = server.start(module);
        let tx_manager = SimpleTxManager::from_wallet(
            RootProvider::new_http(endpoint.parse().unwrap()),
            PrivateKeySigner::random().into(),
            TxManagerConfig {
                num_confirmations: 1,
                network_timeout: Duration::from_secs(3),
                resubmission_timeout: Duration::from_millis(50),
                receipt_query_interval: Duration::from_millis(10),
                tx_send_timeout: Duration::from_secs(5),
                ..Default::default()
            },
            1,
            Arc::new(NoopTxMetrics),
        )
        .await
        .unwrap();
        let rollup = rollup(300);
        let submitter = ProofSubmitter::new(
            Arc::new(ProposalSubmitter::new(
                tx_manager.clone(),
                Address::repeat_byte(0xf0),
                0,
                U256::ZERO,
            )),
            Arc::<MockRollupClient>::clone(&rollup),
            Arc::new(MockDisputeGameFactory::with_uuid_game_responses([
                Address::ZERO,
                Address::repeat_byte(0x66),
            ])),
            Arc::new(MockAggregateVerifier::default()),
            test_fixed_interval_resolver(100, 100),
            &DriverConfig::default(),
        );
        let requester =
            ProofRequesterClient::connect(&ProverServiceClientConfig::new(endpoint)).unwrap();
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().returning(move || Ok(vec![healthy]));
        let pipeline = test_pipeline(
            Arc::new(requester),
            registry,
            rollup,
            Duration::from_secs(1),
            Duration::from_millis(10),
            Some(submitter),
        );
        let cancel = CancellationToken::new();
        let run = pipeline.run(cancel.clone());
        tokio::pin!(run);
        tokio::select! {
            () = &mut run => panic!("pipeline must run until cancelled"),
            result = tokio::time::timeout(Duration::from_secs(2), cleaned.cancelled()) => result.expect("cleanup must occur during the L1 submission"),
        }
        tokio::select! {
            () = &mut run => panic!("cleanup must drain rather than terminate the pipeline"),
            () = tokio::time::sleep(Duration::from_millis(100)) => {},
        }
        assert!(
            !redispatched.is_cancelled(),
            "cleanup must not reset the pipeline before the in-flight send settles"
        );
        assert!(!premature_redispatch.load(Ordering::SeqCst));
        assert!(
            nonces.lock().unwrap().iter().all(|nonce| *nonce == 0),
            "cleanup must not abandon nonce zero and start nonce one"
        );
        if !stall_before_publish {
            tokio::select! {
                () = &mut run => panic!("pipeline must keep the published transaction alive"),
                result = tokio::time::timeout(Duration::from_secs(2), fee_bumped.cancelled()) => result.expect("fee bumping must continue after cleanup while the transaction is unmined"),
            }
        }
        release.cancel();
        tokio::select! {
            () = &mut run => panic!("pipeline must run until cancelled"),
            result = tokio::time::timeout(Duration::from_secs(2), redispatched.cancelled()) => result.expect("cleanup must redispatch after the current send confirms"),
        }
        assert!(receipt_delivered.load(Ordering::SeqCst));
        assert!(!premature_redispatch.load(Ordering::SeqCst));
        assert!(nonces.lock().unwrap().iter().all(|nonce| *nonce == 0));
        let nonce = tx_manager.nonce_manager().next_nonce().await.unwrap();
        assert_eq!(nonce.nonce(), 1, "exactly one nonce must be consumed; fee bumps reuse it");
        nonce.rollback();
        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(100), &mut run).await.unwrap();
        tx_manager.close();
        handle.stop().unwrap();
        tokio::time::timeout(Duration::from_secs(1), handle.stopped()).await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_preserves_consumed_nonce_before_publication() {
        assert_cleanup_preserves_transaction_lifecycle(true).await;
    }

    #[tokio::test]
    async fn cleanup_preserves_fee_bumping_after_publication() {
        assert_cleanup_preserves_transaction_lifecycle(false).await;
    }

    #[tokio::test]
    async fn pipeline_redispatches_deleted_range_while_earlier_proof_is_still_queued() {
        let bad_signer = Address::repeat_byte(0x11);
        let deleted = Arc::new(AtomicBool::new(false));
        let dispatches = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();
        let mut requester = MockRequester::new();
        let dispatch_counter = Arc::clone(&dispatches);
        let delete_observed = Arc::clone(&deleted);
        let stop = cancel.clone();
        requester.expect_prove_block_range().returning(move |request| {
            let ProofRequestKind::Tee(tee) = &request.proof.request else { panic!("expected TEE") };
            if tee.proof.claimed_l2_block_number == 200 {
                let previous = dispatch_counter.fetch_add(1, Ordering::SeqCst);
                if previous > 0 {
                    assert!(
                        delete_observed.load(Ordering::SeqCst),
                        "redispatch must follow signer deletion"
                    );
                    stop.cancel();
                }
            }
            Ok(ProveBlockRangeResponse { session_id: request.proof.session_id })
        });
        requester.expect_get_proof().returning(move |request| {
            if request.session_id == session(100) {
                Ok(unfinished(ProofStatus::Queued))
            } else if request.session_id == session(200) {
                Ok(succeeded(200, bad_signer))
            } else {
                Err(not_found())
            }
        });
        let delete_flag = Arc::clone(&deleted);
        requester
            .expect_delete_proofs_by_tee_signer()
            .withf(move |request| request.tee_signer == bad_signer)
            .times(1)
            .returning(move |_| {
                delete_flag.store(true, Ordering::SeqCst);
                Ok(1)
            });
        requester.expect_delete_proof_request().never();
        let requester: Arc<dyn ProofRequesterProvider> = Arc::new(requester);
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().times(1).returning(|| Ok(vec![]));
        let pipeline = test_pipeline(
            requester,
            registry,
            rollup(200),
            Duration::from_secs(1),
            Duration::from_millis(10),
            None,
        );

        tokio::time::timeout(Duration::from_secs(2), pipeline.run(cancel)).await.expect(
            "deleted ranges must be redispatched without waiting for the earlier queued proof",
        );
        assert!(deleted.load(Ordering::SeqCst));
        assert_eq!(dispatches.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn dispatch_advances_and_cancellation_is_prompt_while_sweep_is_blocked() {
        let head = Arc::new(AtomicUsize::new(200));
        let scan_started = CancellationToken::new();
        let new_range_dispatched = CancellationToken::new();
        let mut rollup = MockRollup::new();
        let finalized_head = Arc::clone(&head);
        rollup.expect_sync_status().returning(move || {
            Ok(test_sync_status(finalized_head.load(Ordering::SeqCst) as u64, B256::ZERO))
        });
        rollup.expect_output_at_block().returning(|block| {
            Ok(OutputAtBlock {
                output_root: root(block),
                block_ref: test_l2_block_ref(block, root(block)),
            })
        });
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", server.local_addr().unwrap());
        let mut module = RpcModule::new(());
        let advance_observed = new_range_dispatched.clone();
        module
            .register_method("prover_proveBlockRange", move |params, _, _| {
                let request: ProveBlockRangeRequest = params.one()?;
                let ProofRequestKind::Tee(tee) = &request.proof.request else {
                    panic!("expected TEE")
                };
                if tee.proof.claimed_l2_block_number == 300 {
                    advance_observed.cancel();
                }
                Ok::<_, ErrorObjectOwned>(ProveBlockRangeResponse {
                    session_id: request.proof.session_id,
                })
            })
            .unwrap();
        let started = scan_started.clone();
        module
            .register_async_method("prover_getProof", move |params, _, _| {
                let started = started.clone();
                let head = Arc::clone(&head);
                async move {
                    let request: GetProofRequest = params.one()?;
                    if request.session_id == session(200) {
                        head.store(300, Ordering::SeqCst);
                        started.cancel();
                        pending::<RpcResult<GetProofResponse>>().await
                    } else {
                        Ok(unfinished(ProofStatus::Queued))
                    }
                }
            })
            .unwrap();
        let handle = server.start(module);
        let requester =
            ProofRequesterClient::connect(&ProverServiceClientConfig::new(endpoint)).unwrap();
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().never();
        let pipeline = test_pipeline(
            Arc::new(requester),
            registry,
            Arc::new(rollup),
            Duration::from_secs(5),
            Duration::from_millis(10),
            None,
        );
        let cancel = CancellationToken::new();
        let run = pipeline.run(cancel.clone());
        tokio::pin!(run);
        tokio::select! {
            () = &mut run => panic!("pipeline must run until cancelled"),
            result = tokio::time::timeout(Duration::from_secs(2), async {
                scan_started.cancelled().await;
                new_range_dispatched.cancelled().await;
            }) => result.expect("dispatch must advance before the blocked sweep times out"),
        }
        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(100), &mut run)
            .await
            .expect("cancellation must drop a blocked sweep promptly");
        handle.stop().unwrap();
        tokio::time::timeout(Duration::from_secs(1), handle.stopped()).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn signer_sweeps_preserve_historical_backlog_and_wrap_at_its_end() {
        // Current-session prefixes must neither truncate a later page nor prevent a real wrap.
        for (cursor, historical_bound, session_bound, expected_cursor, first_block) in [
            (Some(6400), 6500, 0, None, 6500),
            (Some(6400), 13000, 300, Some(12800), 6500),
            (Some(6400), 13000, 6600, Some(12800), 6500),
            (Some(6400), 6400, 6400, Some(6400), 100),
            (None, 6500, 6500, Some(6400), 100),
        ] {
            let lookups = Arc::new(Mutex::new(Vec::new()));
            let observed = Arc::clone(&lookups);
            let mut requester = MockRequester::new();
            requester.expect_get_proof().returning(move |request| {
                observed.lock().unwrap().push(request.session_id);
                Ok(unfinished(ProofStatus::Queued))
            });
            requester.expect_delete_proofs_by_tee_signer().never();
            let mut registry = MockTeeSignerRegistry::new();
            registry.expect_registered_signers().never();
            let pipeline = test_pipeline(
                Arc::new(requester),
                registry,
                rollup(historical_bound),
                Duration::from_secs(1),
                Duration::from_millis(10),
                None,
            );
            let recovered = crate::RecoveredState {
                parent_address: Address::ZERO,
                output_root: root(0),
                l2_block_number: 0,
            };
            let (_tx, rx) = tokio::sync::watch::channel(Some((recovered, session_bound)));
            let mut source = Some(recovered);
            let mut cursor = cursor;
            let mut bound = historical_bound;
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(15),
                    pipeline.tee_recovery_loop(rx, &mut source, &mut cursor, &mut bound),
                )
                .await
                .is_err()
            );
            let lookups = lookups.lock().unwrap();
            assert_eq!(lookups.first(), Some(&session(first_block)));
            assert_eq!(cursor, expected_cursor);
            assert_eq!(bound, historical_bound);
            if expected_cursor == Some(12800) {
                assert_eq!(lookups.last(), Some(&session(12800)));
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_snapshot_resets_scan_state_on_in_flight_epoch_change() {
        let previous = crate::RecoveredState {
            parent_address: Address::ZERO,
            output_root: root(0),
            l2_block_number: 0,
        };
        for recovered in [
            crate::RecoveredState { output_root: B256::repeat_byte(1), ..previous },
            crate::RecoveredState { parent_address: Address::repeat_byte(1), ..previous },
            crate::RecoveredState { l2_block_number: 100, ..previous },
        ] {
            let (tx, rx) = tokio::sync::watch::channel(Some((previous, 6400)));
            let mut requester = MockRequester::new();
            requester.expect_get_proof().returning(move |request| {
                if request.session_id == session(200) {
                    // Recovery changes while the old-epoch sweep is still in flight.
                    tx.send_replace(Some((recovered, 300)));
                    Ok(succeeded(200, Address::repeat_byte(0x11)))
                } else {
                    Ok(unfinished(ProofStatus::Queued))
                }
            });
            requester.expect_delete_proofs_by_tee_signer().times(1).returning(|_| Ok(1));
            let mut registry = MockTeeSignerRegistry::new();
            registry.expect_registered_signers().times(1).returning(|| Ok(vec![]));
            let pipeline = test_pipeline(
                Arc::new(requester),
                registry,
                rollup(6500),
                Duration::from_secs(1),
                Duration::from_millis(10),
                None,
            );
            let mut source = Some(previous);
            let mut cursor = None;
            let mut bound = 6400;
            tokio::time::timeout(
                Duration::from_secs(1),
                pipeline.tee_recovery_loop(rx, &mut source, &mut cursor, &mut bound),
            )
            .await
            .expect("cleanup must return after recording the latest coherent epoch");
            assert_eq!(source, Some(recovered));
            assert_eq!(cursor, None);
            assert_eq!(bound, 300, "accepted bounds from different epochs must not be merged");
        }
    }

    #[tokio::test]
    async fn ambiguous_deletes_reach_later_pages_during_stalled_redispatch() {
        // Stalls cover zero accepted prefix, a partial first page, and exactly one full page.
        for stalled_target in [100, 400, 6500] {
            assert_later_pages_during_stalled_redispatch(stalled_target, false).await;
        }
    }

    #[tokio::test]
    async fn cleanup_preserves_targets_accepted_during_an_in_flight_sweep() {
        for stalled_target in [100, 400] {
            assert_later_pages_during_stalled_redispatch(stalled_target, true).await;
        }
    }

    async fn assert_later_pages_during_stalled_redispatch(
        stalled_target: u64,
        advance_during_sweep: bool,
    ) {
        let first = Address::repeat_byte(0x11);
        let later = Address::repeat_byte(0x22);
        let last = (TeeProofRecovery::MAX_TARGETS_PER_TICK as u64 + 1) * 100;
        let initial_dispatch_done = CancellationToken::new();
        let first_page_dispatched = CancellationToken::new();
        let allow_later_dispatch = CancellationToken::new();
        let redispatch_stalled = CancellationToken::new();
        let later_deleted = CancellationToken::new();
        let delete_attempts = Arc::new(AtomicUsize::new(0));
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", server.local_addr().unwrap());
        let mut module = RpcModule::new(());
        let initial_done = initial_dispatch_done.clone();
        let first_page = first_page_dispatched.clone();
        let allow_later = allow_later_dispatch.clone();
        let stalled = redispatch_stalled.clone();
        let attempts = Arc::clone(&delete_attempts);
        module
            .register_async_method("prover_proveBlockRange", move |params, _, _| {
                let initial_done = initial_done.clone();
                let first_page = first_page.clone();
                let allow_later = allow_later.clone();
                let stalled = stalled.clone();
                let attempts = Arc::clone(&attempts);
                async move {
                    let request: ProveBlockRangeRequest = params.one()?;
                    let ProofRequestKind::Tee(tee) = &request.proof.request else {
                        panic!("expected TEE")
                    };
                    if attempts.load(Ordering::SeqCst) > 0
                        && tee.proof.claimed_l2_block_number == stalled_target
                    {
                        stalled.cancel();
                        return pending::<RpcResult<ProveBlockRangeResponse>>().await;
                    }
                    if tee.proof.claimed_l2_block_number == last - 100 {
                        first_page.cancel();
                    }
                    if advance_during_sweep {
                        if tee.proof.claimed_l2_block_number == last {
                            allow_later.cancelled().await;
                        } else if tee.proof.claimed_l2_block_number == last + 100 {
                            // Issuing the next request proves that `last` was already accepted.
                            initial_done.cancel();
                            return pending::<RpcResult<ProveBlockRangeResponse>>().await;
                        }
                    }
                    if !advance_during_sweep && tee.proof.claimed_l2_block_number == last {
                        initial_done.cancel();
                    }
                    Ok(ProveBlockRangeResponse { session_id: request.proof.session_id })
                }
            })
            .unwrap();
        let initial_done = initial_dispatch_done.clone();
        let first_page = first_page_dispatched.clone();
        let allow_later = allow_later_dispatch.clone();
        let attempts = Arc::clone(&delete_attempts);
        module
            .register_async_method("prover_getProof", move |params, _, _| {
                let initial_done = initial_done.clone();
                let first_page = first_page.clone();
                let allow_later = allow_later.clone();
                let attempts = Arc::clone(&attempts);
                async move {
                    let request: GetProofRequest = params.one()?;
                    if advance_during_sweep
                        && request.session_id == session(200)
                        && first_page.is_cancelled()
                        && attempts.load(Ordering::SeqCst) == 0
                    {
                        // The sweep already sampled the first-page bound. Allow dispatch to
                        // accept the later target before returning the deletion candidate.
                        allow_later.cancel();
                        initial_done.cancelled().await;
                    }
                    let response =
                        if request.session_id == session(200) && initial_done.is_cancelled() {
                            succeeded(200, first)
                        } else if request.session_id == session(last)
                            && attempts.load(Ordering::SeqCst) > 0
                        {
                            succeeded(last, later)
                        } else {
                            unfinished(ProofStatus::Queued)
                        };
                    Ok::<_, ErrorObjectOwned>(response)
                }
            })
            .unwrap();
        let attempts = Arc::clone(&delete_attempts);
        let deleted = later_deleted.clone();
        let stalled = redispatch_stalled.clone();
        module
            .register_method("prover_deleteProofsByTeeSigner", move |params, _, _| {
                let request: DeleteProofsByTeeSignerRequest = params.one()?;
                if request.tee_signer == first {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err(ErrorObjectOwned::owned(-32000, "unknown deletion outcome", None::<()>))
                } else {
                    assert_eq!(request.tee_signer, later);
                    assert!(
                        stalled.is_cancelled(),
                        "later signer must be found during redispatch lag"
                    );
                    deleted.cancel();
                    Ok(1_u64)
                }
            })
            .unwrap();
        let handle = server.start(module);
        let requester =
            ProofRequesterClient::connect(&ProverServiceClientConfig::new(endpoint)).unwrap();
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().returning(|| Ok(vec![]));
        let pipeline = test_pipeline(
            Arc::new(requester),
            registry,
            rollup(if advance_during_sweep { last + 100 } else { last }),
            Duration::from_secs(1),
            Duration::from_millis(100),
            None,
        );
        let cancel = CancellationToken::new();
        let run = pipeline.run(cancel.clone());
        tokio::pin!(run);
        let result = tokio::select! {
            () = &mut run => panic!("pipeline must run until cancelled"),
            result = tokio::time::timeout(Duration::from_secs(3), later_deleted.cancelled()) => result,
        };
        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(100), &mut run).await.unwrap();
        handle.stop().unwrap();
        tokio::time::timeout(Duration::from_secs(1), handle.stopped()).await.unwrap();
        result.expect("later signer must be discovered despite persistently ambiguous cleanup and redispatch lag");
        assert!(delete_attempts.load(Ordering::SeqCst) > 0);
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguous_deletes_back_off_and_later_scan_pages_survive_restarts() {
        let first = Address::repeat_byte(0x11);
        let later = Address::repeat_byte(0x22);
        let last = (TeeProofRecovery::MAX_TARGETS_PER_TICK as u64 + 1) * 100;
        let poll_interval = Duration::from_secs(12);
        let started = Instant::now();
        let deletion_times = Arc::new(Mutex::new(Vec::new()));
        let deleted_later = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let mut requester = requester((100..=last).step_by(100).map(|block| {
            let proof = if block == 200 {
                succeeded(block, first)
            } else if block == last {
                succeeded(block, later)
            } else {
                unfinished(ProofStatus::Queued)
            };
            (block, proof)
        }));
        let later_deleted = Arc::clone(&deleted_later);
        let stop = cancel.clone();
        requester.expect_prove_block_range().returning(move |request| {
            let ProofRequestKind::Tee(tee) = &request.proof.request else { panic!("expected TEE") };
            if tee.proof.claimed_l2_block_number == last && later_deleted.load(Ordering::SeqCst) {
                stop.cancel();
            }
            Ok(ProveBlockRangeResponse { session_id: request.proof.session_id })
        });
        let times = Arc::clone(&deletion_times);
        let later_deleted = Arc::clone(&deleted_later);
        let later_attempts = Arc::new(AtomicUsize::new(0));
        requester.expect_delete_proofs_by_tee_signer().returning(move |request| {
            times.lock().unwrap().push(Instant::now());
            if request.tee_signer == first {
                Err(JsonRpcClientError::RequestTimeout.into())
            } else {
                assert_eq!(request.tee_signer, later);
                if later_attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                    return Err(JsonRpcClientError::RequestTimeout.into());
                }
                later_deleted.store(true, Ordering::SeqCst);
                Ok(1)
            }
        });
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().returning(|| Ok(vec![]));
        let pipeline = test_pipeline(
            Arc::new(requester),
            registry,
            rollup(last),
            Duration::from_secs(1),
            poll_interval,
            None,
        );
        tokio::time::timeout(poll_interval * 7, pipeline.run(cancel))
            .await
            .expect("later signer must be cleaned and redispatched despite an earlier timeout");
        assert!(deleted_later.load(Ordering::SeqCst));
        let times = deletion_times.lock().unwrap();
        assert_eq!(times.len(), 6, "cleanup restarts must retain later scan pages");
        assert!(times[0] - started >= poll_interval);
        assert!(times.windows(2).all(|pair| pair[1] - pair[0] >= poll_interval));
    }

    #[tokio::test]
    async fn failed_read_does_not_block_recovery_of_later_signers() {
        let mut requester = MockRequester::new();
        requester
            .expect_get_proof()
            .withf(|request| request.session_id == session(100))
            .times(1)
            .returning(|_| Err(JsonRpcClientError::RequestTimeout.into()));
        requester
            .expect_get_proof()
            .withf(|request| request.session_id == session(200))
            .times(1)
            .returning(|_| Ok(succeeded(200, Address::repeat_byte(0x11))));
        requester.expect_delete_proofs_by_tee_signer().times(1).returning(|_| Ok(1));
        let mut registry = MockTeeSignerRegistry::new();
        registry.expect_registered_signers().times(1).returning(|| Ok(vec![]));

        assert!(recovery(requester, registry, 200).tick(0, 200, &mut None).await);
    }

    #[tokio::test]
    async fn registry_reader_rejects_zero_implementation_or_registry_addresses() {
        for zero_implementation in [true, false] {
            let factory = Address::repeat_byte(0xf0);
            let implementation = Address::repeat_byte(0xa1);
            let verifier = Address::repeat_byte(0xb1);
            let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
            let provider = RootProvider::new_http(
                format!("http://{}", server.local_addr().unwrap()).parse().unwrap(),
            );
            let mut module = RpcModule::new(());
            module
                .register_method("eth_call", move |params, _, _| {
                    let params = params.parse::<Vec<Value>>()?;
                    let to = params[0]["to"].as_str().unwrap().parse::<Address>().unwrap();
                    let output = if to == factory {
                        if zero_implementation { Address::ZERO } else { implementation }
                    } else if to == implementation {
                        verifier
                    } else if to == verifier {
                        Address::ZERO
                    } else {
                        panic!("must not enumerate an unresolved registry: {to}");
                    };
                    Ok::<_, ErrorObjectOwned>(format!("0x{}", hex::encode(output.abi_encode())))
                })
                .unwrap();
            let handle = server.start(module);
            let registry_reader = OnchainTeeSignerRegistry::new(
                Arc::new(DisputeGameFactoryContractClient::new(factory, provider.clone())),
                Arc::new(AggregateVerifierContractClient::new(provider)),
                0,
            );

            let error = registry_reader
                .registered_signers()
                .await
                .expect_err("unresolved contracts must not be treated as an empty registry");
            assert!(error.to_string().contains("address is zero"));
            handle.stop().unwrap();
            handle.stopped().await;
        }
    }
}
