//! Integration tests for the challenger [`Driver`] loop.

use std::{
    collections::HashMap,
    num::NonZeroU64,
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_primitives::{Address, B256, Bytes};
use base_challenger::{
    AnchorUpdater, ChallengeSubmitter, ChallengerProofAdapter, DisputeIntent, DisputeProofManager,
    Driver, DriverComponents, GameScanner, OutputValidator, PendingProof, ProofKind, ProofPhase,
    test_utils::{
        DEFAULT_L1_HEAD, DEFAULT_TEE_PROVER, MockAggregateVerifier, MockDisputeGameFactory,
        MockGameState, MockL1, MockL2Provider, MockTxManager, MockZkProofProvider,
        MockZkProofState, addr, build_test_header_and_account, factory_game, mock_anchor_registry,
        mock_state, mock_state_with_tee, receipt_with_status,
    },
};
use base_proof_contracts::{
    AggregateVerifierClient, DisputeGameFactoryClient, DisputeKind, GameStatus,
    decode_dispute_calldata, invalid_proof_selector,
};
use base_proof_primitives::Proposal;
use base_proof_rpc::L1Provider;
use base_proof_submission::test_utils::SnarkReceiptFixture;
use base_protocol::OutputRoot;
use base_prover_service_protocol::{
    ProofRequestKind, ProofResult as ApiProofResult, ProofStatus, SnarkPlonkProofRequest, TeeKind,
    TeeProofResult, ZkBackend, ZkProofRequest, ZkVm,
};
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

/// Builds a test driver with the given mocks.
fn test_driver(
    factory: Arc<dyn DisputeGameFactoryClient>,
    verifier: Arc<MockAggregateVerifier>,
    l2_provider: Arc<MockL2Provider>,
    zk_prover: Arc<MockZkProofProvider>,
    tx_manager: MockTxManager,
) -> Driver<MockL2Provider, MockZkProofProvider, MockTxManager> {
    test_driver_with_l1_provider(
        factory,
        verifier,
        l2_provider,
        zk_prover,
        tx_manager,
        Arc::new(MockL1::failure("unused")),
    )
}

/// Builds a test driver with an L1 provider for TEE proof requests.
fn test_driver_with_l1_provider(
    factory: Arc<dyn DisputeGameFactoryClient>,
    verifier: Arc<MockAggregateVerifier>,
    l2_provider: Arc<MockL2Provider>,
    zk_prover: Arc<MockZkProofProvider>,
    tx_manager: MockTxManager,
    l1_provider: Arc<dyn L1Provider>,
) -> Driver<MockL2Provider, MockZkProofProvider, MockTxManager> {
    test_driver_with_timeout(
        factory,
        verifier,
        l2_provider,
        zk_prover,
        tx_manager,
        l1_provider,
        Duration::from_secs(4 * 60 * 60),
    )
}

fn test_driver_with_timeout(
    factory: Arc<dyn DisputeGameFactoryClient>,
    verifier: Arc<MockAggregateVerifier>,
    l2_provider: Arc<MockL2Provider>,
    zk_prover: Arc<MockZkProofProvider>,
    tx_manager: MockTxManager,
    l1_provider: Arc<dyn L1Provider>,
    max_proof_duration: Duration,
) -> Driver<MockL2Provider, MockZkProofProvider, MockTxManager> {
    let anchor_registry = mock_anchor_registry(Address::ZERO);
    let scanner = GameScanner::new(
        Arc::clone(&factory),
        Arc::clone(&verifier) as Arc<dyn AggregateVerifierClient>,
        Arc::clone(&anchor_registry),
    );
    let validator = OutputValidator::new(Arc::clone(&l2_provider));
    let submitter = ChallengeSubmitter::new(tx_manager);

    Driver::new(DriverComponents {
        scanner,
        validator,
        proof_requester: zk_prover,
        submitter,
        l1_provider,
        verifier_client: verifier as Arc<dyn AggregateVerifierClient>,
        bond_manager: None,
        anchor_updater: AnchorUpdater::new(
            factory,
            anchor_registry,
            l2_provider as Arc<dyn base_proof_rpc::L2Provider>,
            Address::repeat_byte(0xAA),
            1,
        ),
        poll_interval: Duration::from_millis(10),
        max_proof_duration,
        tee_submit_retry_limit: 3,
        cancel: CancellationToken::new(),
    })
}

const fn tee_proposal(output_root: B256, prev_output_root: B256) -> Proposal {
    Proposal {
        output_root,
        signature: Bytes::from_static(&[0; 65]),
        l1_origin_hash: DEFAULT_L1_HEAD,
        l1_origin_number: 1000,
        l2_block_number: 20,
        prev_output_root,
        config_hash: B256::ZERO,
        schedule_id: B256::ZERO,
    }
}

fn default_zk_prover() -> Arc<MockZkProofProvider> {
    Arc::new(MockZkProofProvider::default())
}

fn default_tx_manager() -> MockTxManager {
    MockTxManager::new(Ok(receipt_with_status(true, B256::repeat_byte(0xAA))))
}

fn default_l2() -> Arc<MockL2Provider> {
    Arc::new(MockL2Provider::default())
}

fn single_game_factory() -> Arc<MockDisputeGameFactory> {
    Arc::new(MockDisputeGameFactory::new(vec![factory_game(0, 1)]))
}

fn single_game_verifier(state: MockGameState) -> Arc<MockAggregateVerifier> {
    Arc::new(MockAggregateVerifier::new(HashMap::from([(addr(0), state)])))
}

fn default_ready_proof(intent: DisputeIntent) -> PendingProof {
    let request = SnarkPlonkProofRequest {
        proof: ZkProofRequest {
            start_block_number: 15,
            number_of_blocks_to_prove: 5,
            sequence_window: None,
            l1_head: Some(DEFAULT_L1_HEAD),
            schedule_l2_block_number: None,
            intermediate_block_interval: NonZeroU64::new(5).unwrap(),
            zk_vm: ZkVm::Sp1,
            zk_backend: ZkBackend::Cluster,
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

/// Bincode SNARK receipt fixture (matches prover-service download payloads).
fn snark_receipt_bytes() -> Vec<u8> {
    SnarkReceiptFixture::plonk_receipt_bytes([0x5a, 0x09, 0x3a, 0x2f], "dead")
}

fn succeeded_zk_prover() -> Arc<MockZkProofProvider> {
    Arc::new(MockZkProofProvider {
        state: Mutex::new(MockZkProofState {
            proof_status: ProofStatus::Succeeded,
            proof: snark_receipt_bytes(),
            ..Default::default()
        }),
    })
}

fn succeeded_tee_prover(aggregate_proposal: Proposal) -> Arc<MockZkProofProvider> {
    Arc::new(MockZkProofProvider {
        state: Mutex::new(MockZkProofState {
            proof_status: ProofStatus::Succeeded,
            result: Some(ApiProofResult::Tee(TeeProofResult {
                intermediate_roots: vec![aggregate_proposal.output_root],
                aggregate_proposal,
                tee_kind: TeeKind::AwsNitro,
                tee_signer: Address::repeat_byte(0x11),
            })),
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

    let mut l2 = MockL2Provider::default();
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

/// Stages a ready challenge for a game with the given verifier state.
fn driver_with_ready_proof(
    game_state: MockGameState,
    tx: MockTxManager,
) -> Driver<MockL2Provider, MockZkProofProvider, MockTxManager> {
    let factory = single_game_factory();
    let verifier = single_game_verifier(game_state);
    let l2 = default_l2();
    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), tx);
    driver
        .proof_manager
        .pending_proofs_mut()
        .insert(addr(0), default_ready_proof(DisputeIntent::Challenge));
    driver
}

fn sent_disputes(tx: &MockTxManager) -> Vec<(Address, DisputeKind)> {
    tx.recorded_calls()
        .iter()
        .map(|call| {
            let dispute = decode_dispute_calldata(&call.tx_data).expect("dispute calldata");
            (call.to.expect("game destination"), dispute.kind)
        })
        .collect()
}

fn assert_zk_requests(zk: &MockZkProofProvider, count: usize) {
    let state = zk.state.lock().unwrap();
    assert_eq!(state.prove_block_range_log.len(), count);
    for request in &state.prove_block_range_log {
        assert_eq!(
            request.proof.session_id,
            ChallengerProofAdapter::snark_plonk_session_id(addr(0), 1)
        );
        assert!(request.retry_failed);
        let ProofRequestKind::SnarkPlonk(request) = &request.proof.request else {
            panic!("expected ZK request");
        };
        assert_eq!(
            request.proof,
            ZkProofRequest {
                start_block_number: 15,
                number_of_blocks_to_prove: 5,
                sequence_window: None,
                l1_head: Some(DEFAULT_L1_HEAD),
                schedule_l2_block_number: Some(20),
                intermediate_block_interval: NonZeroU64::new(5).unwrap(),
                zk_vm: ZkVm::Sp1,
                zk_backend: ZkBackend::Cluster,
            }
        );
        assert_eq!(request.prover_address, Address::ZERO);
    }
}

fn assert_tee_then_zk_requests(zk: &MockZkProofProvider) {
    let state = zk.state.lock().unwrap();
    assert_eq!(state.prove_block_range_log.len(), 2);
    assert!(matches!(state.prove_block_range_log[0].proof.request, ProofRequestKind::Tee(_)));
    assert!(matches!(
        state.prove_block_range_log[1].proof.request,
        ProofRequestKind::SnarkPlonk(_)
    ));
}

fn complete_zk_proof(zk: &MockZkProofProvider) {
    let mut state = zk.state.lock().unwrap();
    state.result = None;
    state.proof = snark_receipt_bytes();
    state.proof_status = ProofStatus::Succeeded;
    state.error_message = None;
}

#[tokio::test]
async fn test_step_valid_proposals_are_skipped() {
    for game_state in [
        MockGameState { tee_prover: DEFAULT_TEE_PROVER, ..game_state(14) },
        MockGameState { zk_prover: ZK_PROVER_ADDR, ..game_state(14) },
        MockGameState {
            tee_prover: DEFAULT_TEE_PROVER,
            zk_prover: ZK_PROVER_ADDR,
            ..game_state(14)
        },
    ] {
        let mut driver = test_driver(
            single_game_factory(),
            single_game_verifier(game_state),
            default_l2(),
            default_zk_prover(),
            default_tx_manager(),
        );

        driver.step().await.unwrap();

        assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
    }
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

    let mut l2 = MockL2Provider::default();
    l2.error_blocks.push(15);
    l2.error_blocks.push(20);
    let l2 = Arc::new(l2);

    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());

    driver.step().await.unwrap();
}

#[tokio::test]
async fn test_step_invalid_game_submits_zk_challenge() {
    let (l2, factory, verifier) = invalid_game_mocks();
    let zk = succeeded_zk_prover();
    let tx = default_tx_manager();
    let mut driver = test_driver(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        verifier,
        l2,
        Arc::<MockZkProofProvider>::clone(&zk),
        tx.clone(),
    );

    driver.step().await.unwrap();
    assert_zk_requests(&zk, 1);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 1);
    assert!(tx.recorded_calls().is_empty());

    // Keep the game in progress for submission without rediscovering it afterward.
    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();
    assert_eq!(sent_disputes(&tx), vec![(addr(0), DisputeKind::Challenge)]);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
}

#[tokio::test]
async fn test_step_resolved_game_drops_completed_challenge() {
    let (l2, factory, verifier) = invalid_game_mocks();
    let zk = succeeded_zk_prover();
    let tx = default_tx_manager();
    let mut driver = test_driver(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        Arc::clone(&verifier),
        l2,
        Arc::clone(&zk),
        tx.clone(),
    );

    driver.step().await.unwrap();
    assert_zk_requests(&zk, 1);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 1);
    assert!(sent_disputes(&tx).is_empty());

    verifier.games.lock().unwrap().get_mut(&addr(0)).unwrap().status = GameStatus::ChallengerWins;
    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();

    assert!(sent_disputes(&tx).is_empty());
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
    assert_zk_requests(&zk, 1);
}

#[tokio::test]
async fn test_step_failed_submission_reuses_completed_proof() {
    let (l2, factory, verifier) = invalid_game_mocks();
    let zk = succeeded_zk_prover();
    let tx = MockTxManager::with_responses(vec![
        Err(TxManagerError::NonceTooLow),
        Ok(receipt_with_status(true, DEFAULT_TX_HASH)),
    ]);
    let mut driver = test_driver(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        verifier,
        l2,
        Arc::<MockZkProofProvider>::clone(&zk),
        tx.clone(),
    );

    driver.step().await.unwrap();
    assert!(tx.recorded_calls().is_empty());
    driver.step().await.unwrap();
    assert_eq!(sent_disputes(&tx), vec![(addr(0), DisputeKind::Challenge)]);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 1);

    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();
    assert_eq!(sent_disputes(&tx), vec![(addr(0), DisputeKind::Challenge); 2]);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
    assert_zk_requests(&zk, 1);
}

#[tokio::test]
async fn test_poll_or_submit_drops_stale_challenge_proofs() {
    for game_state in [
        mock_state(GameStatus::ChallengerWins, Address::ZERO, 20),
        mock_state(GameStatus::InProgress, ZK_PROVER_ADDR, 20),
        mock_state_with_tee(GameStatus::InProgress, Address::ZERO, Address::ZERO, 20),
    ] {
        let tx = default_tx_manager();
        let mut driver = driver_with_ready_proof(game_state, tx.clone());
        driver.step().await.unwrap();
        assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
        assert!(tx.recorded_calls().is_empty());
    }
}

#[tokio::test]
async fn test_run_cancellation() {
    let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
    let verifier = Arc::new(MockAggregateVerifier::new(HashMap::new()));
    let l2 = default_l2();

    let driver = test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());
    driver.cancel.cancel();

    tokio::time::timeout(Duration::from_secs(2), driver.run())
        .await
        .expect("driver.run() should exit promptly after cancellation");
}

#[tokio::test]
async fn test_step_proof_retry_reuses_deterministic_session_id() {
    let (l2, factory, verifier) = invalid_game_mocks();
    let zk = Arc::new(MockZkProofProvider {
        state: Mutex::new(MockZkProofState {
            proof_status: ProofStatus::Failed,
            error_message: Some("transient backend error".into()),
            ..Default::default()
        }),
    });
    let tx = default_tx_manager();
    let mut driver = test_driver(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        verifier,
        l2,
        Arc::<MockZkProofProvider>::clone(&zk),
        tx.clone(),
    );

    // The prover service requeues a failed row only for the same deterministic session ID.
    driver.step().await.unwrap();
    assert_zk_requests(&zk, 1);
    driver.step().await.unwrap();
    assert_zk_requests(&zk, 2);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 1);
    assert!(tx.recorded_calls().is_empty());

    complete_zk_proof(&zk);
    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();
    assert_eq!(sent_disputes(&tx), vec![(addr(0), DisputeKind::Challenge)]);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
    assert_zk_requests(&zk, 2);
}

#[tokio::test]
async fn test_step_proof_exceeds_max_retries() {
    let (l2, factory, verifier) = invalid_game_mocks();
    let zk = Arc::new(MockZkProofProvider {
        state: Mutex::new(MockZkProofState {
            proof_status: ProofStatus::Failed,
            ..Default::default()
        }),
    });
    let tx = default_tx_manager();
    let mut driver = test_driver(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        verifier,
        l2,
        Arc::<MockZkProofProvider>::clone(&zk),
        tx.clone(),
    );

    driver.step().await.unwrap();
    assert_zk_requests(&zk, 1);
    let max_retries = DisputeProofManager::<MockL2Provider, MockZkProofProvider>::MAX_PROOF_RETRIES;
    for i in 0..max_retries {
        driver.step().await.unwrap();
        assert_zk_requests(&zk, i as usize + 2);
        assert_eq!(driver.proof_manager.pending_proofs_len(), 1);
        assert!(tx.recorded_calls().is_empty());
    }

    // Prevent rediscovery so the exhausted attempt's removal is observable.
    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();
    assert_zk_requests(&zk, 1 + max_retries as usize);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
    assert!(tx.recorded_calls().is_empty());
}

// ── TEE-first proof sourcing tests ─────────────────────────────────────────

#[tokio::test]
async fn test_step_invalid_game_tee_fails_zk_succeeds() {
    let (l2, factory, verifier) = invalid_game_mocks();
    let zk = succeeded_zk_prover();
    let tx = default_tx_manager();
    let l1 = Arc::new(MockL1::failure("unavailable"));
    let mut driver = test_driver_with_l1_provider(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        verifier,
        l2,
        Arc::<MockZkProofProvider>::clone(&zk),
        tx.clone(),
        Arc::<MockL1>::clone(&l1),
    );

    driver.step().await.unwrap();
    assert_zk_requests(&zk, 1);
    assert_eq!(*l1.header_by_hash_requests.lock().unwrap(), vec![DEFAULT_L1_HEAD]);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 1);
    assert!(tx.recorded_calls().is_empty());

    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();
    assert_eq!(sent_disputes(&tx), vec![(addr(0), DisputeKind::Challenge)]);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
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

    let l1_head = Arc::new(MockL1::success(DEFAULT_L1_HEAD, 100));

    let aggregate_proposal = tee_proposal(root_20, root_15);
    let proof_requester = succeeded_tee_prover(aggregate_proposal);

    let tx_manager = default_tx_manager();

    let mut driver = test_driver_with_l1_provider(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        Arc::clone(&verifier),
        l2,
        Arc::clone(&proof_requester),
        tx_manager.clone(),
        l1_head,
    );

    driver.step().await.unwrap();
    {
        let prove_block_range_log = &proof_requester.state.lock().unwrap().prove_block_range_log;
        assert_eq!(prove_block_range_log.len(), 1, "TEE proof should be requested once");
        assert!(
            matches!(
                &prove_block_range_log[0].proof.request,
                ProofRequestKind::Tee(request) if request.proof.l1_head_number == 100
            ),
            "TEE request must include the resolved L1 head number",
        );
    }
    assert!(tx_manager.recorded_calls().is_empty());
    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();
    assert_eq!(sent_disputes(&tx_manager), vec![(addr(0), DisputeKind::Nullify)]);
    assert_eq!(proof_requester.state.lock().unwrap().prove_block_range_log.len(), 1);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
}

