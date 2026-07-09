//! Integration tests for the challenger [`Driver`] loop.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::{Address, B256, Bytes};
use base_challenger::{
    AnchorUpdater, BondManager, ChallengeSubmitter, ChallengerProofAdapter, DisputeIntent, Driver,
    DriverComponents, DriverConfig, GameScanner, L1HeadProvider, OutputValidator, PendingProof,
    PendingProofs, ProofKind, ProofPhase, ProofUpdate, TeeConfig,
    test_utils::{
        DEFAULT_L1_HEAD, DEFAULT_TEE_PROVER, MockAggregateVerifier, MockBondTransactionSubmitter,
        MockDisputeGameFactory, MockGameState, MockL1HeadProvider, MockL2Provider, MockTxManager,
        MockZkProofProvider, MockZkProofState, TEST_DISCOVERY_INTERVAL, addr,
        build_test_header_and_account, empty_factory, factory_game, mock_anchor_registry,
        mock_state, mock_state_with_tee, receipt_with_status,
    },
};
use base_proof_contracts::{
    AggregateVerifierClient, ContractError, DisputeGameFactoryClient, GameAtIndex, GameStatus,
};
use base_proof_primitives::Proposal;
use base_protocol::OutputRoot;
use base_prover_service_protocol::{
    ProofResult as ApiProofResult, ProofStatus, SnarkGroth16ProofRequest, TeeKind, TeeProofResult,
    ZkProofRequest, ZkVm,
};
use base_runtime::TokioRuntime;
use base_tx_manager::TxManagerError;
use tokio_util::sync::CancellationToken;

const STORAGE_HASH: B256 = B256::repeat_byte(0xBB);
const ZK_PROVER_ADDR: Address = Address::new([0xCC; 20]);
const DEFAULT_TX_HASH: B256 = B256::repeat_byte(0xDD);
const BOGUS_ROOT: B256 = B256::repeat_byte(0xFF);
const BOGUS_CLAIM: B256 = B256::repeat_byte(0x01);

/// Returns a base [`MockGameState`] with standard test defaults.
/// Override individual fields with struct update syntax:
/// `MockGameState { tee_prover: DEFAULT_TEE_PROVER, ..game_state(20) }`.
fn game_state(l2_block_number: u64) -> MockGameState {
    MockGameState {
        game_info: base_proof_contracts::GameInfo {
            root_claim: BOGUS_CLAIM,
            l2_block_number,
            parent_address: Address::ZERO,
        },
        starting_block_number: 10,
        l1_head: DEFAULT_L1_HEAD,
        ..Default::default()
    }
}

fn empty_verifier() -> Arc<MockAggregateVerifier> {
    Arc::new(MockAggregateVerifier::new(HashMap::new()))
}

/// Builds a test driver with the given mocks.
fn test_driver(
    factory: Arc<MockDisputeGameFactory>,
    verifier: Arc<MockAggregateVerifier>,
    l2_provider: Arc<MockL2Provider>,
    zk_prover: Arc<MockZkProofProvider>,
    tx_manager: MockTxManager,
) -> Driver<MockL2Provider, MockZkProofProvider, MockTxManager> {
    test_driver_with_tee(factory, verifier, l2_provider, zk_prover, tx_manager, None)
}

/// Builds a test driver with an optional TEE config.
fn test_driver_with_tee(
    factory: Arc<MockDisputeGameFactory>,
    verifier: Arc<MockAggregateVerifier>,
    l2_provider: Arc<MockL2Provider>,
    zk_prover: Arc<MockZkProofProvider>,
    tx_manager: MockTxManager,
    tee: Option<TeeConfig>,
) -> Driver<MockL2Provider, MockZkProofProvider, MockTxManager> {
    let anchor_registry = mock_anchor_registry(Address::ZERO);
    let scanner = GameScanner::new(
        Arc::clone(&factory) as Arc<dyn DisputeGameFactoryClient>,
        Arc::clone(&verifier) as Arc<dyn AggregateVerifierClient>,
        Arc::clone(&anchor_registry),
    );
    let validator = OutputValidator::new(Arc::clone(&l2_provider));
    let submitter = ChallengeSubmitter::new(tx_manager);

    let config = DriverConfig {
        poll_interval: Duration::from_millis(10),
        max_proof_duration: Duration::from_secs(4 * 60 * 60),
        tee_submit_retry_limit: 3,
        cancel: CancellationToken::new(),
    };

    Driver::new(
        config,
        DriverComponents {
            scanner,
            validator,
            proof_requester: zk_prover,
            submitter,
            tee,
            verifier_client: verifier as Arc<dyn AggregateVerifierClient>,
            bond_manager: None,
            anchor_updater: AnchorUpdater::new(
                factory,
                anchor_registry,
                l2_provider as Arc<dyn base_proof_rpc::L2Provider>,
                Address::repeat_byte(0xAA),
                1,
                100,
                100,
            ),
        },
    )
}

fn default_zk_prover() -> Arc<MockZkProofProvider> {
    Arc::new(MockZkProofProvider { session_id: "test-session".to_string(), ..Default::default() })
}

fn default_tx_manager() -> MockTxManager {
    MockTxManager::new(Ok(receipt_with_status(true, B256::repeat_byte(0xAA))))
}

fn default_l2() -> Arc<MockL2Provider> {
    Arc::new(MockL2Provider::new())
}

fn single_game_factory() -> Arc<MockDisputeGameFactory> {
    Arc::new(MockDisputeGameFactory::new(vec![factory_game(0, 1)]))
}

fn single_game_verifier(state: MockGameState) -> Arc<MockAggregateVerifier> {
    Arc::new(MockAggregateVerifier::new(HashMap::from([(addr(0), state)])))
}

fn tee_config(l1_head_provider: Arc<dyn L1HeadProvider>) -> TeeConfig {
    TeeConfig { l1_head_provider }
}

const fn tee_api_result(aggregate_proposal: Proposal) -> ApiProofResult {
    ApiProofResult::Tee(TeeProofResult {
        aggregate_proposal,
        proposals: Vec::new(),
        tee_kind: TeeKind::AwsNitro,
    })
}

fn default_ready_proof(intent: DisputeIntent) -> PendingProof {
    let request = SnarkGroth16ProofRequest {
        proof: ZkProofRequest {
            start_block_number: 15,
            number_of_blocks_to_prove: 5,
            sequence_window: None,
            l1_head: Some(DEFAULT_L1_HEAD),
            intermediate_root_interval: None,
            zk_vm: ZkVm::Sp1,
        },
        prover_address: addr(0),
    };

    PendingProof::ready(
        Bytes::from_static(&[0x01, 0xDE, 0xAD]),
        1,
        B256::repeat_byte(0xEE),
        request,
        intent,
    )
}

fn succeeded_zk_prover(session_id: &str, receipt: Vec<u8>) -> Arc<MockZkProofProvider> {
    Arc::new(MockZkProofProvider {
        session_id: session_id.to_string(),
        state: Mutex::new(MockZkProofState {
            proof_status: ProofStatus::Succeeded,
            proof: receipt,
            ..Default::default()
        }),
    })
}

fn failed_zk_prover(session_id: &str) -> Arc<MockZkProofProvider> {
    Arc::new(MockZkProofProvider {
        session_id: session_id.to_string(),
        state: Mutex::new(MockZkProofState {
            proof_status: ProofStatus::Failed,
            ..Default::default()
        }),
    })
}

/// Builds the common L2 provider, factory, and output roots shared by most
/// invalid-game test scenarios. Layout: starting=10, `l2_block=20`,
/// interval=5, checkpoints at blocks 15 and 20.
fn base_game_mocks() -> (Arc<MockL2Provider>, Arc<MockDisputeGameFactory>, B256, B256) {
    let (header_15, account_15) = build_test_header_and_account(15, STORAGE_HASH);
    let root_15 =
        OutputRoot::from_parts(header_15.state_root, STORAGE_HASH, header_15.hash_slow()).hash();
    let (header_20, account_20) = build_test_header_and_account(20, STORAGE_HASH);
    let root_20 =
        OutputRoot::from_parts(header_20.state_root, STORAGE_HASH, header_20.hash_slow()).hash();

    let mut l2 = MockL2Provider::new();
    l2.insert_block(15, header_15, account_15);
    l2.insert_block(20, header_20, account_20);
    let l2 = Arc::new(l2);

    let factory = single_game_factory();

    (l2, factory, root_15, root_20)
}

/// Builds the common L2, factory, and verifier mocks for an invalid-game
/// scenario: starting=10, `l2_block=20`, interval=5, checkpoints at 15 and
/// 20 with a correct root at 15 and a bogus root at 20 (invalid index 1).
fn invalid_game_mocks()
-> (Arc<MockL2Provider>, Arc<MockDisputeGameFactory>, Arc<MockAggregateVerifier>) {
    let (l2, factory, root_15, _root_20) = base_game_mocks();

    let verifier = single_game_verifier(MockGameState {
        tee_prover: DEFAULT_TEE_PROVER,
        intermediate_output_roots: vec![root_15, BOGUS_ROOT],
        ..game_state(20)
    });

    (l2, factory, verifier)
}

