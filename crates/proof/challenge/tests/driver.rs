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
        MockAggregateVerifier, MockBondTransactionSubmitter, MockDisputeGameFactory, MockGameState,
        MockL1HeadProvider, MockL2Provider, MockTeeProofProvider, MockTxManager,
        MockZkProofProvider, addr, build_test_header_and_account, empty_factory, factory_game,
        mock_state, mock_state_with_tee, receipt_with_status,
    },
};
use base_proof_contracts::{AggregateVerifierClient, ContractError, GameAtIndex};
use base_proof_primitives::{ProofResult, Proposal, ProverClient};
use base_protocol::OutputRoot;
use base_tx_manager::TxManagerError;
use base_zk_client::{ProofJobStatus, ProofType, ProveBlockRequest};
use tokio_util::sync::CancellationToken;

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

fn default_prove_request() -> ProveBlockRequest {
    let session_id = PendingProof::derive_session_id(addr(0), 1);

    ProveBlockRequest {
        start_block_number: 15,
        number_of_blocks_to_prove: 5,
        sequence_window: None,
        proof_type: ProofType::GenericZkvmClusterSnarkGroth16.into(),
        session_id: Some(session_id),
        prover_address: Some(format!("{:#x}", addr(0))),
        l1_head: Some(format!("{:#x}", B256::repeat_byte(0xAA))),
    }
}

/// Builds the common L2, factory, and verifier mocks for an invalid-game
/// scenario: starting=10, `l2_block=20`, interval=5, checkpoints at 15 and
/// 20 with a correct root at 15 and a bogus root at 20 (invalid index 1).
fn invalid_game_mocks()
-> (Arc<MockL2Provider>, Arc<MockDisputeGameFactory>, Arc<MockAggregateVerifier>) {
    let storage_hash = B256::repeat_byte(0xBB);
    let (header_15, account_15) = build_test_header_and_account(15, storage_hash);
    let root_15 =
        OutputRoot::from_parts(header_15.state_root, storage_hash, header_15.hash_slow()).hash();
    let (header_20, account_20) = build_test_header_and_account(20, storage_hash);

    let mut l2 = MockL2Provider::new();
    l2.insert_block(15, header_15, account_15);
    l2.insert_block(20, header_20, account_20);
    let l2 = Arc::new(l2);

    let factory = Arc::new(MockDisputeGameFactory { games: vec![factory_game(0, 1)] });
    let mut verifier_games = HashMap::new();
    let tee_addr = Address::repeat_byte(0xEE);
    verifier_games.insert(
        addr(0),
        MockGameState {
            tee_prover: tee_addr,
            game_info: base_proof_contracts::GameInfo {
                root_claim: B256::repeat_byte(0x01),
                l2_block_number: 20,
                parent_index: 0,
            },
            starting_block_number: 10,
            l1_head: B256::repeat_byte(0xAA),
            intermediate_output_roots: vec![root_15, B256::repeat_byte(0xFF)],
            ..Default::default()
        },
    );
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    (l2, factory, verifier)
}

/// Builds a driver with a single pending `ReadyToSubmit` proof at `addr(0)`
/// whose verifier reports the given `game_state`.
fn driver_with_ready_proof(
    game_state: MockGameState,
) -> Driver<MockL2Provider, MockZkProofProvider, MockTxManager> {
    let (l2, factory, _verifier) = invalid_game_mocks();
    let verifier_games = HashMap::from([(addr(0), game_state)]);
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });
    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());
    driver.pending_proofs.insert(
        addr(0),
        PendingProof::ready(
            Bytes::from_static(&[0x01, 0xDE, 0xAD]),
            1,
            B256::repeat_byte(0xEE),
            default_prove_request(),
            DisputeIntent::Challenge,
        ),
    );
    driver
}

#[tokio::test]
async fn test_step_no_candidates() {
    let factory = Arc::new(MockDisputeGameFactory { games: vec![] });
    let verifier = Arc::new(MockAggregateVerifier { games: HashMap::new() });
    let l2 = Arc::new(MockL2Provider::new());

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());

    driver.step().await.unwrap();
    // No error, no panic — empty scan is fine.
}

#[tokio::test]
async fn test_step_valid_game_skipped() {
    // Game with valid intermediate roots → no proof requested.
    // We set up a game that will pass validation because intermediate_roots is empty
    // and l2_block_number - starting_block_number < intermediate_block_interval
    // so expected_count = 0 → trivially valid.
    let factory = Arc::new(MockDisputeGameFactory { games: vec![factory_game(0, 1)] });
    let mut verifier_games = HashMap::new();
    let tee_addr = Address::repeat_byte(0xEE);
    verifier_games.insert(
        addr(0),
        MockGameState {
            tee_prover: tee_addr,
            game_info: base_proof_contracts::GameInfo {
                root_claim: B256::repeat_byte(0x01),
                l2_block_number: 14,
                parent_index: 0,
            },
            starting_block_number: 10,
            l1_head: B256::repeat_byte(0xAA),
            ..Default::default()
        },
    );
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });
    let l2 = Arc::new(MockL2Provider::new());

    // The ZK prover should NOT be called since the game is valid.
    let zk = Arc::new(MockZkProofProvider {
        session_id: "should-not-be-called".to_string(),
        ..Default::default()
    });

    let mut driver = test_driver(factory, verifier, l2, zk, default_tx_manager());

    driver.step().await.unwrap();
    // If the ZK prover were called, the test would still pass, but the game
    // being valid means process_candidate returns early.
}