#[tokio::test]
async fn test_step_nullified_tee_prover_drops_completed_nullification() {
    let (l2, factory, root_15, root_20) = base_game_mocks();
    let verifier = single_game_verifier(MockGameState {
        tee_prover: DEFAULT_TEE_PROVER,
        intermediate_output_roots: vec![root_15, BOGUS_ROOT],
        ..game_state(20)
    });
    let prover = succeeded_tee_prover(tee_proposal(root_20, root_15));
    let tx = default_tx_manager();
    let mut driver = test_driver_with_l1_provider(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        Arc::clone(&verifier),
        l2,
        Arc::clone(&prover),
        tx.clone(),
        Arc::new(MockL1::success(DEFAULT_L1_HEAD, 100)),
    );

    driver.step().await.unwrap();
    {
        let state = prover.state.lock().unwrap();
        assert_eq!(state.prove_block_range_log.len(), 1);
        assert!(matches!(state.prove_block_range_log[0].proof.request, ProofRequestKind::Tee(_)));
    }
    assert_eq!(driver.proof_manager.pending_proofs_len(), 1);
    assert!(sent_disputes(&tx).is_empty());

    verifier.games.lock().unwrap().get_mut(&addr(0)).unwrap().tee_prover = Address::ZERO;
    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();

    assert!(sent_disputes(&tx).is_empty());
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
    assert_eq!(prover.state.lock().unwrap().prove_block_range_log.len(), 1);
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

    let l1_head = Arc::new(MockL1::success(DEFAULT_L1_HEAD, 100));

    let aggregate_proposal = tee_proposal(root_20, root_15);
    let zk = succeeded_tee_prover(aggregate_proposal);

    // TEE nullify() tx reverts, ZK challenge() tx succeeds.
    let tx_manager = MockTxManager::with_responses(vec![
        Err(TxManagerError::ExecutionReverted {
            reason: Some("unexpected contract revert".to_string()),
            data: None,
        }),
        Ok(receipt_with_status(true, DEFAULT_TX_HASH)),
    ]);

    let mut driver = test_driver_with_l1_provider(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        Arc::clone(&verifier),
        l2,
        Arc::clone(&zk),
        tx_manager.clone(),
        l1_head,
    );

    // Step 1: TEE proof job is initiated.
    driver.step().await.unwrap();

    // Step 2: TEE proof is polled, nullify() tx fails, falls back to ZK.
    driver.step().await.unwrap();

    assert_tee_then_zk_requests(&zk);
    assert_eq!(sent_disputes(&tx_manager), vec![(addr(0), DisputeKind::Nullify)]);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 1);

    complete_zk_proof(&zk);

    // Keep the game in-progress so the pending fallback proof reaches
    // submit_dispute(), but remove it from the scan batch so the same tick
    // cannot re-discover it after the successful fallback submission.
    factory.games.lock().unwrap().clear();

    // Step 3: ZK proof polled → Succeeded → challenge tx submitted → entry cleaned up.
    driver.step().await.unwrap();
    assert_eq!(
        sent_disputes(&tx_manager),
        vec![(addr(0), DisputeKind::Nullify), (addr(0), DisputeKind::Challenge)]
    );
    assert_tee_then_zk_requests(&zk);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
}