/// Builds a driver with a single pending `ReadyToSubmit` proof at `addr(0)`
/// whose verifier reports the given `game_state`.
fn driver_with_ready_proof(
    game_state: MockGameState,
) -> Driver<MockL2Provider, MockZkProofProvider, MockTxManager> {
    let factory = single_game_factory();
    let verifier = single_game_verifier(game_state);
    let l2 = default_l2();
    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());
    driver.pending_proofs.insert(addr(0), default_ready_proof(DisputeIntent::Challenge));
    driver
}

#[tokio::test]
async fn test_step_no_candidates() {
    let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
    let verifier = empty_verifier();
    let l2 = default_l2();

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());

    driver.step().await.unwrap();
}

#[tokio::test]
async fn test_step_valid_game_skipped() {
    // l2_block_number - starting_block_number < intermediate_block_interval
    // → expected_count = 0 → trivially valid, no proof requested.
    let factory = single_game_factory();
    let verifier =
        single_game_verifier(MockGameState { tee_prover: DEFAULT_TEE_PROVER, ..game_state(14) });
    let l2 = default_l2();

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());

    driver.step().await.unwrap();
}

#[tokio::test]
async fn test_step_validation_error_blocks_not_available() {
    // Game with intermediate roots, but checkpoint blocks are unavailable.
    // Validator returns BlockNotAvailable → process_candidate skips gracefully.
    let factory = single_game_factory();
    let verifier = single_game_verifier(MockGameState {
        tee_prover: DEFAULT_TEE_PROVER,
        intermediate_output_roots: vec![BOGUS_ROOT, B256::repeat_byte(0xEE)],
        ..game_state(20)
    });

    let mut l2 = MockL2Provider::new();
    l2.error_blocks.push(15);
    l2.error_blocks.push(20);
    let l2 = Arc::new(l2);

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());

    driver.step().await.unwrap();
}

#[tokio::test]
async fn test_step_invalid_game_proof_succeeded() {
    let (l2, factory, verifier) = invalid_game_mocks();

    let zk = succeeded_zk_prover("proof-123", vec![0xDE, 0xAD]);

    let tx_manager = default_tx_manager();

    let mut driver = test_driver(factory, Arc::clone(&verifier), l2, zk, tx_manager);

    // Step 1: proof initiated, not yet polled.
    driver.step().await.unwrap();
    assert!(
        driver.pending_proofs.contains_key(&addr(0)),
        "proof should be pending after initiation"
    );

    // Simulate the onchain effect of a successful challenge: game is resolved.
    verifier.update_game(
        addr(0),
        MockGameState { status: GameStatus::ChallengerWins, ..game_state(20) },
    );

    // Step 2: proof polled → Succeeded → nullification submitted → entry removed.
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "entry should be removed after successful nullification"
    );
}

#[tokio::test]
async fn test_step_invalid_game_proof_failed() {
    let (l2, factory, verifier) = invalid_game_mocks();

    let zk = failed_zk_prover("proof-fail");

    let tx_manager = default_tx_manager();

    let mut driver = test_driver(factory, verifier, l2, zk, tx_manager);

    // Step 1: proof initiated but not yet polled (deferred to next tick).
    driver.step().await.unwrap();
    assert!(
        driver.pending_proofs.contains_key(&addr(0)),
        "proof should be pending after initiation"
    );

    // Step 2: poll discovers Failed → NeedsRetry → handle_proof_retry
    // re-initiates with retry_count == 1.
    driver.step().await.unwrap();

    // Entry should be retained in AwaitingProof phase (re-initiated) with retry_count == 1.
    let entry =
        driver.pending_proofs.get(&addr(0)).expect("entry should be retained after failure");
    assert!(
        matches!(entry.phase, ProofPhase::AwaitingProof { .. }),
        "phase should be AwaitingProof after re-initiation"
    );
    assert_eq!(entry.retry_count, 1);
}

#[tokio::test]
async fn test_step_scan_error_propagated() {
    /// Factory that always fails on `game_count`.
    #[derive(Debug)]
    struct FailingFactory;

    #[async_trait::async_trait]
    impl base_proof_contracts::DisputeGameFactoryClient for FailingFactory {
        async fn game_count(&self) -> Result<u64, ContractError> {
            Err(ContractError::Validation("simulated game_count failure".into()))
        }

        async fn game_at_index(&self, _index: u64) -> Result<GameAtIndex, ContractError> {
            unreachable!()
        }

        async fn init_bonds(
            &self,
            _game_type: u32,
        ) -> Result<alloy_primitives::U256, ContractError> {
            unreachable!()
        }

        async fn game_impls(&self, _game_type: u32) -> Result<Address, ContractError> {
            unreachable!()
        }

        async fn games(
            &self,
            _game_type: u32,
            _root_claim: alloy_primitives::B256,
            _extra_data: alloy_primitives::Bytes,
        ) -> Result<alloy_primitives::Address, ContractError> {
            unreachable!()
        }
    }

    let factory = Arc::new(FailingFactory);
    let anchor_registry = mock_anchor_registry(Address::ZERO);
    let verifier = empty_verifier();
    let scanner = GameScanner::new(
        Arc::clone(&factory) as Arc<dyn DisputeGameFactoryClient>,
        Arc::clone(&verifier) as Arc<dyn AggregateVerifierClient>,
        Arc::clone(&anchor_registry),
    );

    let l2 = default_l2();
    let validator = OutputValidator::new(Arc::clone(&l2));
    let submitter = ChallengeSubmitter::new(default_tx_manager());

    let config = DriverConfig {
        poll_interval: Duration::from_millis(10),
        max_proof_duration: Duration::from_secs(4 * 60 * 60),
        tee_submit_retry_limit: 3,
        cancel: CancellationToken::new(),
    };

    let mut driver = Driver::new(
        config,
        DriverComponents {
            scanner,
            validator,
            proof_requester: default_zk_prover(),
            submitter,
            tee: None,
            verifier_client: verifier as Arc<dyn AggregateVerifierClient>,
            bond_manager: None::<BondManager<TokioRuntime>>,
            anchor_updater: AnchorUpdater::new(
                factory,
                anchor_registry,
                l2 as Arc<dyn base_proof_rpc::L2Provider>,
                Address::repeat_byte(0xAA),
                1,
                100,
                100,
            ),
        },
    );

    let result = driver.step().await;
    assert!(result.is_err(), "scan error should propagate");
}

#[tokio::test]
async fn test_step_pending_proof_skips_prove_block() {
    let (l2, factory, verifier) = invalid_game_mocks();

    let zk = Arc::new(MockZkProofProvider {
        session_id: "pending-session".to_string(),
        state: Mutex::new(MockZkProofState { proof: vec![0xBE, 0xEF], ..Default::default() }),
    });

    let tx_manager = default_tx_manager();

    let mut driver = test_driver(factory, Arc::clone(&verifier), l2, Arc::clone(&zk), tx_manager);

    // Step 1: proof is initiated → session stored in AwaitingProof. The mock's
    // proof_status is not polled this tick (pending_proofs is empty at poll time).
    driver.step().await.unwrap();
    assert!(
        driver.pending_proofs.contains_key(&addr(0)),
        "session should be stored in pending_proofs"
    );

    // Simulate the proof completing before the next poll.
    zk.state.lock().unwrap().proof_status = ProofStatus::Succeeded;

    // Simulate the onchain effect: game is resolved after challenge tx.
    verifier.update_game(
        addr(0),
        MockGameState { status: GameStatus::ChallengerWins, ..game_state(20) },
    );

    // Step 2: same game re-discovered → polls existing session, proof succeeds,
    // challenge tx submitted, session removed from pending_proofs.
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "session should be removed after proof succeeded"
    );
}

#[tokio::test]
async fn test_step_nullification_failure_preserves_proof() {
    let (l2, factory, verifier) = invalid_game_mocks();

    let zk = succeeded_zk_prover("proof-ok", vec![0xDE, 0xAD]);

    // First tx call fails (NonceTooLow), second succeeds.
    let tx_manager = MockTxManager::with_responses(vec![
        Err(TxManagerError::NonceTooLow),
        Ok(receipt_with_status(true, DEFAULT_TX_HASH)),
    ]);

    let mut driver = test_driver(factory, Arc::clone(&verifier), l2, zk, tx_manager);

    // Step 1: proof initiated but not yet polled.
    driver.step().await.unwrap();
    assert!(
        driver.pending_proofs.contains_key(&addr(0)),
        "proof should be pending after initiation"
    );

    // Step 2: proof polled → Succeeded → ReadyToSubmit → dispute tx fails.
    driver.step().await.unwrap();

    // Entry must still be in pending_proofs as ReadyToSubmit.
    let entry = driver.pending_proofs.get(&addr(0)).expect("proof should be preserved");
    assert!(entry.is_ready(), "phase should be ReadyToSubmit after tx failure");

    // Simulate the onchain effect of a successful challenge: game is resolved.
    verifier.update_game(
        addr(0),
        MockGameState { status: GameStatus::ChallengerWins, ..game_state(20) },
    );

    // Step 3: poll_pending_proofs re-submits the challenge tx, now it succeeds.
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "entry should be removed after successful submission"
    );
}

