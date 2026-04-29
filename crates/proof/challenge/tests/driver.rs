//! Integration tests for the challenger [`Driver`] loop.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Duration,
};

use alloy_primitives::{Address, B256, Bytes};
use base_challenger::{
    BondManager, ChallengeSubmitter, DisputeIntent, Driver, DriverComponents, DriverConfig,
    GameScanner, L1HeadProvider, OutputValidator, PendingProof, ProofPhase, ScannerConfig,
    TeeConfig,
    test_utils::{
        DEFAULT_L1_HEAD, DEFAULT_TEE_PROVER, MockAggregateVerifier, MockBondTransactionSubmitter,
        MockDisputeGameFactory, MockGameState, MockL1HeadProvider, MockL2Provider,
        MockTeeProofProvider, MockTxManager, MockZkProofProvider, MockZkProofState,
        TEST_DISCOVERY_INTERVAL, addr, build_test_header_and_account, empty_factory, factory_game,
        mock_state, mock_state_with_tee, receipt_with_status,
    },
};
use base_proof_contracts::{AggregateVerifierClient, ContractError, GameAtIndex};
use base_proof_primitives::{ProofResult, Proposal, ProverClient};
use base_protocol::OutputRoot;
use base_runtime::TokioRuntime;
use base_tx_manager::TxManagerError;
use base_zk_client::{ProofJobStatus, ProofType, ProveBlockRequest};
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
    let scanner = GameScanner::new(
        factory,
        Arc::clone(&verifier) as Arc<dyn AggregateVerifierClient>,
        ScannerConfig { lookback_games: 1000 },
    );
    let validator = OutputValidator::new(l2_provider);
    let submitter = ChallengeSubmitter::new(tx_manager);

    let config = DriverConfig {
        poll_interval: Duration::from_millis(10),
        cancel: CancellationToken::new(),
        ready: Arc::new(AtomicBool::new(false)),
    };

    Driver::new(
        config,
        DriverComponents {
            scanner,
            validator,
            zk_prover,
            submitter,
            tee,
            verifier_client: verifier as Arc<dyn AggregateVerifierClient>,
            bond_manager: None,
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
    Arc::new(MockDisputeGameFactory { games: vec![factory_game(0, 1)] })
}

fn single_game_verifier(state: MockGameState) -> Arc<MockAggregateVerifier> {
    Arc::new(MockAggregateVerifier::new(HashMap::from([(addr(0), state)])))
}

fn tee_config(
    provider: Arc<dyn ProverClient>,
    l1_head_provider: Arc<dyn L1HeadProvider>,
) -> TeeConfig {
    TeeConfig { provider, l1_head_provider, request_timeout: Duration::from_secs(30) }
}

fn default_ready_proof(intent: DisputeIntent) -> PendingProof {
    let session_id = PendingProof::derive_session_id(addr(0), 1);

    let request = ProveBlockRequest {
        start_block_number: 15,
        number_of_blocks_to_prove: 5,
        sequence_window: None,
        proof_type: ProofType::SnarkGroth16.into(),
        session_id: Some(session_id),
        prover_address: Some(format!("{:#x}", addr(0))),
        l1_head: Some(format!("{DEFAULT_L1_HEAD:#x}")),
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
            proof_status: ProofJobStatus::Succeeded as i32,
            receipt,
            ..Default::default()
        }),
    })
}