#[tokio::test]
async fn test_step_invalid_tee_result_falls_back_to_zk_without_timeout() {
    let (l2, factory, root_15, _root_20) = base_game_mocks();

    let verifier = single_game_verifier(MockGameState {
        tee_prover: DEFAULT_TEE_PROVER,
        intermediate_output_roots: vec![root_15, BOGUS_ROOT],
        ..game_state(20)
    });

    let l1_head = Arc::new(MockL1::success(DEFAULT_L1_HEAD, 100));

    let bad_aggregate_proposal = tee_proposal(B256::repeat_byte(0x42), root_15);
    let proof_requester = succeeded_tee_prover(bad_aggregate_proposal);

    let tx = default_tx_manager();
    let mut driver = test_driver_with_l1_provider(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        verifier,
        l2,
        Arc::<MockZkProofProvider>::clone(&proof_requester),
        tx.clone(),
        l1_head,
    );

    // Step 1: TEE proof job is initiated.
    driver.step().await.unwrap();

    // Step 2: completed TEE proof has the wrong root, so the driver should
    // immediately transition to ZK fallback instead of waiting for timeout.
    driver.step().await.unwrap();

    assert_tee_then_zk_requests(&proof_requester);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 1);
    assert!(tx.recorded_calls().is_empty());

    complete_zk_proof(&proof_requester);
    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();
    assert_eq!(sent_disputes(&tx), vec![(addr(0), DisputeKind::Challenge)]);
    assert_tee_then_zk_requests(&proof_requester);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
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

    let tx = default_tx_manager();
    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), tx.clone());
    driver
        .proof_manager
        .pending_proofs_mut()
        .insert(addr(0), default_ready_proof(DisputeIntent::Nullify));

    driver.step().await.unwrap();

    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
    assert_eq!(sent_disputes(&tx), vec![(addr(0), DisputeKind::Nullify)]);
}