#[tokio::test]
async fn test_step_validation_error_blocks_not_available() {
    // Game with intermediate roots, but checkpoint blocks are unavailable.
    // Validator returns BlockNotAvailable → process_candidate skips gracefully.
    let factory = Arc::new(MockDisputeGameFactory { games: vec![factory_game(0, 1)] });
    let mut verifier_games = HashMap::new();
    let tee_addr = Address::repeat_byte(0xEE);
    verifier_games.insert(
        addr(0),
        MockGameState {
            status: 0,
            zk_prover: Address::ZERO,
            tee_prover: tee_addr,
            game_info: base_proof_contracts::GameInfo {
                root_claim: B256::repeat_byte(0x01),
                l2_block_number: 20,
                parent_index: 0,
            },
            starting_block_number: 10,
            l1_head: B256::repeat_byte(0xAA),
            intermediate_output_roots: vec![B256::repeat_byte(0xFF), B256::repeat_byte(0xEE)],
            ..Default::default()
        },
    );
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    // Checkpoint blocks are not available → validator returns BlockNotAvailable.
    let mut l2 = MockL2Provider::new();
    l2.error_blocks.push(15);
    l2.error_blocks.push(20);
    let l2 = Arc::new(l2);

    let zk = Arc::new(MockZkProofProvider {
        session_id: "test-session".to_string(),
        ..Default::default()
    });

    let mut driver = test_driver(factory, verifier, l2, zk, default_tx_manager());

    // step succeeds — BlockNotAvailable causes process_candidate to skip
    driver.step().await.unwrap();
}

#[tokio::test]
async fn test_step_invalid_game_proof_succeeded() {
    // Proof succeeds → nullification submitted.
    let (l2, factory, verifier) = invalid_game_mocks();

    let zk = Arc::new(MockZkProofProvider {
        session_id: "proof-123".to_string(),
        proof_status: Mutex::new(ProofJobStatus::Succeeded as i32),
        receipt: Mutex::new(vec![0xDE, 0xAD]),
        ..Default::default()
    });

    let tx_hash = B256::repeat_byte(0xCC);
    let tx_manager = MockTxManager::new(Ok(receipt_with_status(true, tx_hash)));

    let mut driver = test_driver(factory, verifier, l2, zk, tx_manager);

    // Step 1: proof initiated, not yet polled.
    driver.step().await.unwrap();
    assert!(
        driver.pending_proofs.contains_key(&addr(0)),
        "proof should be pending after initiation"
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
    // ZK prover returns Failed → entry retained and re-initiated with retry_count == 1.
    let (l2, factory, verifier) = invalid_game_mocks();

    let zk = Arc::new(MockZkProofProvider {
        session_id: "proof-fail".to_string(),
        proof_status: Mutex::new(ProofJobStatus::Failed as i32),
        ..Default::default()
    });

    // tx_manager should NOT be called (proof failed → no submission)
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
async fn test_step_validation_error_skipped() {
    // Game where validator returns an error (e.g., BlockNotAvailable)
    // → process_candidate logs and returns Ok.
    let factory = Arc::new(MockDisputeGameFactory { games: vec![factory_game(0, 1)] });
    let mut verifier_games = HashMap::new();
    let tee_addr = Address::repeat_byte(0xEE);
    verifier_games.insert(
        addr(0),
        MockGameState {
            status: 0,
            zk_prover: Address::ZERO,
            tee_prover: tee_addr,
            game_info: base_proof_contracts::GameInfo {
                root_claim: B256::repeat_byte(0x01),
                l2_block_number: 20,
                parent_index: 0,
            },
            starting_block_number: 10,
            l1_head: B256::repeat_byte(0xAA),
            // 2 roots expected at interval=5, provide 2 so count matches
            intermediate_output_roots: vec![B256::ZERO, B256::ZERO],
            ..Default::default()
        },
    );
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    // L2 provider has no blocks → validator returns BlockNotAvailable
    let l2 = Arc::new(MockL2Provider::new());

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());

    // step succeeds — validation error is skipped
    driver.step().await.unwrap();
}

#[tokio::test]
async fn test_step_scan_error_propagated() {
    // Use ErrorOnIndexFactory where game_count succeeds but game_at_index
    // errors. But scan itself catches per-game errors. To get scan to fail,
    // we need game_count to fail, which requires a custom factory.

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
    }

    let factory = Arc::new(FailingFactory);
    let verifier = Arc::new(MockAggregateVerifier { games: HashMap::new() });
    let scanner = GameScanner::new(
        factory,
        Arc::clone(&verifier) as Arc<dyn AggregateVerifierClient>,
        ScannerConfig { lookback_games: 1000 },
    );

    let l2 = Arc::new(MockL2Provider::new());
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
            bond_manager: None,
        },
    );

    let result = driver.step().await;
    assert!(result.is_err(), "scan error should propagate");
}