fn failed_zk_prover(session_id: &str) -> Arc<MockZkProofProvider> {
    Arc::new(MockZkProofProvider {
        session_id: session_id.to_string(),
        state: Mutex::new(MockZkProofState {
            proof_status: ProofJobStatus::Failed as i32,
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
    let factory = Arc::new(MockDisputeGameFactory { games: vec![] });
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

    // Simulate the on-chain effect of a successful challenge: game is resolved.
    verifier.update_game(addr(0), MockGameState { status: 1, ..game_state(20) });

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
    let verifier = empty_verifier();
    let scanner = GameScanner::new(
        factory,
        Arc::clone(&verifier) as Arc<dyn AggregateVerifierClient>,
        ScannerConfig { lookback_games: 1000 },
    );

    let l2 = default_l2();
    let validator = OutputValidator::new(l2);
    let submitter = ChallengeSubmitter::new(default_tx_manager());

    let config = DriverConfig {
        poll_interval: Duration::from_millis(10),
        cancel: CancellationToken::new(),
        ready: Arc::new(AtomicBool::new(false)),
    };

    let mut driver = Driver::new(
        config,
        DriverComponents {
            scanner,
            validator,
            zk_prover: default_zk_prover(),
            submitter,
            tee: None,
            verifier_client: verifier as Arc<dyn AggregateVerifierClient>,
            bond_manager: None::<BondManager<TokioRuntime>>,
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
        state: Mutex::new(MockZkProofState { receipt: vec![0xBE, 0xEF], ..Default::default() }),
    });

    let tx_manager = default_tx_manager();

    let mut driver = test_driver(factory, Arc::clone(&verifier), l2, Arc::clone(&zk), tx_manager);

    // Step 1: proof is initiated but not ready (Unspecified) → session stored.
    driver.step().await.unwrap();
    assert!(
        driver.pending_proofs.contains_key(&addr(0)),
        "session should be stored in pending_proofs"
    );

    // Simulate the proof completing before the next poll.
    zk.state.lock().unwrap().proof_status = ProofJobStatus::Succeeded as i32;

    // Simulate the on-chain effect: game is resolved after challenge tx.
    verifier.update_game(addr(0), MockGameState { status: 1, ..game_state(20) });

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

    // Simulate the on-chain effect of a successful challenge: game is resolved.
    verifier.update_game(addr(0), MockGameState { status: 1, ..game_state(20) });

    // Step 3: poll_pending_proofs re-submits the challenge tx, now it succeeds.
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "entry should be removed after successful submission"
    );
}

#[tokio::test]
async fn test_poll_or_submit_drops_resolved_game() {
    // Game has resolved (status=1 CHALLENGER_WINS) — driver should drop the
    // pending proof without attempting submission.
    let mut driver = driver_with_ready_proof(mock_state(1, Address::ZERO, 20));
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
    let mut driver = driver_with_ready_proof(mock_state(0, ZK_PROVER_ADDR, 20));
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
    let mut driver =
        driver_with_ready_proof(mock_state_with_tee(0, Address::ZERO, Address::ZERO, 20));
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "nullified game should be removed from pending_proofs"
    );
}

#[tokio::test]
async fn test_run_cancellation() {
    let factory = Arc::new(MockDisputeGameFactory { games: vec![] });
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
            proof_status: ProofJobStatus::Failed as i32,
            receipt: vec![0xBE, 0xEF],
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
    zk.state.lock().unwrap().proof_status = ProofJobStatus::Succeeded as i32;

    // Simulate the on-chain effect of a successful challenge: game is resolved.
    verifier.update_game(addr(0), MockGameState { status: 1, ..game_state(20) });

    // Step 3: proof succeeds, challenge tx submitted, entry removed.
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "entry should be removed after successful challenge submission"
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

    // Simulate the on-chain effect: mark the game as resolved so the
    // stateless scanner does not re-discover it after the entry is dropped.
    verifier.update_game(addr(0), MockGameState { status: 1, ..game_state(20) });

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
    // TEE proof attempt fails → driver falls back to ZK.
    let (l2, factory, verifier) = invalid_game_mocks();

    let tee = Arc::new(MockTeeProofProvider::failure("enclave unreachable"));

    let tx_manager = default_tx_manager();
    let mut driver = test_driver_with_tee(
        factory,
        verifier,
        l2,
        default_zk_prover(),
        tx_manager,
        Some(tee_config(tee, Arc::new(MockL1HeadProvider::failure("dummy")))),
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
    // No TEE provider configured → go straight to ZK.
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

    let tee = Arc::new(MockTeeProofProvider::failure("L1 unreachable"));
    let zk = succeeded_zk_prover("zk-after-tee-fail", vec![0xDE, 0xAD]);

    let tx_manager = default_tx_manager();

    let mut driver = test_driver_with_tee(
        factory,
        Arc::clone(&verifier),
        l2,
        zk,
        tx_manager,
        Some(tee_config(tee, Arc::new(MockL1HeadProvider::failure("dummy")))),
    );

    // Step 1: TEE path is attempted (fails due to provider error), falls back
    // to ZK, proof session initiated (polled on next tick).
    driver.step().await.unwrap();
    assert!(
        driver.pending_proofs.contains_key(&addr(0)),
        "ZK proof should be pending after TEE fallback"
    );

    // Simulate the on-chain effect of a successful challenge: game is resolved.
    verifier.update_game(addr(0), MockGameState { status: 1, ..game_state(20) });

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
    let tee_provider = Arc::new(MockTeeProofProvider::success(ProofResult::Tee {
        aggregate_proposal,
        proposals: vec![],
    }));

    let tx_manager = default_tx_manager();

    let mut driver = test_driver_with_tee(
        factory,
        verifier,
        l2,
        default_zk_prover(),
        tx_manager,
        Some(tee_config(tee_provider, l1_head)),
    );

    driver.step().await.unwrap();

    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "no pending ZK proof should exist after successful TEE submission"
    );
}

#[tokio::test]
async fn test_step_tee_submission_failure_falls_back_to_zk() {
    // TEE proof succeeds but the on-chain nullify() tx fails.
    // The driver should immediately fall back to a ZK proof instead of
    // retrying the same TEE proof indefinitely.
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
    let tee_provider = Arc::new(MockTeeProofProvider::success(ProofResult::Tee {
        aggregate_proposal,
        proposals: vec![],
    }));

    let zk = succeeded_zk_prover("zk-fallback-after-tee-tx-fail", vec![0xDE, 0xAD]);

    // TEE nullify() tx fails (NonceTooLow), ZK challenge() tx succeeds.
    let tx_manager = MockTxManager::with_responses(vec![
        Err(TxManagerError::NonceTooLow),
        Ok(receipt_with_status(true, DEFAULT_TX_HASH)),
    ]);

    let mut driver = test_driver_with_tee(
        factory,
        Arc::clone(&verifier),
        l2,
        zk,
        tx_manager,
        Some(tee_config(tee_provider, l1_head)),
    );

    // Step 1: TEE proof obtained, nullify() tx fails, falls back to ZK.
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

    // Simulate the on-chain effect of a successful challenge: game is
    // resolved. This prevents the scanner from re-discovering the game
    // after the pending proof is submitted in step 2.
    verifier.update_game(addr(0), MockGameState { status: 1, ..game_state(20) });

    // Step 2: ZK proof polled → Succeeded → entry cleaned up.
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "entry should be removed after ZK fallback completes"
    );
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
    let mut game_state = mock_state_with_tee(0, ZK_PROVER_ADDR, DEFAULT_TEE_PROVER, 20);
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
async fn test_poll_or_submit_challenge_intent_dropped_when_zk_prover_set() {
    // A pending proof with DisputeIntent::Challenge should be dropped
    // when zkProver is non-zero (game already challenged).
    let factory = single_game_factory();
    let l2 = default_l2();
    let verifier =
        single_game_verifier(mock_state_with_tee(0, ZK_PROVER_ADDR, DEFAULT_TEE_PROVER, 20));

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
/// `correct_root_at_20` controls whether the on-chain root at index 1
/// (block 20) matches the L2-computed root:
/// - `true`: on-chain root is correct → ZK challenge was fraudulent → nullify.
/// - `false`: on-chain root is bogus → ZK challenge was legitimate → skip.
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
    // The on-chain root at the challenged index is wrong, meaning the ZK
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
    // The on-chain root at the challenged index is correct, meaning the
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
    let tee_provider = Arc::new(MockTeeProofProvider::success(ProofResult::Tee {
        aggregate_proposal,
        proposals: vec![],
    }));

    let tx_manager = default_tx_manager();

    let mut driver = test_driver_with_tee(
        factory,
        verifier,
        l2,
        default_zk_prover(),
        tx_manager,
        Some(tee_config(tee_provider, l1_head)),
    );

    driver.step().await.unwrap();

    // TEE proof was submitted synchronously; no pending ZK proof should remain.
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "TEE proof should have been submitted directly for dual-proof game"
    );
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

    let tee = Arc::new(MockTeeProofProvider::failure("enclave unreachable"));

    let mut driver = test_driver_with_tee(
        factory,
        verifier,
        l2,
        default_zk_prover(),
        default_tx_manager(),
        Some(tee_config(tee, Arc::new(MockL1HeadProvider::failure("dummy")))),
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
    let mut state = mock_state(1, Address::ZERO, 100);
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
    // status=1 (CHALLENGER_WINS) to represent a game that has already been
    // resolved on-chain. The manager detects this and advances directly
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

    // Poll 1: NeedsResolve → status=1 (already resolved, CHALLENGER_WINS) → NeedsUnlock.
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

    // Poll 2: NeedsUnlock → bond_unlocked=true → AwaitingDelay (no tx).
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 1);

    // Poll 3: AwaitingDelay (delay=0) → NeedsWithdraw.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 1);

    // Poll 4: NeedsWithdraw → submit withdraw → Completed → removed.
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

    // Poll 1: NeedsResolve → status=1 (already resolved, CHALLENGER_WINS) → NeedsUnlock.
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
    state.status = 2; // DEFENDER_WINS
    let verifier = single_game_verifier(state);

    // One response for the anchor state update that is attempted after
    // advance_game (DEFENDER_WINS triggers a setAnchorState call).
    let submitter = MockBondTransactionSubmitter::with_responses(vec![Ok(DEFAULT_TX_HASH)]);

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
    state.status = 2; // DEFENDER_WINS
    state.bond_recipient = other_addr; // bond goes to someone else
    let verifier = single_game_verifier(state);

    // One response for the anchor state update — even though the bond is
    // not claimable, anchor updates are a public good and are still
    // attempted for DEFENDER_WINS games before removal.
    let submitter = MockBondTransactionSubmitter::with_responses(vec![Ok(DEFAULT_TX_HASH)]);

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

    // The only transaction submitted should be the anchor state update.
    let calls = submitter.recorded_calls();
    assert_eq!(calls.len(), 1, "only the anchor update tx should be submitted");
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