#[tokio::test]
async fn test_successful_staged_tee_nullify_sends_only_dispute() {
    let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
    let l2 = default_l2();
    let verifier = single_game_verifier(mock_state_with_tee(
        GameStatus::InProgress,
        ZK_PROVER_ADDR,
        DEFAULT_TEE_PROVER,
        20,
    ));

    let tx = default_tx_manager();
    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), tx.clone());
    driver.proof_manager.pending_proofs_mut().insert(
        addr(0),
        PendingProof {
            phase: ProofPhase::ReadyToSubmit { proof_bytes: Bytes::from_static(&[0x00, 0xAA]) },
            kind: ProofKind::Tee { zk_fallback: None },
            invalid_index: 1,
            expected_root: BOGUS_ROOT,
            retry_count: 0,
            tee_submit_retry_count: 0,
            intent: DisputeIntent::Nullify,
        },
    );

    driver.step().await.unwrap();

    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
    assert_eq!(sent_disputes(&tx), vec![(addr(0), DisputeKind::Nullify)]);
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
    let mut driver = test_driver(factory, verifier, l2, default_zk_prover(), tx.clone());
    driver
        .proof_manager
        .pending_proofs_mut()
        .insert(addr(0), default_ready_proof(DisputeIntent::Challenge));

    driver.step().await.unwrap();

    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
    assert!(tx.recorded_calls().is_empty());
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
        driver.proof_manager.pending_proofs_len() == 0,
        "no proof should be initiated when the ZK challenge is legitimate"
    );
}