#[tokio::test]
async fn test_poll_or_submit_drops_resolved_game() {
    // Game has resolved (ChallengerWins) — driver should drop the
    // pending proof without attempting submission.
    let mut driver =
        driver_with_ready_proof(mock_state(GameStatus::ChallengerWins, Address::ZERO, 20));
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "resolved game should be removed from pending_proofs"
    );
}

#[tokio::test]
async fn test_poll_or_submit_drops_already_challenged_game() {
    // Game is still IN_PROGRESS but already challenged (zk_prover != ZERO)
    // — driver should drop the pending proof.
    let mut driver =
        driver_with_ready_proof(mock_state(GameStatus::InProgress, ZK_PROVER_ADDR, 20));
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "already-challenged game should be removed from pending_proofs"
    );
}

#[tokio::test]
async fn test_poll_or_submit_drops_nullified_game() {
    // Game is still IN_PROGRESS but both provers are ZERO (nullified)
    // — driver should drop the pending proof without attempting submission.
    let mut driver = driver_with_ready_proof(mock_state_with_tee(
        GameStatus::InProgress,
        Address::ZERO,
        Address::ZERO,
        20,
    ));
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "nullified game should be removed from pending_proofs"
    );
}

#[tokio::test]
async fn test_run_cancellation() {
    let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
    let verifier = empty_verifier();
    let l2 = default_l2();

    let driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());
    driver.cancel.cancel();

    tokio::time::timeout(Duration::from_secs(2), driver.run())
        .await
        .expect("driver.run() should exit promptly after cancellation");
}

#[tokio::test]
async fn test_step_proof_retry_succeeds() {
    let (l2, factory, verifier) = invalid_game_mocks();

    let zk = Arc::new(MockZkProofProvider {
        session_id: "retry-session".to_string(),
        state: Mutex::new(MockZkProofState {
            proof_status: ProofStatus::Failed,
            proof: vec![0xBE, 0xEF],
            ..Default::default()
        }),
    });

    let tx_manager = default_tx_manager();

    let mut driver = test_driver(factory, Arc::clone(&verifier), l2, Arc::clone(&zk), tx_manager);

    // Step 1: proof initiated, not yet polled.
    driver.step().await.unwrap();
    assert!(
        driver.pending_proofs.contains_key(&addr(0)),
        "proof should be pending after initiation"
    );

    // Step 2: proof polled → Failed → NeedsRetry → handle_proof_retry
    // re-initiates prove_block → AwaitingProof with retry_count == 1.
    driver.step().await.unwrap();
    let entry = driver.pending_proofs.get(&addr(0)).expect("entry should exist");
    assert!(
        matches!(entry.phase, ProofPhase::AwaitingProof { .. }),
        "phase should be AwaitingProof after retry re-initiation"
    );
    assert_eq!(entry.retry_count, 1);

    // Simulate proof succeeding on the retry session.
    zk.state.lock().unwrap().proof_status = ProofStatus::Succeeded;

    // Simulate the onchain effect of a successful challenge: game is resolved.
    verifier.update_game(
        addr(0),
        MockGameState { status: GameStatus::ChallengerWins, ..game_state(20) },
    );

    // Step 3: proof succeeds, challenge tx submitted, entry removed.
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "entry should be removed after successful challenge submission"
    );
}

// Regression test for CHAIN-4297 / Immunefi #75829: after a FAILED proof, the
// driver must re-invoke `proveBlockRange` with the same deterministic
// `session_id` so the service-side fix in `create_with_outbox` can requeue
// the row. Independent of the DB layer; fails on a challenger-side regression
// such as dropping the retry call or losing proof-session determinism.
#[tokio::test]
async fn test_step_proof_retry_reuses_deterministic_session_id() {
    let (l2, factory, verifier) = invalid_game_mocks();

    let zk = Arc::new(MockZkProofProvider {
        session_id: String::new(),
        state: Mutex::new(MockZkProofState {
            proof_status: ProofStatus::Failed,
            error_message: Some("transient backend error".into()),
            ..Default::default()
        }),
    });

    let tx_manager = default_tx_manager();
    let mut driver = test_driver(factory, Arc::clone(&verifier), l2, Arc::clone(&zk), tx_manager);

    let expected_session_id = ChallengerProofAdapter::snark_groth16_session_id(addr(0), 1);

    // Step 1: initial proveBlockRange call from initiate_zk_proof.
    driver.step().await.unwrap();
    {
        let log = &zk.state.lock().unwrap().prove_block_range_log;
        assert_eq!(log.len(), 1, "exactly one prove_block_range call on initiation");
        assert_eq!(
            log[0].proof.session_id.as_str(),
            expected_session_id.as_str(),
            "challenger must use game-address/invalid-index session_id on initiation",
        );
    }

    // Step 2: poll observes Failed → NeedsRetry → handle_proof_retry must
    // invoke proveBlockRange again, reusing the same deterministic session_id.
    driver.step().await.unwrap();
    {
        let log = &zk.state.lock().unwrap().prove_block_range_log;
        assert_eq!(log.len(), 2, "retry must invoke prove_block_range a second time");
        assert_eq!(
            log[1].proof.session_id.as_str(),
            expected_session_id.as_str(),
            "retry must reuse the deterministic session_id so the service can requeue",
        );
        assert_eq!(
            log[0].proof.session_id.as_str(),
            log[1].proof.session_id.as_str(),
            "the deterministic session_id must be stable across retries",
        );
    }

    let entry = driver.pending_proofs.get(&addr(0)).expect("entry should be retained after retry");
    assert!(
        matches!(entry.phase, ProofPhase::AwaitingProof { ref session_id, .. } if session_id == &expected_session_id),
        "post-retry phase must be AwaitingProof with the deterministic session_id",
    );
    assert_eq!(entry.retry_count, 1);

    // Simulate the service requeuing on the second prove_block and the proof
    // eventually succeeding on the retry session.
    {
        let mut state = zk.state.lock().unwrap();
        state.proof_status = ProofStatus::Succeeded;
        state.proof = vec![0xDE, 0xAD, 0xBE, 0xEF];
        state.error_message = None;
    }
    verifier.update_game(
        addr(0),
        MockGameState { status: GameStatus::ChallengerWins, ..game_state(20) },
    );

    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "entry should be removed after successful retry submission",
    );
}

#[tokio::test]
async fn test_step_proof_exceeds_max_retries() {
    let (l2, factory, verifier) = invalid_game_mocks();

    let zk = failed_zk_prover("fail-forever");

    let tx_manager = default_tx_manager();
    let mut driver = test_driver(factory, Arc::clone(&verifier), l2, zk, tx_manager);

    // Step 1: proof initiated, not yet polled.
    driver.step().await.unwrap();
    let entry = driver.pending_proofs.get(&addr(0)).expect("entry should exist after initiation");
    assert_eq!(entry.retry_count, 0);

    // Each subsequent step: poll returns Failed → NeedsRetry (retry_count
    // increments), then handle_proof_retry re-initiates → AwaitingProof.
    let max_retries =
        Driver::<MockL2Provider, MockZkProofProvider, MockTxManager>::MAX_PROOF_RETRIES;
    for i in 0..max_retries {
        driver.step().await.unwrap();
        let entry = driver.pending_proofs.get(&addr(0)).expect("entry should exist during retries");
        assert_eq!(entry.retry_count, i + 1);
    }

    // Simulate the onchain effect: mark the game as resolved so the
    // stateless scanner does not re-discover it after the entry is dropped.
    verifier.update_game(
        addr(0),
        MockGameState { status: GameStatus::ChallengerWins, ..game_state(20) },
    );

    // One more step: poll returns Failed → retry_count becomes max_retries + 1,
    // handle_proof_retry sees retry_count > MAX_PROOF_RETRIES and drops the entry.
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "entry should be dropped after exceeding max retries"
    );
}

// ── TEE-first proof sourcing tests ─────────────────────────────────────────

#[tokio::test]
async fn test_step_invalid_game_tee_fails_zk_fallback() {
    // TEE request construction fails → driver falls back to ZK.
    let (l2, factory, verifier) = invalid_game_mocks();

    let tx_manager = default_tx_manager();
    let mut driver = test_driver_with_tee(
        factory,
        Arc::clone(&verifier),
        l2,
        default_zk_prover(),
        tx_manager,
        Some(tee_config(Arc::new(MockL1HeadProvider::failure("dummy")))),
    );

    driver.step().await.unwrap();

    let entry =
        driver.pending_proofs.get(&addr(0)).expect("ZK proof should be pending after TEE fallback");
    assert!(
        matches!(entry.phase, ProofPhase::AwaitingProof { .. }),
        "phase should be AwaitingProof (ZK fallback)"
    );
}

#[tokio::test]
async fn test_step_invalid_game_no_tee_provider_zk_only() {
    // No TEE config → go straight to ZK.
    let (l2, factory, verifier) = invalid_game_mocks();

    let tx_manager = default_tx_manager();
    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), tx_manager);

    driver.step().await.unwrap();

    let entry = driver.pending_proofs.get(&addr(0)).expect("ZK proof should be pending");
    assert!(
        matches!(entry.phase, ProofPhase::AwaitingProof { .. }),
        "phase should be AwaitingProof (ZK, no TEE provider)"
    );
}