#[tokio::test]
async fn test_step_pending_proof_skips_prove_block() {
    // First step: proof initiated (status=Unspecified via Default, not ready).
    // Second step: same game re-discovered → polls existing session,
    // proof succeeds, nullification submitted.
    let (l2, factory, verifier) = invalid_game_mocks();

    let zk = Arc::new(MockZkProofProvider {
        session_id: "pending-session".to_string(),
        receipt: Mutex::new(vec![0xBE, 0xEF]),
        ..Default::default()
    });

    let tx_hash = B256::repeat_byte(0xDD);
    let tx_manager = MockTxManager::new(Ok(receipt_with_status(true, tx_hash)));

    let mut driver = test_driver(factory, verifier, l2, Arc::clone(&zk), tx_manager);

    // Step 1: proof is initiated but not ready (Unspecified) → session stored.
    driver.step().await.unwrap();
    assert!(
        driver.pending_proofs.contains_key(&addr(0)),
        "session should be stored in pending_proofs"
    );

    // Simulate the proof completing before the next poll.
    *zk.proof_status.lock().unwrap() = ProofJobStatus::Succeeded as i32;

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
    // Proof succeeds on the second step but nullification tx fails.
    // The entry should stay in pending_proofs as ReadyToSubmit.
    // On the next step the tx succeeds without re-proving.
    let (l2, factory, verifier) = invalid_game_mocks();

    let zk = Arc::new(MockZkProofProvider {
        session_id: "proof-ok".to_string(),
        proof_status: Mutex::new(ProofJobStatus::Succeeded as i32),
        receipt: Mutex::new(vec![0xDE, 0xAD]),
        ..Default::default()
    });

    // First tx call fails (NonceTooLow), second succeeds.
    let tx_manager = MockTxManager::with_responses(vec![
        Err(TxManagerError::NonceTooLow),
        Ok(receipt_with_status(true, B256::repeat_byte(0xCC))),
    ]);

    let mut driver = test_driver(factory, verifier, l2, zk, tx_manager);

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
    let mut driver = driver_with_ready_proof(mock_state(0, Address::repeat_byte(0xCC), 20));
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
    let verifier = Arc::new(MockAggregateVerifier { games: HashMap::new() });
    let l2 = Arc::new(MockL2Provider::new());

    let scanner = GameScanner::new(
        Arc::clone(&factory) as Arc<dyn base_proof_contracts::DisputeGameFactoryClient>,
        Arc::clone(&verifier) as Arc<dyn AggregateVerifierClient>,
        ScannerConfig { lookback_games: 1000 },
    );
    let validator = OutputValidator::new(l2);
    let submitter = ChallengeSubmitter::new(default_tx_manager());
    let cancel = CancellationToken::new();

    let config = DriverConfig {
        poll_interval: Duration::from_secs(60), // long poll so it blocks
        cancel: cancel.clone(),
        ready: Arc::new(AtomicBool::new(false)),
    };

    let driver = Driver::new(
        config,
        DriverComponents {
            scanner,
            validator,
            zk_prover: default_zk_prover(),
            submitter,
            tee: None,
            verifier_client: verifier as Arc<dyn AggregateVerifierClient>,
            bond_manager: None,
        },
    );

    // Cancel immediately
    cancel.cancel();

    // run() should return promptly
    tokio::time::timeout(Duration::from_secs(2), driver.run())
        .await
        .expect("driver.run() should exit promptly after cancellation");
}

#[tokio::test]
async fn test_step_proof_retry_succeeds() {
    // Proof is initiated on the first tick, fails on the second tick
    // (NeedsRetry), then re-initiated prove_block returns a new session.
    // On the third tick the proof succeeds and challenge tx is submitted.
    let (l2, factory, verifier) = invalid_game_mocks();

    let zk = Arc::new(MockZkProofProvider {
        session_id: "retry-session".to_string(),
        proof_status: Mutex::new(ProofJobStatus::Failed as i32),
        receipt: Mutex::new(vec![0xBE, 0xEF]),
        ..Default::default()
    });

    let tx_hash = B256::repeat_byte(0xDD);
    let tx_manager = MockTxManager::new(Ok(receipt_with_status(true, tx_hash)));

    let mut driver = test_driver(factory, verifier, l2, Arc::clone(&zk), tx_manager);

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
    *zk.proof_status.lock().unwrap() = ProofJobStatus::Succeeded as i32;

    // Step 3: proof succeeds, challenge tx submitted, entry removed.
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "entry should be removed after successful challenge submission"
    );
}

#[tokio::test]
async fn test_step_proof_exceeds_max_retries() {
    // Proof keeps failing → entry dropped after MAX_PROOF_RETRIES + 1 failures.
    let (l2, factory, verifier) = invalid_game_mocks();

    let zk = Arc::new(MockZkProofProvider {
        session_id: "fail-forever".to_string(),
        proof_status: Mutex::new(ProofJobStatus::Failed as i32),
        ..Default::default()
    });

    let tx_manager = default_tx_manager();
    let mut driver = test_driver(factory, verifier, l2, zk, tx_manager);

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

    // One more step: poll returns Failed → retry_count becomes max_retries + 1,
    // handle_proof_retry sees retry_count > MAX_PROOF_RETRIES and drops the entry.
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "entry should be dropped after exceeding max retries"
    );
}

// ── TEE-first proof sourcing tests ─────────────────────────────────────────

/// Builds the common mocks for a TEE-eligible invalid-game scenario.
///
/// The game at `addr(0)` has `tee_prover = 0xEE..EE` and the same block
/// layout as `invalid_game_mocks()`.
#[tokio::test]
async fn test_step_invalid_game_tee_fails_zk_fallback() {
    // Game has a TEE prover, TEE provider is configured, but the TEE proof
    // attempt fails (L1 provider is unreachable with dummy). The driver
    // should fall back to ZK and initiate a ZK proof session.
    let (l2, factory, verifier) = invalid_game_mocks();

    let tee = Arc::new(MockTeeProofProvider::failure("enclave unreachable"));
    let zk = Arc::new(MockZkProofProvider {
        session_id: "zk-fallback".to_string(),
        ..Default::default()
    });

    let tx_manager = default_tx_manager();
    let mut driver = test_driver_with_tee(
        factory,
        verifier,
        l2,
        zk,
        tx_manager,
        Some(TeeConfig {
            provider: tee as Arc<dyn ProverClient>,
            l1_head_provider: Arc::new(MockL1HeadProvider::failure("dummy")),
            request_timeout: Duration::from_secs(30),
        }),
    );

    driver.step().await.unwrap();

    // The TEE attempt fails, so a ZK proof session should be initiated.
    let entry =
        driver.pending_proofs.get(&addr(0)).expect("ZK proof should be pending after TEE fallback");
    assert!(
        matches!(entry.phase, ProofPhase::AwaitingProof { .. }),
        "phase should be AwaitingProof (ZK fallback)"
    );
}

