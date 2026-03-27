//! Integration tests for the challenger [`Driver`] loop.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Duration,
};

use alloy_primitives::{Address, B256, Bytes, U256};
use base_challenger::{
    ChallengeSubmitter, Driver, DriverConfig, GameScanner, L1HeadProvider, OutputValidator,
    PendingProof, ProofPhase, ScannerConfig, TeeConfig, derive_session_id,
    test_utils::{
        MockAggregateVerifier, MockDisputeGameFactory, MockGameState, MockL1HeadProvider,
        MockL2Provider, MockTeeProofProvider, MockTxManager, MockZkProofProvider, addr,
        build_test_header_and_account, factory_game, mock_state, receipt_with_status,
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
        scanner,
        validator,
        zk_prover,
        submitter,
        tee,
        verifier as Arc<dyn AggregateVerifierClient>,
    )
}

fn default_zk_prover() -> Arc<MockZkProofProvider> {
    Arc::new(MockZkProofProvider { session_id: "test-session".to_string(), ..Default::default() })
}

fn default_tx_manager() -> MockTxManager {
    MockTxManager::new(Ok(receipt_with_status(true, B256::repeat_byte(0xAA))))
}

fn default_prove_request() -> ProveBlockRequest {
    let session_id = derive_session_id(addr(0), 1);

    ProveBlockRequest {
        start_block_number: 15,
        number_of_blocks_to_prove: 5,
        sequence_window: None,
        proof_type: ProofType::GenericZkvmClusterSnarkGroth16.into(),
        session_id: Some(session_id),
        prover_address: Some(format!("{:#x}", addr(0))),
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
    verifier_games.insert(
        addr(0),
        MockGameState {
            status: 0,
            zk_prover: Address::ZERO,
            tee_prover: Address::ZERO,
            game_info: base_proof_contracts::GameInfo {
                root_claim: B256::repeat_byte(0x01),
                l2_block_number: 20,
                parent_index: 0,
            },
            starting_block_number: 10,
            intermediate_output_roots: vec![root_15, B256::repeat_byte(0xFF)],
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
    verifier_games.insert(
        addr(0),
        MockGameState {
            status: 0,
            zk_prover: Address::ZERO,
            tee_prover: Address::ZERO,
            game_info: base_proof_contracts::GameInfo {
                root_claim: B256::repeat_byte(0x01),
                l2_block_number: 14,
                parent_index: 0,
            },
            starting_block_number: 10,
            intermediate_output_roots: vec![],
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
    verifier_games.insert(
        addr(0),
        MockGameState {
            status: 0,
            zk_prover: Address::ZERO,
            tee_prover: Address::ZERO,
            game_info: base_proof_contracts::GameInfo {
                root_claim: B256::repeat_byte(0x01),
                l2_block_number: 20,
                parent_index: 0,
            },
            starting_block_number: 10,
            intermediate_output_roots: vec![B256::repeat_byte(0xFF), B256::repeat_byte(0xEE)],
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

    driver.step().await.unwrap();
    // The tx_manager response was consumed → nullification was submitted.
    // If it wasn't consumed, the next call would panic.
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

    // step succeeds — proof failure triggers re-initiation via handle_proof_retry
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
    verifier_games.insert(
        addr(0),
        MockGameState {
            status: 0,
            zk_prover: Address::ZERO,
            tee_prover: Address::ZERO,
            game_info: base_proof_contracts::GameInfo {
                root_claim: B256::repeat_byte(0x01),
                l2_block_number: 20,
                parent_index: 0,
            },
            starting_block_number: 10,
            // 2 roots expected at interval=5, provide 2 so count matches
            intermediate_output_roots: vec![B256::ZERO, B256::ZERO],
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
        scanner,
        validator,
        default_zk_prover(),
        submitter,
        None,
        verifier as Arc<dyn AggregateVerifierClient>,
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
    // nullification submitted, session removed from pending_proofs.
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "session should be removed after proof succeeded"
    );
}

#[tokio::test]
async fn test_step_nullification_failure_preserves_proof() {
    // Proof succeeds on first step but nullification tx fails.
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

    // Step 1: proof succeeds, but nullification tx fails.
    // initiate_proof catches the poll_or_submit error and logs a warning,
    // so the error does not propagate up through process_candidate → step.
    driver.step().await.unwrap();

    // Entry must still be in pending_proofs as ReadyToSubmit.
    let entry = driver.pending_proofs.get(&addr(0)).expect("proof should be preserved");
    assert!(entry.is_ready(), "phase should be ReadyToSubmit after tx failure");

    // Step 2: poll_pending_proofs re-submits, now the tx succeeds.
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
        scanner,
        validator,
        default_zk_prover(),
        submitter,
        None,
        verifier as Arc<dyn AggregateVerifierClient>,
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
    // Proof fails on first tick (NeedsRetry), then re-initiated prove_block
    // returns a new session. On the next tick the proof succeeds and
    // nullification is submitted.
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

    // Step 1: proof initiated then immediately fails → NeedsRetry.
    // Then handle_proof_retry re-initiates prove_block → AwaitingProof.
    driver.step().await.unwrap();
    let entry = driver.pending_proofs.get(&addr(0)).expect("entry should exist");
    assert!(
        matches!(entry.phase, ProofPhase::AwaitingProof { .. }),
        "phase should be AwaitingProof after retry re-initiation"
    );
    assert_eq!(entry.retry_count, 1);

    // Simulate proof succeeding on the retry session.
    *zk.proof_status.lock().unwrap() = ProofJobStatus::Succeeded as i32;

    // Step 2: proof succeeds, nullification submitted, entry removed.
    driver.step().await.unwrap();
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "entry should be removed after successful nullification"
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

    // Each step: poll returns Failed → NeedsRetry (retry_count increments),
    // then handle_proof_retry re-initiates → AwaitingProof.
    // After MAX_PROOF_RETRIES + 1 total failures the entry is dropped.
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
fn invalid_game_mocks_with_tee()
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
            intermediate_output_roots: vec![root_15, B256::repeat_byte(0xFF)],
        },
    );
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    (l2, factory, verifier)
}

#[tokio::test]
async fn test_step_invalid_game_tee_fails_zk_fallback() {
    // Game has a TEE prover, TEE provider is configured, but the TEE proof
    // attempt fails (L1 provider is unreachable with dummy). The driver
    // should fall back to ZK and initiate a ZK proof session.
    let (l2, factory, verifier) = invalid_game_mocks_with_tee();

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
async fn test_step_invalid_game_no_tee_prover_zk_only() {
    // Game has `tee_prover == ZERO`. Even though a TEE provider is
    // configured, the TEE path should NOT be attempted. Goes straight to ZK.
    let (l2, factory, verifier) = invalid_game_mocks();

    // This TEE provider should never be called since tee_prover is ZERO.
    let tee = Arc::new(MockTeeProofProvider::failure("should not be called"));
    let zk =
        Arc::new(MockZkProofProvider { session_id: "zk-direct".to_string(), ..Default::default() });

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

    // The ZK proof should be initiated directly (no TEE attempt since
    // tee_prover == ZERO).
    let entry = driver.pending_proofs.get(&addr(0)).expect("ZK proof should be pending");
    assert!(
        matches!(entry.phase, ProofPhase::AwaitingProof { .. }),
        "phase should be AwaitingProof (direct ZK)"
    );
}

#[tokio::test]
async fn test_step_invalid_game_no_tee_provider_zk_only() {
    // Game has a TEE prover, but the driver has no TEE provider configured
    // (tee-rpc-url was not set). Should go straight to ZK.
    let (l2, factory, verifier) = invalid_game_mocks_with_tee();

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
    // ZK, ZK proof succeeds immediately, nullification submitted.
    let (l2, factory, verifier) = invalid_game_mocks_with_tee();

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

    // Step: TEE path is attempted (fails due to provider error), falls back to
    // ZK, proof succeeds immediately, nullification submitted.
    driver.step().await.unwrap();

    // Proof was submitted and removed from pending.
    assert!(
        !driver.pending_proofs.contains_key(&addr(0)),
        "entry should be removed after successful ZK fallback submission"
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
            // root_15 is correct, index 1 is bogus — invalid_index == 1
            intermediate_output_roots: vec![root_15, B256::repeat_byte(0xFF)],
        },
    );
    let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });

    let l1_hash = B256::repeat_byte(0xAA);
    let l1_head = Arc::new(MockL1HeadProvider::success(l1_hash, 100));

    let aggregate_proposal = Proposal {
        output_root: root_20,
        signature: Bytes::from(vec![0u8; 65]),
        l1_origin_hash: l1_hash,
        l1_origin_number: U256::from(1000),
        l2_block_number: U256::from(20),
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