#[tokio::test]
async fn test_step_invalid_game_tee_fails_zk_succeeds() {
    let (l2, factory, verifier) = invalid_game_mocks();

    let zk = succeeded_zk_prover("zk-after-tee-fail", vec![0xDE, 0xAD]);

    let tx_manager = default_tx_manager();

    let mut driver = test_driver_with_tee(
        factory,
        Arc::clone(&verifier),
        l2,
        zk,
        tx_manager,
        Some(tee_config(Arc::new(MockL1HeadProvider::failure("dummy")))),
    );

    // Step 1: TEE path is attempted (fails building request), falls back
    // to ZK, proof session initiated (polled on next tick).
    driver.step().await.unwrap();
    assert!(
        driver.pending_proofs.contains_key(&addr(0)),
        "ZK proof should be pending after TEE fallback"
    );

    // Simulate the onchain effect of a successful challenge: game is resolved.
    verifier.update_game(
        addr(0),
        MockGameState { status: GameStatus::ChallengerWins, ..game_state(20) },
    );

    // Step 2: proof polled → Succeeded → challenge tx submitted → entry removed.
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "entry should be removed after successful ZK challenge submission"
    );
}

#[tokio::test]
async fn test_step_invalid_game_tee_proof_succeeds() {
    // TEE proof succeeds → submitted directly without ZK.
    let (l2, factory, root_15, root_20) = base_game_mocks();

    let verifier = single_game_verifier(MockGameState {
        tee_prover: DEFAULT_TEE_PROVER,
        // root_15 is correct, index 1 is bogus — invalid_index == 1
        intermediate_output_roots: vec![root_15, BOGUS_ROOT],
        ..game_state(20)
    });

    let l1_head = Arc::new(MockL1HeadProvider::success(DEFAULT_L1_HEAD, 100));

    let aggregate_proposal = Proposal {
        output_root: root_20,
        signature: Bytes::from(vec![0u8; 65]),
        l1_origin_hash: DEFAULT_L1_HEAD,
        l1_origin_number: 1000,
        l2_block_number: 20,
        prev_output_root: root_15,
        config_hash: B256::ZERO,
    };
    let proof_requester = Arc::new(MockZkProofProvider {
        session_id: String::new(),
        state: Mutex::new(MockZkProofState {
            proof_status: ProofStatus::Succeeded,
            result: Some(tee_api_result(aggregate_proposal)),
            ..Default::default()
        }),
    });

    let tx_manager = default_tx_manager();

    let mut driver = test_driver_with_tee(
        factory,
        Arc::clone(&verifier),
        l2,
        proof_requester,
        tx_manager,
        Some(tee_config(l1_head)),
    );

    driver.step().await.unwrap();
    verifier.update_game(
        addr(0),
        MockGameState {
            tee_prover: Address::ZERO,
            intermediate_output_roots: vec![root_15, BOGUS_ROOT],
            ..game_state(20)
        },
    );
    driver.step().await.unwrap();

    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "no pending proof should remain after TEE nullification is observed"
    );
}

#[tokio::test]
async fn test_step_tee_contract_revert_falls_back_to_zk() {
    // TEE proof succeeds but the onchain nullify() call reverts.
    // Contract-level failures are proof-specific enough to fall back to ZK.
    let (l2, factory, root_15, root_20) = base_game_mocks();

    let verifier = single_game_verifier(MockGameState {
        tee_prover: DEFAULT_TEE_PROVER,
        // root_15 is correct, index 1 is bogus — invalid_index == 1
        intermediate_output_roots: vec![root_15, BOGUS_ROOT],
        ..game_state(20)
    });

    let l1_head = Arc::new(MockL1HeadProvider::success(DEFAULT_L1_HEAD, 100));

    let aggregate_proposal = Proposal {
        output_root: root_20,
        signature: Bytes::from(vec![0u8; 65]),
        l1_origin_hash: DEFAULT_L1_HEAD,
        l1_origin_number: 1000,
        l2_block_number: 20,
        prev_output_root: root_15,
        config_hash: B256::ZERO,
    };
    let zk = Arc::new(MockZkProofProvider {
        session_id: String::new(),
        state: Mutex::new(MockZkProofState {
            proof_status: ProofStatus::Succeeded,
            result: Some(tee_api_result(aggregate_proposal)),
            ..Default::default()
        }),
    });

    // TEE nullify() tx reverts, ZK challenge() tx succeeds.
    let tx_manager = MockTxManager::with_responses(vec![
        Err(TxManagerError::ExecutionReverted {
            reason: Some("unexpected contract revert".to_string()),
            data: None,
        }),
        Ok(receipt_with_status(true, DEFAULT_TX_HASH)),
    ]);

    let mut driver = test_driver_with_tee(
        Arc::clone(&factory),
        Arc::clone(&verifier),
        l2,
        zk,
        tx_manager,
        Some(tee_config(l1_head)),
    );

    // Step 1: TEE proof job is initiated.
    driver.step().await.unwrap();

    // Step 2: TEE proof is polled, nullify() tx fails, falls back to ZK.
    driver.step().await.unwrap();

    // The entry should now be a ZK proof in AwaitingProof phase (ZK fallback).
    let entry = driver
        .pending_proofs
        .get(&addr(0))
        .expect("ZK fallback proof should be pending after TEE tx failure");
    assert!(
        matches!(entry.phase, ProofPhase::AwaitingProof { .. }),
        "phase should be AwaitingProof (ZK fallback) after TEE tx failure"
    );
    assert_eq!(
        entry.intent,
        DisputeIntent::Challenge,
        "ZK fallback should use Challenge intent for Path 1"
    );
    assert!(
        matches!(entry.kind, base_challenger::ProofKind::Zk { .. }),
        "kind should have transitioned from Tee to Zk after fallback"
    );

    {
        let mut state = driver.proof_requester.state.lock().unwrap();
        state.result = None;
        state.proof = vec![0xDE, 0xAD];
        state.proof_status = ProofStatus::Succeeded;
    }

    // Keep the game in-progress so the pending fallback proof reaches
    // submit_dispute(), but remove it from the scan batch so the same tick
    // cannot re-discover it after the successful fallback submission.
    factory.games.lock().unwrap().clear();

    // Step 3: ZK proof polled → Succeeded → challenge tx submitted → entry cleaned up.
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "entry should be removed after ZK fallback completes"
    );
}

#[tokio::test]
async fn test_step_invalid_tee_result_falls_back_to_zk_without_timeout() {
    let (l2, factory, root_15, _root_20) = base_game_mocks();

    let verifier = single_game_verifier(MockGameState {
        tee_prover: DEFAULT_TEE_PROVER,
        intermediate_output_roots: vec![root_15, BOGUS_ROOT],
        ..game_state(20)
    });

    let l1_head = Arc::new(MockL1HeadProvider::success(DEFAULT_L1_HEAD, 100));

    let bad_aggregate_proposal = Proposal {
        output_root: B256::repeat_byte(0x42),
        signature: Bytes::from(vec![0u8; 65]),
        l1_origin_hash: DEFAULT_L1_HEAD,
        l1_origin_number: 1000,
        l2_block_number: 20,
        prev_output_root: root_15,
        config_hash: B256::ZERO,
    };
    let proof_requester = Arc::new(MockZkProofProvider {
        session_id: String::new(),
        state: Mutex::new(MockZkProofState {
            proof_status: ProofStatus::Succeeded,
            result: Some(tee_api_result(bad_aggregate_proposal)),
            ..Default::default()
        }),
    });

    let mut driver = test_driver_with_tee(
        factory,
        verifier,
        l2,
        proof_requester,
        default_tx_manager(),
        Some(tee_config(l1_head)),
    );

    // Step 1: TEE proof job is initiated.
    driver.step().await.unwrap();

    // Step 2: completed TEE proof has the wrong root, so the driver should
    // immediately transition to ZK fallback instead of waiting for timeout.
    driver.step().await.unwrap();

    let entry = driver
        .pending_proofs
        .get(&addr(0))
        .expect("ZK fallback proof should be pending after invalid TEE result");
    assert!(matches!(entry.kind, base_challenger::ProofKind::Zk { .. }));
    assert!(matches!(entry.phase, ProofPhase::AwaitingProof { .. }));
}

#[tokio::test]
async fn test_step_nullified_game_not_reprocessed() {
    // Both provers zeroed (post-nullification) → scanner filters it out.
    let (l2, factory, root_15, _root_20) = base_game_mocks();

    let verifier = single_game_verifier(MockGameState {
        intermediate_output_roots: vec![root_15, BOGUS_ROOT],
        ..game_state(20)
    });
    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());

    // Run two steps — the game should be filtered by the scanner on both.
    driver.step().await.unwrap();
    driver.step().await.unwrap();

    assert!(driver.pending_proofs.is_empty(), "no proofs should be pending for a nullified game");
}