#[tokio::test]
async fn test_step_invalid_game_no_tee_provider_zk_only() {
    // Game has a TEE prover, but the driver has no TEE provider configured
    // (tee-rpc-url was not set). Should go straight to ZK.
    let (l2, factory, verifier) = invalid_game_mocks();

    let zk = Arc::new(MockZkProofProvider {
        session_id: "zk-no-provider".to_string(),
        ..Default::default()
    });

    let tx_manager = default_tx_manager();
    // No TEE provider (None).
    let mut driver = test_driver(factory, verifier, l2, zk, tx_manager);

    driver.step().await.unwrap();

    let entry = driver.pending_proofs.get(&addr(0)).expect("ZK proof should be pending");
    assert!(
        matches!(entry.phase, ProofPhase::AwaitingProof { .. }),
        "phase should be AwaitingProof (ZK, no TEE provider)"
    );
}

#[tokio::test]
async fn test_step_invalid_game_tee_fails_zk_succeeds() {
    // Game has a TEE prover, TEE proof attempt fails, driver falls back to
    // ZK, ZK proof succeeds on the next poll, challenge tx submitted.
    let (l2, factory, verifier) = invalid_game_mocks();

    let tee = Arc::new(MockTeeProofProvider::failure("L1 unreachable"));
    let zk = Arc::new(MockZkProofProvider {
        session_id: "zk-after-tee-fail".to_string(),
        proof_status: Mutex::new(ProofJobStatus::Succeeded as i32),
        receipt: Mutex::new(vec![0xDE, 0xAD]),
        ..Default::default()
    });

    let tx_hash = B256::repeat_byte(0xCC);
    let tx_manager = MockTxManager::new(Ok(receipt_with_status(true, tx_hash)));

    let mut driver = test_driver_with_tee(
        factory,
        verifier,
        l2,
        zk,
        tx_manager,
        Some(TeeConfig {
            provider: tee as Arc<dyn ProverClient>,
            l1_head_provider: Arc::new(MockL1HeadProvider::failure("dummy")),
            request_timeout: Duration::from_secs(30),
        }),
    );

    // Step 1: TEE path is attempted (fails due to provider error), falls back
    // to ZK, proof session initiated (polled on next tick).
    driver.step().await.unwrap();
    assert!(
        driver.pending_proofs.contains_key(&addr(0)),
        "ZK proof should be pending after TEE fallback"
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
    // Game has a TEE prover, L1 head provider returns a valid hash, TEE proof
    // provider returns a valid proof with the correct output root. The driver
    // should submit the TEE proof directly without initiating a ZK session.
    let storage_hash = B256::repeat_byte(0xBB);
    let (header_15, account_15) = build_test_header_and_account(15, storage_hash);
    let root_15 =
        OutputRoot::from_parts(header_15.state_root, storage_hash, header_15.hash_slow()).hash();
    let (header_20, account_20) = build_test_header_and_account(20, storage_hash);
    let root_20 =
        OutputRoot::from_parts(header_20.state_root, storage_hash, header_20.hash_slow()).hash();

    let mut l2 = MockL2Provider::new();
    l2.insert_block(15, header_15, account_15);
    l2.insert_block(20, header_20, account_20);
    let l2 = Arc::new(l2);

    let l1_hash = B256::repeat_byte(0xAA);

    let factory = Arc::new(MockDisputeGameFactory { games: vec![factory_game(0, 1)] });
    let tee_addr = Address::repeat_byte(0xEE);
    let mut verifier_games = HashMap::new();
    verifier_games.insert(
        addr(0),
        MockGameState {
            status: 0,
            zk_prover: Address::ZERO,
            tee_prover: tee_addr,
            game_info: base_proof_contracts::GameInfo {
                root_claim: B256::repeat_byte(0x01),
                l2_block_number: 20,
                parent_index: 0,
            },
            starting_block_number: 10,
            l1_head: l1_hash,
            // root_15 is correct, index 1 is bogus — invalid_index == 1
            intermediate_output_roots: vec![root_15, B256::repeat_byte(0xFF)],
            ..Default::default()
        },
    );
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    let l1_head = Arc::new(MockL1HeadProvider::success(l1_hash, 100));

    let aggregate_proposal = Proposal {
        output_root: root_20,
        signature: Bytes::from(vec![0u8; 65]),
        l1_origin_hash: l1_hash,
        l1_origin_number: 1000,
        l2_block_number: 20,
        prev_output_root: root_15,
        config_hash: B256::ZERO,
    };
    let tee_provider = Arc::new(MockTeeProofProvider::success(ProofResult::Tee {
        aggregate_proposal,
        proposals: vec![],
    }));

    let tx_hash = B256::repeat_byte(0xDD);
    let tx_manager = MockTxManager::new(Ok(receipt_with_status(true, tx_hash)));

    // ZK prover should NOT be called since TEE proof succeeds.
    let zk = Arc::new(MockZkProofProvider {
        session_id: "should-not-be-called".to_string(),
        ..Default::default()
    });

    let mut driver = test_driver_with_tee(
        factory,
        verifier,
        l2,
        zk,
        tx_manager,
        Some(TeeConfig {
            provider: tee_provider as Arc<dyn ProverClient>,
            l1_head_provider: l1_head as Arc<dyn L1HeadProvider>,
            request_timeout: Duration::from_secs(30),
        }),
    );

    driver.step().await.unwrap();

    // TEE proof was submitted directly — no pending ZK proof.
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "no pending ZK proof should exist after successful TEE submission"
    );
}