#[tokio::test]
async fn test_step_fraudulent_zk_challenge_nullifies() {
    // The onchain root at the challenged index is correct, meaning the
    // ZK challenge was fraudulent. The driver should initiate a ZK proof
    // with DisputeIntent::Nullify.
    let (l2, factory, verifier) = fraudulent_zk_challenge_mocks(true);

    let zk = succeeded_zk_prover();
    let tx = default_tx_manager();
    let mut driver = test_driver(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        verifier,
        l2,
        Arc::<MockZkProofProvider>::clone(&zk),
        tx.clone(),
    );
    driver.step().await.unwrap();
    assert_zk_requests(&zk, 1);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 1);
    assert!(tx.recorded_calls().is_empty());

    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();
    assert_eq!(sent_disputes(&tx), vec![(addr(0), DisputeKind::Nullify)]);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
}

#[tokio::test]
async fn test_step_nullified_zk_prover_drops_completed_nullification() {
    let (l2, factory, verifier) = fraudulent_zk_challenge_mocks(true);
    let zk = succeeded_zk_prover();
    let tx = default_tx_manager();
    let mut driver = test_driver(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        Arc::clone(&verifier),
        l2,
        Arc::clone(&zk),
        tx.clone(),
    );

    driver.step().await.unwrap();
    assert_zk_requests(&zk, 1);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 1);
    assert!(sent_disputes(&tx).is_empty());

    verifier.games.lock().unwrap().get_mut(&addr(0)).unwrap().zk_prover = Address::ZERO;
    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();

    assert!(sent_disputes(&tx).is_empty());
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
    assert_zk_requests(&zk, 1);
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

    let zk = succeeded_zk_prover();
    let tx = default_tx_manager();
    let mut driver = test_driver(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        verifier,
        l2,
        Arc::<MockZkProofProvider>::clone(&zk),
        tx.clone(),
    );
    driver.step().await.unwrap();
    assert_zk_requests(&zk, 1);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 1);
    assert!(tx.recorded_calls().is_empty());

    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();
    assert_eq!(sent_disputes(&tx), vec![(addr(0), DisputeKind::Nullify)]);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
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

    let zk = succeeded_zk_prover();
    let tx = default_tx_manager();
    let mut driver = test_driver(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        verifier,
        l2,
        Arc::<MockZkProofProvider>::clone(&zk),
        tx.clone(),
    );
    driver.step().await.unwrap();
    assert_zk_requests(&zk, 1);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 1);
    assert!(tx.recorded_calls().is_empty());

    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();
    assert_eq!(sent_disputes(&tx), vec![(addr(0), DisputeKind::Nullify)]);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
}