// ──────────────────────────────────────────────────────────────────────────
// Path 2: Correct TEE proof challenged with wrong ZK proof → nullify ZK
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_poll_or_submit_nullify_intent_not_dropped_when_zk_prover_set() {
    // A pending proof with DisputeIntent::Nullify should NOT be dropped
    // when zkProver is non-zero (unlike DisputeIntent::Challenge, which
    // requires zkProver == ZERO).
    let factory = single_game_factory();
    let l2 = default_l2();
    let mut game_state =
        mock_state_with_tee(GameStatus::InProgress, ZK_PROVER_ADDR, DEFAULT_TEE_PROVER, 20);
    game_state.countered_index = 2; // challenged at 0-based index 1
    // Provide intermediate roots so the scanner's FraudulentZkChallenge
    // processing (which runs after the pending proof is submitted) does not
    // panic when fetching the root at the challenged index.
    game_state.intermediate_output_roots = vec![B256::repeat_byte(0x01), B256::repeat_byte(0x02)];
    let verifier = single_game_verifier(game_state);

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());
    driver.pending_proofs.insert(addr(0), default_ready_proof(DisputeIntent::Nullify));

    driver.step().await.unwrap();

    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "nullify intent should be submitted, not dropped due to zk_prover"
    );
}

#[tokio::test]
async fn test_successful_nullify_does_not_track_anchor_update() {
    let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
    let l2 = default_l2();
    let verifier = single_game_verifier(mock_state_with_tee(
        GameStatus::InProgress,
        ZK_PROVER_ADDR,
        DEFAULT_TEE_PROVER,
        20,
    ));

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());
    driver.pending_proofs.insert(
        addr(0),
        PendingProof {
            phase: ProofPhase::ReadyToSubmit { proof_bytes: Bytes::from_static(&[0x00, 0xAA]) },
            kind: ProofKind::Tee { zk_fallback_request: None, zk_fallback_intent: None },
            invalid_index: 1,
            expected_root: BOGUS_ROOT,
            retry_count: 0,
            tee_submit_retry_count: 0,
            intent: DisputeIntent::Nullify,
        },
    );

    driver.step().await.unwrap();

    assert!(!driver.pending_proofs.contains_key(&addr(0)));
}

#[tokio::test]
async fn test_poll_or_submit_challenge_intent_dropped_when_zk_prover_set() {
    // A pending proof with DisputeIntent::Challenge should be dropped
    // when zkProver is non-zero (game already challenged).
    let factory = single_game_factory();
    let l2 = default_l2();
    let verifier = single_game_verifier(mock_state_with_tee(
        GameStatus::InProgress,
        ZK_PROVER_ADDR,
        DEFAULT_TEE_PROVER,
        20,
    ));

    let tx = MockTxManager::new(Err(TxManagerError::NonceTooLow)); // Should never be called
    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), tx);
    driver.pending_proofs.insert(addr(0), default_ready_proof(DisputeIntent::Challenge));

    driver.step().await.unwrap();

    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "challenge intent should be dropped when game is already challenged"
    );
}

/// Builds mocks for a Path 2 (`FraudulentZkChallenge`) scenario.
///
/// The game at `addr(0)` has both TEE and ZK provers set with
/// `countered_index = 2` (1-based), meaning the challenged intermediate
/// root is at 0-based index 1 (block 20).
///
/// Layout: starting=10, `l2_block=20`, interval=5, checkpoints at 15 and 20.
/// `correct_root_at_20` controls whether the onchain root at index 1
/// (block 20) matches the L2-computed root:
/// - `true`: onchain root is correct → ZK challenge was fraudulent → nullify.
/// - `false`: onchain root is bogus → ZK challenge was legitimate → skip.
fn fraudulent_zk_challenge_mocks(
    correct_root_at_20: bool,
) -> (Arc<MockL2Provider>, Arc<MockDisputeGameFactory>, Arc<MockAggregateVerifier>) {
    let (l2, factory, root_15, root_20) = base_game_mocks();
    let onchain_root_at_20 = if correct_root_at_20 { root_20 } else { BOGUS_ROOT };

    let verifier = single_game_verifier(MockGameState {
        zk_prover: ZK_PROVER_ADDR,
        tee_prover: DEFAULT_TEE_PROVER,
        intermediate_output_roots: vec![root_15, onchain_root_at_20],
        countered_index: 2, // 1-based → challenged_index = 1
        ..game_state(20)
    });

    (l2, factory, verifier)
}

#[tokio::test]
async fn test_step_fraudulent_zk_challenge_legitimate_skips() {
    // The onchain root at the challenged index is wrong, meaning the ZK
    // challenge was legitimate. The driver should skip without initiating
    // a proof.
    let (l2, factory, verifier) = fraudulent_zk_challenge_mocks(false);

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());
    driver.step().await.unwrap();

    assert!(
        driver.pending_proofs.is_empty(),
        "no proof should be initiated when the ZK challenge is legitimate"
    );
}

#[tokio::test]
async fn test_step_fraudulent_zk_challenge_nullifies() {
    // The onchain root at the challenged index is correct, meaning the
    // ZK challenge was fraudulent. The driver should initiate a ZK proof
    // with DisputeIntent::Nullify.
    let (l2, factory, verifier) = fraudulent_zk_challenge_mocks(true);

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());
    driver.step().await.unwrap();

    let entry = driver
        .pending_proofs
        .get(&addr(0))
        .expect("proof should be pending for fraudulent ZK challenge");
    assert_eq!(
        entry.intent,
        DisputeIntent::Nullify,
        "intent should be Nullify for fraudulent ZK challenge"
    );
}

#[tokio::test]
async fn test_step_fraudulent_zk_challenge_nullifies_despite_earlier_invalid_root() {
    // Regression: an earlier intermediate root (index 0) is invalid, but the
    // challenged root (index 1) is correct. The ZK challenge targets a valid
    // root, so it is fraudulent and must be nullified. Previously the
    // challenger incorrectly skipped because the first invalid index was
    // <= the challenged index.
    let (l2, factory, _root_15, root_20) = base_game_mocks();

    let verifier = single_game_verifier(MockGameState {
        zk_prover: ZK_PROVER_ADDR,
        tee_prover: DEFAULT_TEE_PROVER,
        // Index 0 is bogus, index 1 (challenged) is correct.
        intermediate_output_roots: vec![BOGUS_ROOT, root_20],
        countered_index: 2, // 1-based → challenged_index = 1
        ..game_state(20)
    });

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());
    driver.step().await.unwrap();

    let entry = driver
        .pending_proofs
        .get(&addr(0))
        .expect("proof should be pending — challenged root is valid, ZK challenge is fraudulent");
    assert_eq!(
        entry.intent,
        DisputeIntent::Nullify,
        "intent should be Nullify when challenged root is valid despite earlier invalid root"
    );
}

// ──────────────────────────────────────────────────────────────────────────
// Path 3: Wrong ZK proposal → nullify with ZK
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_step_invalid_zk_proposal_initiates_zk_nullification() {
    // A game proposed with a ZK proof (tee_prover == ZERO, zk_prover != ZERO)
    // with invalid intermediate roots should trigger a ZK proof with
    // DisputeIntent::Nullify.
    let (l2, factory, root_15, _root_20) = base_game_mocks();

    let verifier = single_game_verifier(MockGameState {
        zk_prover: ZK_PROVER_ADDR,
        intermediate_output_roots: vec![root_15, BOGUS_ROOT],
        ..game_state(20)
    });

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());
    driver.step().await.unwrap();

    let entry =
        driver.pending_proofs.get(&addr(0)).expect("ZK nullification proof should be pending");
    assert_eq!(entry.intent, DisputeIntent::Nullify, "intent should be Nullify for ZK proposals");
}

#[tokio::test]
async fn test_step_valid_zk_proposal_skipped() {
    // A ZK-proposed game with valid intermediate roots should not trigger
    // any action.
    let factory = single_game_factory();
    let verifier =
        single_game_verifier(MockGameState { zk_prover: ZK_PROVER_ADDR, ..game_state(14) });
    let l2 = default_l2();

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());
    driver.step().await.unwrap();

    assert!(driver.pending_proofs.is_empty(), "valid ZK proposal should not trigger any proof");
}

// ──────────────────────────────────────────────────────────────────────────
// Dual-proof games: both TEE and ZK proofs verified (no challenge)
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_step_dual_proof_invalid_without_tee_config_falls_back_to_zk_nullify() {
    // A game with both TEE and ZK proofs verified (via verifyProposalProof,
    // not challenge) where the output roots are invalid and no TEE config is
    // available should fall back to a ZK proof with DisputeIntent::Nullify.
    let (l2, factory, root_15, _root_20) = base_game_mocks();

    let verifier = single_game_verifier(MockGameState {
        tee_prover: DEFAULT_TEE_PROVER,
        zk_prover: ZK_PROVER_ADDR,
        intermediate_output_roots: vec![root_15, BOGUS_ROOT],
        ..game_state(20)
    });

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());
    driver.step().await.unwrap();

    let entry = driver
        .pending_proofs
        .get(&addr(0))
        .expect("ZK nullification proof should be pending for dual-proof game");
    assert_eq!(
        entry.intent,
        DisputeIntent::Nullify,
        "dual-proof game without TEE config should fall back to ZK Nullify"
    );
}