#[tokio::test]
async fn test_step_nullified_game_not_reprocessed() {
    // Simulate the post-nullification on-chain state: game is still
    // IN_PROGRESS but both teeProver and zkProver are address(0).
    // The scanner should filter it out and no proof should be initiated.
    let storage_hash = B256::repeat_byte(0xBB);
    let (header_15, account_15) = build_test_header_and_account(15, storage_hash);
    let root_15 =
        OutputRoot::from_parts(header_15.state_root, storage_hash, header_15.hash_slow()).hash();
    let (header_20, account_20) = build_test_header_and_account(20, storage_hash);

    let mut l2 = MockL2Provider::new();
    l2.insert_block(15, header_15, account_15);
    l2.insert_block(20, header_20, account_20);
    let l2 = Arc::new(l2);

    let factory = Arc::new(MockDisputeGameFactory { games: vec![factory_game(0, 1)] });
    let mut verifier_games = HashMap::new();
    verifier_games.insert(
        addr(0),
        MockGameState {
            status: 0,
            zk_prover: Address::ZERO,
            // Both provers zeroed — this is the state after TEE nullification.
            game_info: base_proof_contracts::GameInfo {
                root_claim: B256::repeat_byte(0x01),
                l2_block_number: 20,
                parent_index: 0,
            },
            starting_block_number: 10,
            l1_head: B256::repeat_byte(0xAA),
            intermediate_output_roots: vec![root_15, B256::repeat_byte(0xFF)],
            ..Default::default()
        },
    );
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    // Neither the ZK prover nor the tx manager should be called.
    let zk = Arc::new(MockZkProofProvider {
        session_id: "should-not-be-called".to_string(),
        ..Default::default()
    });
    let tx_manager = default_tx_manager();

    let mut driver = test_driver(factory, verifier, l2, zk, tx_manager);

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
    let tee_addr = Address::repeat_byte(0xEE);
    let zk_addr = Address::repeat_byte(0xCC);

    let (l2, factory, _verifier) = invalid_game_mocks();
    let mut game_state = mock_state_with_tee(0, zk_addr, tee_addr, 20);
    game_state.countered_index = 2; // challenged at 0-based index 1
    let verifier_games = HashMap::from([(addr(0), game_state)]);
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());
    driver.pending_proofs.insert(
        addr(0),
        PendingProof::ready(
            Bytes::from_static(&[0x01, 0xDE, 0xAD]),
            1,
            B256::repeat_byte(0xEE),
            default_prove_request(),
            DisputeIntent::Nullify,
        ),
    );

    driver.step().await.unwrap();

    // The pending proof should have been submitted (and removed), not dropped.
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "nullify intent should be submitted, not dropped due to zk_prover"
    );
}

#[tokio::test]
async fn test_poll_or_submit_challenge_intent_dropped_when_zk_prover_set() {
    // A pending proof with DisputeIntent::Challenge should be dropped
    // when zkProver is non-zero (game already challenged).
    let tee_addr = Address::repeat_byte(0xEE);
    let zk_addr = Address::repeat_byte(0xCC);

    let (l2, factory, _verifier) = invalid_game_mocks();
    let game_state = mock_state_with_tee(0, zk_addr, tee_addr, 20);
    let verifier_games = HashMap::from([(addr(0), game_state)]);
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    let tx = MockTxManager::new(Err(TxManagerError::NonceTooLow)); // Should never be called
    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), tx);
    driver.pending_proofs.insert(
        addr(0),
        PendingProof::ready(
            Bytes::from_static(&[0x01, 0xDE, 0xAD]),
            1,
            B256::repeat_byte(0xEE),
            default_prove_request(),
            DisputeIntent::Challenge,
        ),
    );

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
    let storage_hash = B256::repeat_byte(0xBB);
    let (header_15, account_15) = build_test_header_and_account(15, storage_hash);
    let root_15 =
        OutputRoot::from_parts(header_15.state_root, storage_hash, header_15.hash_slow()).hash();
    let (header_20, account_20) = build_test_header_and_account(20, storage_hash);
    let root_20 =
        OutputRoot::from_parts(header_20.state_root, storage_hash, header_20.hash_slow()).hash();

    let mut l2 = MockL2Provider::new();
    l2.insert_block(15, header_15, account_15);
    l2.insert_block(20, header_20, account_20);
    let l2 = Arc::new(l2);

    let tee_addr = Address::repeat_byte(0xEE);
    let zk_addr = Address::repeat_byte(0xCC);
    let factory = Arc::new(MockDisputeGameFactory { games: vec![factory_game(0, 1)] });

    let onchain_root_at_20 = if correct_root_at_20 { root_20 } else { B256::repeat_byte(0xFF) };

    let mut verifier_games = HashMap::new();
    verifier_games.insert(
        addr(0),
        MockGameState {
            status: 0,
            zk_prover: zk_addr,
            tee_prover: tee_addr,
            game_info: base_proof_contracts::GameInfo {
                root_claim: B256::repeat_byte(0x01),
                l2_block_number: 20,
                parent_index: 0,
            },
            starting_block_number: 10,
            l1_head: B256::repeat_byte(0xAA),
            intermediate_output_roots: vec![root_15, onchain_root_at_20],
            countered_index: 2, // 1-based → challenged_index = 1
            ..Default::default()
        },
    );
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    (l2, factory, verifier)
}