// ──────────────────────────────────────────────────────────────────────────
// Dual-proof games: both TEE and ZK proofs verified (no challenge)
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_step_dual_proof_invalid_tee_request_failure_falls_back_to_zk_nullify() {
    // A game with both TEE and ZK proofs verified (via verifyProposalProof,
    // not challenge) where the output roots are invalid and the TEE request
    // fails should fall back to a ZK proof with DisputeIntent::Nullify.
    let (l2, factory, root_15, _root_20) = base_game_mocks();

    let verifier = single_game_verifier(MockGameState {
        tee_prover: DEFAULT_TEE_PROVER,
        zk_prover: ZK_PROVER_ADDR,
        intermediate_output_roots: vec![root_15, BOGUS_ROOT],
        ..game_state(20)
    });

    let zk = succeeded_zk_prover();
    let tx = default_tx_manager();
    let mut driver = test_driver(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        verifier,
        l2,
        Arc::<MockZkProofProvider>::clone(&zk),
        tx.clone(),
    );
    driver.step().await.unwrap();
    assert_zk_requests(&zk, 1);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 1);
    assert!(tx.recorded_calls().is_empty());

    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();
    assert_eq!(sent_disputes(&tx), vec![(addr(0), DisputeKind::Nullify)]);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
}

#[tokio::test]
async fn test_step_dual_proof_invalid_with_tee_provider_nullifies_tee_first() {
    // A game with both TEE and ZK proofs verified where output roots are
    // invalid and a TEE provider is available should attempt TEE nullification
    // first (fast path). After TEE nullification the game will be rescanned
    // as InvalidZkProposal on the next tick.
    let (l2, factory, root_15, root_20) = base_game_mocks();

    let verifier = single_game_verifier(MockGameState {
        tee_prover: DEFAULT_TEE_PROVER,
        zk_prover: ZK_PROVER_ADDR,
        intermediate_output_roots: vec![root_15, BOGUS_ROOT],
        ..game_state(20)
    });

    let l1_head = Arc::new(MockL1::success(DEFAULT_L1_HEAD, 100));

    let aggregate_proposal = tee_proposal(root_20, root_15);
    let proof_requester = succeeded_tee_prover(aggregate_proposal);

    let tx_manager = MockTxManager::with_responses(vec![
        Ok(receipt_with_status(true, DEFAULT_TX_HASH)),
        Ok(receipt_with_status(true, DEFAULT_TX_HASH)),
    ]);

    let mut driver = test_driver_with_l1_provider(
        Arc::<MockDisputeGameFactory>::clone(&factory),
        Arc::clone(&verifier),
        l2,
        Arc::<MockZkProofProvider>::clone(&proof_requester),
        tx_manager.clone(),
        l1_head,
    );

    driver.step().await.unwrap();
    assert!(tx_manager.recorded_calls().is_empty());
    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();
    assert_eq!(sent_disputes(&tx_manager), vec![(addr(0), DisputeKind::Nullify)]);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);

    verifier.update_game(
        addr(0),
        MockGameState {
            tee_prover: Address::ZERO,
            zk_prover: ZK_PROVER_ADDR,
            intermediate_output_roots: vec![root_15, BOGUS_ROOT],
            ..game_state(20)
        },
    );
    factory.games.lock().unwrap().push(factory_game(0, 1));
    driver.step().await.unwrap();
    assert_tee_then_zk_requests(&proof_requester);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 1);
    assert_eq!(sent_disputes(&tx_manager), vec![(addr(0), DisputeKind::Nullify)]);

    complete_zk_proof(&proof_requester);
    factory.games.lock().unwrap().clear();
    driver.step().await.unwrap();
    assert_eq!(sent_disputes(&tx_manager), vec![(addr(0), DisputeKind::Nullify); 2]);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
}

