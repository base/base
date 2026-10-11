//! Proactive recovery of completed proofs from unregistered TEE signers.

use std::{collections::HashSet, sync::Arc, time::Duration};

use alloy_primitives::Address;
use async_trait::async_trait;
use base_proof_contracts::{AggregateVerifierContractClient, DisputeGameFactoryClient};
use base_proof_rpc::RollupProvider;
use base_prover_service_client::ProofRequesterProvider;
use base_prover_service_protocol::{DeleteProofsByTeeSignerRequest, GetProofRequest, ProofStatus};
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
    /// Returns a fresh registration snapshot. Callers must not cache it across sweeps.
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
/// The prover service's signer deletion API preserves queued/running jobs. A fresh
/// registration snapshot is used each tick, so re-registration is observed too.
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
    /// state changes. Returns `true` when both pipeline cursors must restart so deleted ranges
    /// behind the dispatcher are resubmitted. An interrupted or failed delete also requires a
    /// restart: its server-side outcome may be unknown after the client stops waiting.
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
        let mut registered_signers = None;
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
            let registered = match &registered_signers {
                Some(signers) => signers,
                None => registered_signers.insert(
                    self.registry.registered_signers().await?.into_iter().collect::<HashSet<_>>(),
                ),
            };
            if registered.contains(&proof.tee_signer) {
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
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use alloy_primitives::{B256, U256, hex};
    use alloy_provider::RootProvider;
    use alloy_sol_types::SolValue;
    use base_proof_contracts::DisputeGameFactoryContractClient;
    use base_prover_service_client::{
        ProofRequesterClient, ProverServiceClientConfig, ProverServiceClientError,
    };
    use base_prover_service_protocol::{
        DeleteProofRequest, GetProofResponse, ListProofsRequest, ListProofsResponse,
        PROOF_REQUEST_NOT_FOUND_MESSAGE, ProofRequestKind, ProofResult, ProveBlockRangeRequest,
        ProveBlockRangeResponse, TeeKind, TeeProofResult, ZkProofResult, ZkVm,
    };
    use jsonrpsee::{
        RpcModule,
        core::{RpcResult, client::Error as JsonRpcClientError},
        server::ServerBuilder,
        types::ErrorObjectOwned,
    };
    use mockall::Sequence;
    use serde_json::Value;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        DriverConfig, ProofCollector, ProofDispatcher, ProofRecovery, ProofRecoveryConfig,
        ProofSubmitter, ProvingPipeline,
        test_utils::{
            MockAggregateVerifier, MockAnchorStateRegistry, MockDisputeGameFactory, MockL1, MockL2,
            MockOutputProposer, MockRollupClient, test_anchor_root, test_fixed_interval_resolver,
            test_interval_resolver, test_proposal, test_sync_status,
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
        registry.expect_registered_signers().times(1).returning(move || Ok(vec![healthy]));

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
        registry.expect_registered_signers().times(1).returning(|| Ok(vec![]));

        assert!(recovery(requester, registry, 200).tick(0, 200, &mut None).await);
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
        let rollup = rollup(200);
        let intervals = test_fixed_interval_resolver(100, 100);
        let factory = Arc::new(MockDisputeGameFactory::default());
        let config =
            DriverConfig { poll_interval: Duration::from_millis(10), ..Default::default() };
        let dispatcher = ProofDispatcher::new(
            Arc::clone(&requester),
            Arc::new(MockL1::new(1000)),
            Arc::new(MockL2),
            Arc::<MockRollupClient>::clone(&rollup),
            Arc::clone(&intervals),
            Address::ZERO,
        );
        let chain_recovery = Arc::new(ProofRecovery::new(
            ProofRecoveryConfig {
                game_type: 0,
                anchor_state_registry_address: Address::ZERO,
                scan_concurrency: 1,
            },
            Arc::<MockRollupClient>::clone(&rollup),
            Arc::new(MockAnchorStateRegistry {
                anchor_root: test_anchor_root(0),
                anchor_game: Address::ZERO,
            }),
            Arc::<MockDisputeGameFactory>::clone(&factory),
            Arc::clone(&intervals),
        ));
        let submitter = ProofSubmitter::new(
            Arc::new(MockOutputProposer::default()),
            Arc::<MockRollupClient>::clone(&rollup),
            factory,
            Arc::new(MockAggregateVerifier::default()),
            Arc::clone(&intervals),
            &config,
        );
        let collector = ProofCollector::new(
            Arc::clone(&requester),
            Arc::clone(&rollup),
            submitter,
            Arc::clone(&intervals),
            config.submit_timeout,
        );
        let recovery = TeeProofRecovery::new(
            requester,
            rollup,
            intervals,
            Arc::new(registry),
            Duration::from_secs(1),
        );
        let pipeline = ProvingPipeline::new(config, dispatcher, chain_recovery, collector)
            .with_tee_proof_recovery(recovery);

        tokio::time::timeout(Duration::from_secs(2), pipeline.run(cancel)).await.expect(
            "deleted ranges must be redispatched without waiting for the earlier queued proof",
        );
        assert!(deleted.load(Ordering::SeqCst));
        assert_eq!(dispatches.load(Ordering::SeqCst), 2);
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