#[tokio::test]
async fn test_step_fraudulent_zk_challenge_legitimate_skips() {
    // The on-chain root at the challenged index is wrong, meaning the ZK
    // challenge was legitimate. The driver should skip without initiating
    // a proof.
    let (l2, factory, verifier) = fraudulent_zk_challenge_mocks(false);

    let zk = Arc::new(MockZkProofProvider {
        session_id: "should-not-be-called".to_string(),
        ..Default::default()
    });

    let mut driver = test_driver(factory, verifier, l2, zk, default_tx_manager());
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

    let zk = Arc::new(MockZkProofProvider {
        session_id: "nullify-fraudulent".to_string(),
        ..Default::default()
    });

    let mut driver = test_driver(factory, verifier, l2, zk, default_tx_manager());
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

// ──────────────────────────────────────────────────────────────────────────
// Path 3: Wrong ZK proposal → nullify with ZK
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_step_invalid_zk_proposal_initiates_zk_nullification() {
    // A game proposed with a ZK proof (tee_prover == ZERO, zk_prover != ZERO)
    // with invalid intermediate roots should trigger a ZK proof with
    // DisputeIntent::Nullify.
    let storage_hash = B256::repeat_byte(0xBB);
    let (header_15, _account_15) = build_test_header_and_account(15, storage_hash);
    let root_15 =
        OutputRoot::from_parts(header_15.state_root, storage_hash, header_15.hash_slow()).hash();
    let (header_20, account_20) = build_test_header_and_account(20, storage_hash);

    let mut l2 = MockL2Provider::new();
    l2.insert_block(15, header_15, account_20.clone());
    l2.insert_block(20, header_20, account_20);
    let l2 = Arc::new(l2);

    let zk_addr = Address::repeat_byte(0xCC);
    let factory = Arc::new(MockDisputeGameFactory { games: vec![factory_game(0, 1)] });
    let mut verifier_games = HashMap::new();
    verifier_games.insert(
        addr(0),
        MockGameState {
            status: 0,
            zk_prover: zk_addr,
            game_info: base_proof_contracts::GameInfo {
                root_claim: B256::repeat_byte(0x01),
                l2_block_number: 20,
                parent_index: 0,
            },
            starting_block_number: 10,
            l1_head: B256::repeat_byte(0xAA),
            intermediate_output_roots: vec![root_15, B256::repeat_byte(0xFF)],
            ..Default::default()
        },
    );
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    let zk = Arc::new(MockZkProofProvider {
        session_id: "zk-nullify-session".to_string(),
        ..Default::default()
    });

    let mut driver = test_driver(factory, verifier, l2, zk, default_tx_manager());
    driver.step().await.unwrap();

    // A pending proof should have been created with Nullify intent.
    let entry = driver.pending_proofs.get(&addr(0));
    assert!(entry.is_some(), "ZK nullification proof should be pending");
    let entry = entry.unwrap();
    assert_eq!(entry.intent, DisputeIntent::Nullify, "intent should be Nullify for ZK proposals");
}

#[tokio::test]
async fn test_step_valid_zk_proposal_skipped() {
    // A ZK-proposed game with valid intermediate roots should not trigger
    // any action.
    let factory = Arc::new(MockDisputeGameFactory { games: vec![factory_game(0, 1)] });
    let zk_addr = Address::repeat_byte(0xCC);
    let mut verifier_games = HashMap::new();
    verifier_games.insert(
        addr(0),
        MockGameState {
            status: 0,
            zk_prover: zk_addr,
            game_info: base_proof_contracts::GameInfo {
                root_claim: B256::repeat_byte(0x01),
                l2_block_number: 14,
                parent_index: 0,
            },
            starting_block_number: 10,
            l1_head: B256::repeat_byte(0xAA),
            ..Default::default()
        },
    );
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });
    let l2 = Arc::new(MockL2Provider::new());

    let zk = Arc::new(MockZkProofProvider {
        session_id: "should-not-be-called".to_string(),
        ..Default::default()
    });

    let mut driver = test_driver(factory, verifier, l2, zk, default_tx_manager());
    driver.step().await.unwrap();

    assert!(driver.pending_proofs.is_empty(), "valid ZK proposal should not trigger any proof");
}

// ──────────────────────────────────────────────────────────────────────────
// Bond lifecycle integration tests
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_bond_manager_full_lifecycle() {
    // Verify the full bond lifecycle: NeedsResolve → NeedsUnlock →
    // AwaitingDelay → NeedsWithdraw → Completed.
    //
    // The mock verifier uses a static game state, so we set
    // status=1 (CHALLENGER_WINS) to represent a game that has already been
    // resolved on-chain. The manager detects this and advances directly
    // to NeedsUnlock without submitting a resolve transaction.
    let claim_addr = Address::repeat_byte(0xCC);
    let game_addr = addr(0);
    let tx_hash = B256::repeat_byte(0xDD);

    // Set up verifier mock: status=1 (CHALLENGER_WINS), bond_unlocked=false,
    // bond_claimed=false.
    let mut verifier_games = HashMap::new();
    let mut state = mock_state(1, Address::ZERO, 100);
    state.bond_recipient = claim_addr;
    verifier_games.insert(game_addr, state);
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    // Mock submitter with enough responses for unlock + withdraw.
    // No resolve tx needed since the game is already resolved.
    let submitter = MockBondTransactionSubmitter::with_responses(vec![
        Ok(tx_hash), // claimCredit (unlock) tx
        Ok(tx_hash), // claimCredit (withdraw) tx
    ]);

    let mut mgr = BondManager::new(
        vec![claim_addr],
        "http://localhost:8545".parse().unwrap(),
        empty_factory(),
        1000,
    );
    mgr.set_weth_delay(Duration::from_secs(0)); // instant delay for testing

    // Track the game.
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
    for (target, _) in &calls {
        assert_eq!(*target, game_addr, "all transactions should target the game address");
    }
}