// ──────────────────────────────────────────────────────────────────────────
// Checkpoint count mismatch (stale interval) surfaces as an error
// ──────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_step_checkpoint_count_mismatch_surfaces_error() {
    // After a governance `setImplementation` changes `INTERMEDIATE_BLOCK_INTERVAL`,
    // the cached stale interval produces a `CheckpointCountMismatch`
    // error. This error must NOT be silently swallowed — it should
    // propagate as a per-game error so operators notice.
    //
    // Setup: starting=10, l2_block=20, span=10.
    // Onchain impl uses interval=10 → stores 1 intermediate root.
    // But we configure the mock so the scanner resolves interval=5
    // (the default mock value), which expects 2 intermediate roots.
    // This triggers CheckpointCountMismatch { expected: 2, actual: 1 }.
    let factory = single_game_factory();

    let verifier = single_game_verifier(MockGameState {
        tee_prover: DEFAULT_TEE_PROVER,
        // Onchain: only 1 intermediate root (as if interval=10).
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
        driver.proof_manager.pending_proofs_len() == 0,
        "no proof should be initiated when checkpoint count mismatches — \
         the error should be surfaced, not silently swallowed"
    );
}

#[tokio::test]
async fn test_step_proof_timeout_reuses_session_without_submitting() {
    for status in [ProofStatus::Queued, ProofStatus::Running] {
        let (l2, factory, verifier) = invalid_game_mocks();
        let zk = Arc::new(MockZkProofProvider {
            state: Mutex::new(MockZkProofState { proof_status: status, ..Default::default() }),
        });
        let tx = default_tx_manager();
        let mut driver = test_driver_with_timeout(
            factory,
            verifier,
            l2,
            Arc::<MockZkProofProvider>::clone(&zk),
            tx.clone(),
            Arc::new(MockL1::failure("unused")),
            Duration::ZERO,
        );
        driver.step().await.unwrap();
        assert_zk_requests(&zk, 1);
        assert!(tx.recorded_calls().is_empty());

        driver.step().await.unwrap();
        assert_zk_requests(&zk, 2);
        assert_eq!(driver.proof_manager.pending_proofs_len(), 1);
        assert!(tx.recorded_calls().is_empty());
    }
}