#[tokio::test]
async fn test_step_dual_proof_invalid_with_tee_config_nullifies_tee_first() {
    // A game with both TEE and ZK proofs verified where output roots are
    // invalid and a TEE config is available should attempt TEE nullification
    // first (fast path). After TEE nullification the game will be rescanned
    // as InvalidZkProposal on the next tick.
    let (l2, factory, root_15, root_20) = base_game_mocks();

    let verifier = single_game_verifier(MockGameState {
        tee_prover: DEFAULT_TEE_PROVER,
        zk_prover: ZK_PROVER_ADDR,
        intermediate_output_roots: vec![root_15, BOGUS_ROOT],
        ..game_state(20)
    });

    let l1_head = Arc::new(MockL1HeadProvider::success(DEFAULT_L1_HEAD, 100));

    let aggregate_proposal = Proposal {
        output_root: root_20,
        signature: Bytes::from(vec![0u8; 65]),
        l1_origin_hash: DEFAULT_L1_HEAD,
        l1_origin_number: 1000,
        l2_block_number: 20,
        prev_output_root: root_15,
        config_hash: B256::ZERO,
    };
    let proof_requester = Arc::new(MockZkProofProvider {
        session_id: String::new(),
        state: Mutex::new(MockZkProofState {
            proof_status: ProofStatus::Succeeded,
            result: Some(tee_api_result(aggregate_proposal)),
            ..Default::default()
        }),
    });

    let tx_manager = default_tx_manager();

    let mut driver = test_driver_with_tee(
        factory,
        Arc::clone(&verifier),
        l2,
        proof_requester,
        tx_manager,
        Some(tee_config(l1_head)),
    );

    driver.step().await.unwrap();
    verifier.update_game(
        addr(0),
        MockGameState {
            tee_prover: Address::ZERO,
            zk_prover: ZK_PROVER_ADDR,
            intermediate_output_roots: vec![root_15, BOGUS_ROOT],
            ..game_state(20)
        },
    );
    driver.step().await.unwrap();

    // After TEE nullification is observed, the same game is re-scanned as an
    // invalid ZK proposal and a ZK nullification proof is initiated.
    let entry = driver
        .pending_proofs
        .get(&addr(0))
        .expect("ZK proof should be pending after TEE nullification");
    assert!(matches!(entry.kind, base_challenger::ProofKind::Zk { .. }));
    assert_eq!(entry.intent, DisputeIntent::Nullify);
}

#[tokio::test]
async fn test_step_dual_proof_valid_skipped() {
    // A game with both TEE and ZK proofs verified where output roots are
    // valid should not trigger any action.
    let factory = single_game_factory();
    let verifier = single_game_verifier(MockGameState {
        tee_prover: DEFAULT_TEE_PROVER,
        zk_prover: ZK_PROVER_ADDR,
        ..game_state(14)
    });
    let l2 = default_l2();

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());
    driver.step().await.unwrap();

    assert!(driver.pending_proofs.is_empty(), "valid dual-proof game should not trigger any proof");
}

#[tokio::test]
async fn test_step_dual_proof_tee_fails_falls_back_to_zk_nullify() {
    // A dual-proof game where the TEE proof fails should fall back to ZK
    // with DisputeIntent::Nullify (not Challenge).
    let (l2, factory, root_15, _root_20) = base_game_mocks();

    let verifier = single_game_verifier(MockGameState {
        tee_prover: DEFAULT_TEE_PROVER,
        zk_prover: ZK_PROVER_ADDR,
        intermediate_output_roots: vec![root_15, BOGUS_ROOT],
        ..game_state(20)
    });

    let mut driver = test_driver_with_tee(
        factory,
        verifier,
        l2,
        default_zk_prover(),
        default_tx_manager(),
        Some(tee_config(Arc::new(MockL1HeadProvider::failure("dummy")))),
    );

    driver.step().await.unwrap();

    let entry = driver
        .pending_proofs
        .get(&addr(0))
        .expect("ZK proof should be pending after TEE fallback for dual-proof game");
    assert_eq!(
        entry.intent,
        DisputeIntent::Nullify,
        "dual-proof TEE fallback must use Nullify intent, not Challenge"
    );
}

// ──────────────────────────────────────────────────────────────────────────
// Bond lifecycle integration tests
// ──────────────────────────────────────────────────────────────────────────

const fn bond_test_state(claim_addr: Address) -> MockGameState {
    let mut state = mock_state(GameStatus::ChallengerWins, Address::ZERO, 100);
    state.bond_recipient = claim_addr;
    state
}

fn bond_test_verifier(claim_addr: Address) -> Arc<MockAggregateVerifier> {
    single_game_verifier(bond_test_state(claim_addr))
}

fn default_bond_manager(claim_addr: Address) -> BondManager<TokioRuntime> {
    let mut mgr = BondManager::new(
        vec![claim_addr],
        "http://localhost:8545".parse().unwrap(),
        empty_factory(),
        1000,
        TEST_DISCOVERY_INTERVAL,
        TokioRuntime::new(),
    );
    mgr.set_weth_delay(Duration::from_secs(0));
    mgr
}

#[tokio::test]
async fn test_bond_manager_full_lifecycle() {
    // Verify the full bond lifecycle: NeedsResolve → NeedsUnlock →
    // AwaitingDelay → NeedsWithdraw → Completed.
    //
    // The mock verifier uses a static game state, so we set
    // ChallengerWins to represent a game that has already been
    // resolved onchain. The manager detects this and advances directly
    // to NeedsUnlock without submitting a resolve transaction.
    let claim_addr = ZK_PROVER_ADDR;
    let game_addr = addr(0);
    let tx_hash = DEFAULT_TX_HASH;
    let verifier = bond_test_verifier(claim_addr);

    let submitter = MockBondTransactionSubmitter::with_responses(vec![
        Ok(tx_hash), // claimCredit (unlock) tx
        Ok(tx_hash), // claimCredit (withdraw) tx
    ]);

    let mut mgr = default_bond_manager(claim_addr);

    assert!(mgr.track_game(game_addr, claim_addr));
    assert_eq!(mgr.tracked_count(), 1);

    // Poll 1: NeedsResolve → already resolved (ChallengerWins) → NeedsUnlock.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 1, "game should still be tracked after detecting resolution");

    // Poll 2: NeedsUnlock → claimCredit (unlock) tx → AwaitingDelay.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 1, "game should still be tracked during delay");

    // Poll 3: AwaitingDelay (delay=0s, already elapsed) → NeedsWithdraw.
    // check_delay transitions to NeedsWithdraw, but advance_game returns
    // Ok(false), so the game is still tracked. Need one more poll.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 1, "game should still be tracked after delay");

    // Poll 4: NeedsWithdraw → claimCredit (withdraw) tx → Completed → removed.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 0, "game should be removed after completion");

    // Verify 2 transactions were submitted (unlock + withdraw, no resolve).
    let calls = submitter.recorded_calls();
    assert_eq!(calls.len(), 2, "expected 2 bond transactions (unlock, withdraw)");
    for (game, target, _) in &calls {
        assert_eq!(*game, game_addr, "all transactions should reference the game address");
        assert_eq!(*target, game_addr, "all transactions should target the game address");
    }
}

#[tokio::test]
async fn test_bond_manager_skips_already_unlocked_game() {
    let claim_addr = ZK_PROVER_ADDR;
    let game_addr = addr(0);
    let tx_hash = DEFAULT_TX_HASH;

    let mut state = bond_test_state(claim_addr);
    state.bond_unlocked = true;
    state.resolved_at = 1_000_000;
    let verifier = single_game_verifier(state);

    let submitter = MockBondTransactionSubmitter::with_responses(vec![
        Ok(tx_hash), // withdraw
    ]);

    let mut mgr = default_bond_manager(claim_addr);
    mgr.track_game(game_addr, claim_addr);

    // Poll 1: NeedsResolve → status != 0 → NeedsUnlock (no tx).
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 1);

    // Poll 2: NeedsUnlock -> bond_unlocked=true and delay=0 -> NeedsWithdraw (no tx).
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 1);

    // Poll 3: NeedsWithdraw -> submit withdraw -> Completed -> removed.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 0);

    assert_eq!(submitter.recorded_calls().len(), 1);
}

#[tokio::test]
async fn test_bond_manager_skips_already_claimed_game() {
    let claim_addr = ZK_PROVER_ADDR;
    let game_addr = addr(0);

    let mut state = bond_test_state(claim_addr);
    state.bond_unlocked = true;
    state.bond_claimed = true;
    state.resolved_at = 1_000_000;
    let verifier = single_game_verifier(state);

    let submitter = MockBondTransactionSubmitter::with_responses(vec![]);

    let mut mgr = default_bond_manager(claim_addr);
    mgr.track_game(game_addr, claim_addr);

    // Polls 1-3: NeedsResolve → NeedsUnlock → AwaitingDelay → NeedsWithdraw (no txs).
    for _ in 0..3 {
        mgr.poll(&*verifier, &submitter).await;
    }

    // Poll 4: NeedsWithdraw → bond_claimed=true → Completed → removed.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 0);

    assert!(
        submitter.recorded_calls().is_empty(),
        "no transactions should be submitted for already-claimed bond"
    );
}