#[tokio::test]
async fn test_bond_manager_skips_already_resolved_game() {
    // When a game is already resolved on-chain (status != 0), the manager
    // should skip resolve and advance directly to NeedsUnlock.
    let claim_addr = Address::repeat_byte(0xCC);
    let game_addr = addr(0);
    let tx_hash = B256::repeat_byte(0xDD);

    let mut verifier_games = HashMap::new();
    let mut state = mock_state(1, Address::ZERO, 100); // status=1 (CHALLENGER_WINS)
    state.bond_recipient = claim_addr;
    verifier_games.insert(game_addr, state);
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    // Only need 2 responses: unlock + withdraw (no resolve since already resolved).
    let submitter = MockBondTransactionSubmitter::with_responses(vec![
        Ok(tx_hash), // claimCredit (unlock) tx
        Ok(tx_hash), // claimCredit (withdraw) tx
    ]);

    let mut mgr = BondManager::new(
        vec![claim_addr],
        "http://localhost:8545".parse().unwrap(),
        empty_factory(),
        1000,
    );
    mgr.set_weth_delay(Duration::from_secs(0));
    mgr.track_game(game_addr, claim_addr);

    // Poll 1: NeedsResolve → sees status != 0 → advances to NeedsUnlock (no tx).
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 1);

    // Poll 2: NeedsUnlock → submits unlock tx → AwaitingDelay.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 1);

    // Poll 3: AwaitingDelay (delay=0) → NeedsWithdraw.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 1);

    // Poll 4: NeedsWithdraw → submits withdraw tx → Completed → removed.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(mgr.tracked_count(), 0);

    // Only 2 transactions submitted (no resolve).
    assert_eq!(submitter.recorded_calls().len(), 2);
}

#[tokio::test]
async fn test_bond_manager_skips_already_unlocked_game() {
    // When the bond is already unlocked on-chain, the manager should skip
    // to AwaitingDelay.
    let claim_addr = Address::repeat_byte(0xCC);
    let game_addr = addr(0);
    let tx_hash = B256::repeat_byte(0xDD);

    let mut verifier_games = HashMap::new();
    let mut state = mock_state(1, Address::ZERO, 100);
    state.bond_recipient = claim_addr;
    state.bond_unlocked = true;
    state.resolved_at = 1_000_000;
    verifier_games.insert(game_addr, state);
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    // Only need 1 response: withdraw (resolve and unlock already done).
    let submitter = MockBondTransactionSubmitter::with_responses(vec![
        Ok(tx_hash), // claimCredit (withdraw) tx
    ]);

    let mut mgr = BondManager::new(
        vec![claim_addr],
        "http://localhost:8545".parse().unwrap(),
        empty_factory(),
        1000,
    );
    mgr.set_weth_delay(Duration::from_secs(0));
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
    // When the bond is already claimed on-chain, the manager should skip
    // and complete immediately.
    let claim_addr = Address::repeat_byte(0xCC);
    let game_addr = addr(0);

    let mut verifier_games = HashMap::new();
    let mut state = mock_state(1, Address::ZERO, 100);
    state.bond_recipient = claim_addr;
    state.bond_unlocked = true;
    state.bond_claimed = true;
    state.resolved_at = 1_000_000;
    verifier_games.insert(game_addr, state);
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    // No responses needed — no transactions should be submitted.
    let submitter = MockBondTransactionSubmitter::with_responses(vec![]);

    let mut mgr = BondManager::new(
        vec![claim_addr],
        "http://localhost:8545".parse().unwrap(),
        empty_factory(),
        1000,
    );
    mgr.set_weth_delay(Duration::from_secs(0));
    mgr.track_game(game_addr, claim_addr);

    // Poll 1: NeedsResolve → status != 0 → NeedsUnlock.
    mgr.poll(&*verifier, &submitter).await;

    // Poll 2: NeedsUnlock → bond_unlocked=true → AwaitingDelay.
    mgr.poll(&*verifier, &submitter).await;

    // Poll 3: AwaitingDelay (delay=0) → NeedsWithdraw.
    mgr.poll(&*verifier, &submitter).await;

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
    // When a bond transaction fails, the manager should retry on the next
    // poll tick. Tests claimCredit (unlock) failure and retry from the
    // NeedsUnlock phase.
    let claim_addr = Address::repeat_byte(0xCC);
    let game_addr = addr(0);
    let tx_hash = B256::repeat_byte(0xDD);

    let mut verifier_games = HashMap::new();
    let mut state = mock_state(1, Address::ZERO, 100); // status=1 (CHALLENGER_WINS)
    state.bond_recipient = claim_addr;
    verifier_games.insert(game_addr, state);
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    // First claimCredit attempt fails, second succeeds.
    let submitter = MockBondTransactionSubmitter::with_responses(vec![
        Err(base_tx_manager::TxManagerError::NonceTooLow.into()),
        Ok(tx_hash), // retry succeeds
    ]);

    let mut mgr = BondManager::new(
        vec![claim_addr],
        "http://localhost:8545".parse().unwrap(),
        empty_factory(),
        1000,
    );
    mgr.set_weth_delay(Duration::from_secs(0));
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
    // Games whose bondRecipient is not in the claim addresses should not
    // be tracked.
    let claim_addr = Address::repeat_byte(0xCC);
    let other_addr = Address::repeat_byte(0xDD);
    let game_addr = addr(0);

    let mut mgr = BondManager::new(
        vec![claim_addr],
        "http://localhost:8545".parse().unwrap(),
        empty_factory(),
        1000,
    );
    assert!(!mgr.track_game(game_addr, other_addr));
    assert_eq!(mgr.tracked_count(), 0);
}