#[tokio::test]
async fn test_step_zk_invalid_proof_ignores_game_without_reproving_or_resubmitting() {
    let (l2, factory, verifier) = invalid_game_mocks();
    let zk = succeeded_zk_prover();
    let tx = MockTxManager::new(Err(TxManagerError::ExecutionReverted {
        reason: None,
        data: Some(Bytes::from(invalid_proof_selector().to_vec())),
    }));
    let mut driver =
        test_driver(factory, verifier, l2, Arc::<MockZkProofProvider>::clone(&zk), tx.clone());

    driver.step().await.unwrap();
    assert_zk_requests(&zk, 1);
    assert!(tx.recorded_calls().is_empty());
    driver.step().await.unwrap();
    assert_eq!(driver.proof_manager.ignored_games_len(), 1);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
    assert_zk_requests(&zk, 1);
    assert_eq!(sent_disputes(&tx), vec![(addr(0), DisputeKind::Challenge)]);

    driver.step().await.unwrap();
    assert_eq!(driver.proof_manager.ignored_games_len(), 1);
    assert_eq!(driver.proof_manager.pending_proofs_len(), 0);
    assert_zk_requests(&zk, 1);
    assert_eq!(sent_disputes(&tx), vec![(addr(0), DisputeKind::Challenge)]);
}