#[tokio::test]
async fn test_bond_manager_tx_failure_retries() {
    let claim_addr = ZK_PROVER_ADDR;
    let game_addr = addr(0);
    let tx_hash = DEFAULT_TX_HASH;
    let verifier = bond_test_verifier(claim_addr);

    let submitter = MockBondTransactionSubmitter::with_responses(vec![
        Err(base_tx_manager::TxManagerError::NonceTooLow.into()),
        Ok(tx_hash), // retry succeeds
    ]);

    let mut mgr = default_bond_manager(claim_addr);
    mgr.track_game(game_addr, claim_addr);

    // Poll 1: NeedsResolve → already resolved (ChallengerWins) → NeedsUnlock.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 1, "game should still be tracked after detecting resolution");

    // Poll 2: NeedsUnlock → claimCredit tx fails → stays NeedsUnlock.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 1, "game should still be tracked after tx failure");

    // Poll 3: NeedsUnlock → retry → claimCredit tx succeeds → AwaitingDelay.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 1, "game should still be tracked after unlock");

    assert_eq!(submitter.recorded_calls().len(), 2, "expected 2 claimCredit attempts");
}

#[tokio::test]
async fn test_bond_manager_ignores_non_claim_addresses() {
    let claim_addr = ZK_PROVER_ADDR;
    let other_addr = Address::repeat_byte(0xDD);
    let game_addr = addr(0);

    let mut mgr = default_bond_manager(claim_addr);
    assert!(!mgr.track_game(game_addr, other_addr));
    assert_eq!(mgr.tracked_count(), 0);
}

#[tokio::test]
async fn test_bond_manager_keeps_defender_wins_when_recipient_is_claimable() {
    // DEFENDER_WINS but bondRecipient is ours → keep and advance to NeedsUnlock.
    let claim_addr = ZK_PROVER_ADDR;
    let game_addr = addr(0);

    let mut state = bond_test_state(claim_addr);
    state.status = GameStatus::DefenderWins;
    let verifier = single_game_verifier(state);

    let submitter = MockBondTransactionSubmitter::with_responses(vec![]);

    let mut mgr = default_bond_manager(claim_addr);
    mgr.track_game(game_addr, claim_addr);
    assert_eq!(mgr.tracked_count(), 1);

    // Poll 1: NeedsResolve → resolved, bondRecipient in claim set → NeedsUnlock.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(
        mgr.tracked_count(),
        1,
        "game should be kept when bondRecipient is in claim addresses"
    );
}

#[tokio::test]
async fn test_bond_manager_removes_game_when_recipient_not_claimable() {
    // bondRecipient not in claim set → removed from tracking.
    let claim_addr = ZK_PROVER_ADDR;
    let other_addr = Address::repeat_byte(0xDD);
    let game_addr = addr(0);

    let mut state = bond_test_state(claim_addr);
    state.status = GameStatus::DefenderWins;
    state.bond_recipient = other_addr; // bond goes to someone else
    let verifier = single_game_verifier(state);

    let submitter = MockBondTransactionSubmitter::with_responses(vec![]);

    let mut mgr = default_bond_manager(claim_addr);
    mgr.track_game(game_addr, claim_addr);
    assert_eq!(mgr.tracked_count(), 1);

    // Poll 1: NeedsResolve → resolved, bondRecipient not in claim set → removed.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(
        mgr.tracked_count(),
        0,
        "game should be removed when bondRecipient is not in claim addresses"
    );

    let calls = submitter.recorded_calls();
    assert!(calls.is_empty(), "bond manager should not submit anchor update txs");
}

#[tokio::test]
async fn test_driver_tracks_bond_after_successful_challenge() {
    let (l2, factory, verifier) = invalid_game_mocks();
    let sender_addr = Address::ZERO; // MockTxManager returns ZERO as sender_address

    let zk = succeeded_zk_prover("bond-track", vec![0xDE, 0xAD]);

    let tx_manager = default_tx_manager();

    let mut bond_manager = default_bond_manager(sender_addr);
    bond_manager.set_weth_delay(Duration::from_secs(3600));

    let mut driver = test_driver(factory, verifier, l2, zk, tx_manager);
    driver.bond_manager = Some(bond_manager);

    // Step 1: proof initiated, not yet polled.
    driver.step().await.unwrap();
    assert!(
        driver.pending_proofs.contains_key(&addr(0)),
        "proof should be pending after initiation"
    );

    // Step 2: proof polled → Succeeded → challenge tx submitted → bond tracked.
    driver.step().await.unwrap();

    let bond_mgr = driver.bond_manager.as_ref().expect("bond_manager should be Some");
    assert!(
        bond_mgr.is_tracking(&addr(0)),
        "game should be tracked by bond manager after successful challenge"
    );
}

// ──────────────────────────────────────────────────────────────────────────
// Checkpoint count mismatch (stale interval) surfaces as an error
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_step_checkpoint_count_mismatch_surfaces_error() {
    // Regression test for Immunefi #74652: after a governance
    // `setImplementation` that changes `INTERMEDIATE_BLOCK_INTERVAL`,
    // the cached stale interval produces a `CheckpointCountMismatch`
    // error. This error must NOT be silently swallowed — it should
    // propagate as a per-game error so operators notice.
    //
    // Setup: starting=10, l2_block=20, span=10.
    // On-chain impl uses interval=10 → stores 1 intermediate root.
    // But we configure the mock so the scanner resolves interval=5
    // (the default mock value), which expects 2 intermediate roots.
    // This triggers CheckpointCountMismatch { expected: 2, actual: 1 }.
    let factory = single_game_factory();

    let verifier = single_game_verifier(MockGameState {
        tee_prover: DEFAULT_TEE_PROVER,
        // On-chain: only 1 intermediate root (as if interval=10).
        intermediate_output_roots: vec![BOGUS_ROOT],
        ..game_state(20)
    });

    // The mock verifier's `read_intermediate_block_interval` returns 5,
    // so the scanner caches interval=5. With span=10, the validator
    // expects 10/5 = 2 intermediate roots, but only 1 is onchain.
    let l2 = default_l2();

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());

    // step() should succeed (per-game errors are caught at the candidate
    // level), but the game should NOT have a pending proof — the mismatch
    // error prevents silent skipping and instead logs an error.
    driver.step().await.unwrap();

    assert!(
        driver.pending_proofs.is_empty(),
        "no proof should be initiated when checkpoint count mismatches — \
         the error should be surfaced, not silently swallowed"
    );
}

const fn minimal_prove_request() -> SnarkGroth16ProofRequest {
    SnarkGroth16ProofRequest {
        proof: ZkProofRequest {
            start_block_number: 0,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            l1_head: None,
            intermediate_root_interval: None,
            zk_vm: ZkVm::Sp1,
        },
        prover_address: Address::ZERO,
    }
}

#[tokio::test]
async fn test_poll_failed_status_triggers_retry() {
    let zk = Arc::new(MockZkProofProvider {
        session_id: "failed-session".to_string(),
        state: Mutex::new(MockZkProofState {
            proof_status: ProofStatus::Failed,
            ..Default::default()
        }),
    });

    let mut proofs = PendingProofs::new();
    proofs.insert(
        addr(0),
        PendingProof::awaiting(
            "failed-session".to_string(),
            0,
            B256::ZERO,
            minimal_prove_request(),
            DisputeIntent::Challenge,
        ),
    );

    let update = proofs.poll(addr(0), &*zk, Duration::from_secs(3600)).await.unwrap();
    assert!(matches!(update, Some(ProofUpdate::NeedsRetry)), "Failed should trigger NeedsRetry");
    let entry = proofs.get(&addr(0)).unwrap();
    assert_eq!(entry.retry_count, 1);
    assert!(matches!(entry.phase, ProofPhase::NeedsRetry));
}

#[tokio::test]
async fn test_poll_running_within_timeout_stays_pending() {
    let zk = Arc::new(MockZkProofProvider {
        session_id: "running-session".to_string(),
        state: Mutex::new(MockZkProofState {
            proof_status: ProofStatus::Running,
            ..Default::default()
        }),
    });

    let mut proofs = PendingProofs::new();
    proofs.insert(
        addr(0),
        PendingProof::awaiting(
            "running-session".to_string(),
            0,
            B256::ZERO,
            minimal_prove_request(),
            DisputeIntent::Challenge,
        ),
    );

    let update = proofs.poll(addr(0), &*zk, Duration::from_secs(3600)).await.unwrap();
    assert!(
        matches!(update, Some(ProofUpdate::Pending)),
        "Running within timeout should stay Pending"
    );
    let entry = proofs.get(&addr(0)).unwrap();
    assert_eq!(entry.retry_count, 0);
    assert!(matches!(entry.phase, ProofPhase::AwaitingProof { .. }));
}