#[tokio::test]
async fn test_bond_manager_keeps_defender_wins_when_recipient_is_claimable() {
    // When a game resolves as DEFENDER_WINS and the on-chain bondRecipient
    // is in our claim addresses (i.e. we are the proposer), the manager
    // should keep the game and advance it to NeedsUnlock — the bond is ours.
    let claim_addr = Address::repeat_byte(0xCC);
    let game_addr = addr(0);

    let mut verifier_games = HashMap::new();
    let mut state = mock_state(2, Address::ZERO, 100); // status=2 (DEFENDER_WINS)
    state.bond_recipient = claim_addr;
    verifier_games.insert(game_addr, state);
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    let submitter = MockBondTransactionSubmitter::with_responses(vec![]);

    let mut mgr = BondManager::new(
        vec![claim_addr],
        "http://localhost:8545".parse().unwrap(),
        empty_factory(),
        1000,
    );
    mgr.set_weth_delay(Duration::from_secs(0));
    mgr.track_game(game_addr, claim_addr);
    assert_eq!(mgr.tracked_count(), 1);

    // Poll 1: NeedsResolve → status=2 (already resolved) → bondRecipient
    // is in claim set → advance to NeedsUnlock (not removed).
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(
        mgr.tracked_count(),
        1,
        "game should be kept when bondRecipient is in claim addresses"
    );
}

#[tokio::test]
async fn test_bond_manager_removes_game_when_recipient_not_claimable() {
    // When a game is resolved and the on-chain bondRecipient is NOT in our
    // claim addresses, the manager should remove the game from tracking.
    // This covers the case where we matched via zkProver during the startup
    // scan, but after resolve the bond goes to someone else.
    let claim_addr = Address::repeat_byte(0xCC);
    let other_addr = Address::repeat_byte(0xDD);
    let game_addr = addr(0);

    let mut verifier_games = HashMap::new();
    let mut state = mock_state(2, Address::ZERO, 100); // status=2 (DEFENDER_WINS)
    state.bond_recipient = other_addr; // bond goes to someone else
    verifier_games.insert(game_addr, state);
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    // No responses needed — no transactions should be submitted.
    let submitter = MockBondTransactionSubmitter::with_responses(vec![]);

    let mut mgr = BondManager::new(
        vec![claim_addr],
        "http://localhost:8545".parse().unwrap(),
        empty_factory(),
        1000,
    );
    mgr.set_weth_delay(Duration::from_secs(0));
    mgr.track_game(game_addr, claim_addr);
    assert_eq!(mgr.tracked_count(), 1);

    // Poll 1: NeedsResolve → already resolved → bondRecipient not in
    // claim set → removed from tracking.
    mgr.poll(&*verifier, &submitter).await;
    assert_eq!(
        mgr.tracked_count(),
        0,
        "game should be removed when bondRecipient is not in claim addresses"
    );

    assert!(
        submitter.recorded_calls().is_empty(),
        "no transactions should be submitted when bond is not claimable"
    );
}

#[tokio::test]
async fn test_driver_tracks_bond_after_successful_challenge() {
    // After a successful challenge() submission, the driver should register
    // the game with the bond manager.
    let (l2, factory, verifier) = invalid_game_mocks();
    let sender_addr = Address::ZERO; // MockTxManager returns ZERO as sender_address

    let zk = Arc::new(MockZkProofProvider {
        session_id: "bond-track".to_string(),
        proof_status: Mutex::new(base_zk_client::ProofJobStatus::Succeeded as i32),
        receipt: Mutex::new(vec![0xDE, 0xAD]),
        ..Default::default()
    });

    let tx_hash = B256::repeat_byte(0xCC);
    let tx_manager = MockTxManager::new(Ok(receipt_with_status(true, tx_hash)));

    let scanner = GameScanner::new(
        Arc::clone(&factory) as Arc<dyn base_proof_contracts::DisputeGameFactoryClient>,
        Arc::clone(&verifier) as Arc<dyn AggregateVerifierClient>,
        ScannerConfig { lookback_games: 1000 },
    );
    let validator = OutputValidator::new(l2);
    let submitter = ChallengeSubmitter::new(tx_manager);

    let config = DriverConfig {
        poll_interval: Duration::from_millis(10),
        cancel: CancellationToken::new(),
        ready: Arc::new(AtomicBool::new(false)),
    };

    // Bond manager with the sender address as a claim address.
    let mut bond_manager = BondManager::new(
        vec![sender_addr],
        "http://localhost:8545".parse().unwrap(),
        empty_factory(),
        1000,
    );
    bond_manager.set_weth_delay(Duration::from_secs(3600));

    let mut driver = Driver::new(
        config,
        DriverComponents {
            scanner,
            validator,
            zk_prover: zk,
            submitter,
            tee: None,
            verifier_client: verifier as Arc<dyn AggregateVerifierClient>,
            bond_manager: Some(bond_manager),
        },
    );

    // Step 1: proof initiated, not yet polled.
    driver.step().await.unwrap();
    assert!(
        driver.pending_proofs.contains_key(&addr(0)),
        "proof should be pending after initiation"
    );

    // Step 2: proof polled → Succeeded → challenge tx submitted → bond tracked.
    driver.step().await.unwrap();

    // After a successful challenge, the bond_manager should be tracking the game.
    let bond_mgr = driver.bond_manager.as_ref().expect("bond_manager should be Some");
    assert!(
        bond_mgr.is_tracking(&addr(0)),
        "game should be tracked by bond manager after successful challenge"
    );
}