#[tokio::test]
async fn test_poll_running_timeout_triggers_retry() {
    let zk = Arc::new(MockZkProofProvider {
        session_id: "stuck-session".to_string(),
        state: Mutex::new(MockZkProofState {
            proof_status: ProofStatus::Running,
            ..Default::default()
        }),
    });

    let mut proofs = PendingProofs::new();
    proofs.insert(
        addr(0),
        PendingProof::awaiting(
            "stuck-session".to_string(),
            0,
            B256::ZERO,
            minimal_prove_request(),
            DisputeIntent::Challenge,
        ),
    );

    // Zero timeout: already expired on the first poll.
    let update = proofs.poll(addr(0), &*zk, Duration::ZERO).await.unwrap();
    assert!(
        matches!(update, Some(ProofUpdate::NeedsRetry)),
        "Timed-out Running should trigger NeedsRetry"
    );
    let entry = proofs.get(&addr(0)).unwrap();
    assert_eq!(entry.retry_count, 1);
    assert!(matches!(entry.phase, ProofPhase::NeedsRetry));
}

// Metric emission tests for `proof_session_failures_total{reason}` and
// `proof_retries_exhausted_total`. Each test installs a local
// `DebuggingRecorder` and asserts on the resulting snapshot.
#[cfg(feature = "metrics")]
mod metrics_emission {
    use base_challenger::ChallengerMetrics;
    use metrics_util::{
        CompositeKey, MetricKind,
        debugging::{DebugValue, DebuggingRecorder, Snapshotter},
    };

    use super::*;

    type SnapEntry =
        (CompositeKey, Option<metrics::Unit>, Option<metrics::SharedString>, DebugValue);

    /// Installs a local `DebuggingRecorder` for the closure and snapshots its
    /// metrics afterward.
    fn with_recorder<F>(f: F)
    where
        F: FnOnce(Snapshotter),
    {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || f(snapshotter));
    }

    /// Returns the value of `proof_session_failures_total` whose `reason`
    /// label matches `expected_reason`, if present in the snapshot.
    fn find_failure_counter_with_reason(snap: &[SnapEntry], expected_reason: &str) -> Option<u64> {
        snap.iter().find_map(|(ck, _, _, v)| {
            if ck.kind() != MetricKind::Counter
                || ck.key().name() != "base_challenger.proof_session_failures_total"
            {
                return None;
            }
            let reason_match =
                ck.key().labels().any(|l| l.key() == "reason" && l.value() == expected_reason);
            if !reason_match {
                return None;
            }
            match v {
                DebugValue::Counter(n) => Some(*n),
                _ => None,
            }
        })
    }

    /// Returns the value of a bare counter by name, if present.
    fn find_counter(snap: &[SnapEntry], name: &str) -> Option<u64> {
        snap.iter().find_map(|(ck, _, _, v)| {
            if ck.kind() != MetricKind::Counter || ck.key().name() != name {
                return None;
            }
            match v {
                DebugValue::Counter(n) => Some(*n),
                _ => None,
            }
        })
    }

    #[test]
    fn test_poll_timeout_emits_failure_metric() {
        with_recorder(|snap| {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

            rt.block_on(async {
                let zk = Arc::new(MockZkProofProvider {
                    session_id: "stuck-session".to_string(),
                    state: Mutex::new(MockZkProofState {
                        proof_status: ProofStatus::Running,
                        ..Default::default()
                    }),
                });

                let mut proofs = PendingProofs::new();
                proofs.insert(
                    addr(0),
                    PendingProof::awaiting(
                        "stuck-session".to_string(),
                        0,
                        B256::ZERO,
                        minimal_prove_request(),
                        DisputeIntent::Challenge,
                    ),
                );

                proofs.poll(addr(0), &*zk, Duration::ZERO).await.unwrap();
            });

            let snapshot = snap.snapshot().into_vec();
            assert_eq!(
                find_failure_counter_with_reason(
                    &snapshot,
                    ChallengerMetrics::PROOF_FAILURE_TIMEOUT,
                ),
                Some(1),
                "timeout path must increment proof_session_failures_total{{reason=timeout}}",
            );
        });
    }

    #[test]
    fn test_poll_failed_status_emits_failure_metric() {
        with_recorder(|snap| {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

            rt.block_on(async {
                let zk = Arc::new(MockZkProofProvider {
                    session_id: "failed-session".to_string(),
                    state: Mutex::new(MockZkProofState {
                        proof_status: ProofStatus::Failed,
                        ..Default::default()
                    }),
                });

                let mut proofs = PendingProofs::new();
                proofs.insert(
                    addr(0),
                    PendingProof::awaiting(
                        "failed-session".to_string(),
                        0,
                        B256::ZERO,
                        minimal_prove_request(),
                        DisputeIntent::Challenge,
                    ),
                );

                proofs.poll(addr(0), &*zk, Duration::from_secs(3600)).await.unwrap();
            });

            let snapshot = snap.snapshot().into_vec();
            assert_eq!(
                find_failure_counter_with_reason(
                    &snapshot,
                    ChallengerMetrics::PROOF_FAILURE_FAILED,
                ),
                Some(1),
                "explicit Failed status must increment \
                 proof_session_failures_total{{reason=failed}}",
            );
        });
    }

    #[test]
    fn test_poll_succeeded_without_result_emits_malformed_metric() {
        with_recorder(|snap| {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

            rt.block_on(async {
                let zk = Arc::new(MockZkProofProvider {
                    session_id: "malformed-session".to_string(),
                    state: Mutex::new(MockZkProofState {
                        proof_status: ProofStatus::Succeeded,
                        omit_result_on_success: true,
                        ..Default::default()
                    }),
                });

                let mut proofs = PendingProofs::new();
                proofs.insert(
                    addr(0),
                    PendingProof::awaiting(
                        "malformed-session".to_string(),
                        0,
                        B256::ZERO,
                        minimal_prove_request(),
                        DisputeIntent::Challenge,
                    ),
                );

                proofs.poll(addr(0), &*zk, Duration::from_secs(3600)).await.unwrap();
            });

            let snapshot = snap.snapshot().into_vec();
            assert_eq!(
                find_failure_counter_with_reason(
                    &snapshot,
                    ChallengerMetrics::PROOF_FAILURE_MALFORMED,
                ),
                Some(1),
                "Succeeded with no result must increment \
                 proof_session_failures_total{{reason=malformed}}",
            );
        });
    }

    #[test]
    fn test_poll_tee_validation_failure_emits_metric() {
        with_recorder(|snap| {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

            rt.block_on(async {
                let expected_root = B256::repeat_byte(0xaa);
                let bad_proposal = Proposal {
                    output_root: B256::repeat_byte(0xbb),
                    signature: Bytes::from(vec![0u8; 65]),
                    l1_origin_hash: DEFAULT_L1_HEAD,
                    l1_origin_number: 1000,
                    l2_block_number: 20,
                    prev_output_root: B256::ZERO,
                    config_hash: B256::ZERO,
                };
                let zk = Arc::new(MockZkProofProvider {
                    session_id: "tee-bad-root-session".to_string(),
                    state: Mutex::new(MockZkProofState {
                        proof_status: ProofStatus::Succeeded,
                        result: Some(tee_api_result(bad_proposal)),
                        ..Default::default()
                    }),
                });

                let mut proofs = PendingProofs::new();
                proofs.insert(
                    addr(0),
                    PendingProof::awaiting_tee(
                        "tee-bad-root-session".to_string(),
                        0,
                        expected_root,
                        Some(minimal_prove_request()),
                        Some(DisputeIntent::Challenge),
                    ),
                );

                proofs.poll(addr(0), &*zk, Duration::from_secs(3600)).await.unwrap();
            });

            let snapshot = snap.snapshot().into_vec();
            assert_eq!(
                find_failure_counter_with_reason(
                    &snapshot,
                    ChallengerMetrics::PROOF_FAILURE_TEE_VALIDATION,
                ),
                Some(1),
                "TEE root mismatch must increment \
                 proof_session_failures_total{{reason=tee_validation_failed}}",
            );
        });
    }

    #[test]
    fn test_step_proof_exhaustion_emits_metric() {
        with_recorder(|snap| {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

            rt.block_on(async {
                let (l2, factory, verifier) = invalid_game_mocks();
                let zk = failed_zk_prover("fail-forever");
                let tx_manager = default_tx_manager();
                let mut driver = test_driver(factory, Arc::clone(&verifier), l2, zk, tx_manager);

                // Step 1: initiate the proof.
                driver.step().await.unwrap();

                // Force the entry past the retry budget so the next `step()`
                // exhausts. The scanner re-creates a fresh entry with
                // `retry_count = 0` in the same call, so we assert on the
                // metric only.
                let max_retries =
                    Driver::<MockL2Provider, MockZkProofProvider, MockTxManager>::MAX_PROOF_RETRIES;
                let entry = driver
                    .pending_proofs
                    .get_mut(&addr(0))
                    .expect("entry should exist after initiation");
                entry.retry_count = max_retries + 1;
                entry.phase = ProofPhase::NeedsRetry;
                let _ = &verifier; // game already InProgress from invalid_game_mocks

                driver.step().await.unwrap();
            });

            let snapshot = snap.snapshot().into_vec();
            assert_eq!(
                find_counter(&snapshot, "base_challenger.proof_retries_exhausted_total"),
                Some(1),
                "retry exhaustion must increment proof_retries_exhausted_total",
            );
        });
    }
}
