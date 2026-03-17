//! Main driver loop for the challenger service.
//!
//! The [`Driver`] ties together all challenger components — scanning for
//! invalid dispute games, validating output roots, requesting ZK proofs, and
//! submitting nullification transactions — into a single polling loop.

use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy_primitives::{Address, B256, Bytes};
use base_proof_contracts::AggregateVerifierClient;
use base_proof_rpc::L2Provider;
use base_tx_manager::TxManager;
use base_zk_client::{
    GetProofRequest, ProofJobStatus, ProofType, ProveBlockRequest, ReceiptType, ZkProofProvider,
};
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    CandidateGame, ChallengeSubmitter, ChallengerMetrics, GameScanner,
    IntermediateValidationParams, OutputValidator, ValidatorError,
};

/// Proof type discriminator byte prepended to ZK proof receipts.
const ZK_PROOF_TYPE_BYTE: u8 = 0x01;

/// Phase of a pending proof: either awaiting the ZK service or ready for
/// on-chain submission.
#[derive(Debug, Clone)]
pub enum ProofPhase {
    /// Waiting for the ZK proof service to complete.
    AwaitingProof {
        /// Session ID returned by the ZK proof service.
        session_id: String,
    },
    /// Proof obtained — receipt bytes are ready for nullification submission.
    ReadyToSubmit {
        /// Type-prefixed proof receipt bytes.
        proof_bytes: Bytes,
    },
}

/// State for an in-flight proof session.
#[derive(Debug, Clone)]
pub struct PendingProof {
    /// Current phase of this proof lifecycle.
    pub phase: ProofPhase,
    /// The index of the invalid intermediate root.
    pub invalid_index: usize,
    /// The expected correct root at that index.
    pub expected_root: B256,
}

/// Configuration for the challenger [`Driver`].
#[derive(Debug)]
pub struct DriverConfig {
    /// How often the driver polls for new games.
    pub poll_interval: Duration,
    /// Cancellation token for graceful shutdown.
    pub cancel: CancellationToken,
}

/// Orchestrates the challenger pipeline: scan, validate, prove, submit.
pub struct Driver<L2, P, T>
where
    L2: L2Provider,
    P: ZkProofProvider,
    T: TxManager,
{
    scanner: GameScanner,
    validator: OutputValidator<L2>,
    zk_prover: Arc<P>,
    submitter: ChallengeSubmitter<T>,
    verifier_client: Arc<dyn AggregateVerifierClient>,
    /// In-flight proof sessions keyed by game address.
    pending_proofs: HashMap<Address, PendingProof>,
    poll_interval: Duration,
    cancel: CancellationToken,
    last_scanned: Option<u64>,
}

impl<L2: L2Provider, P: ZkProofProvider, T: TxManager> std::fmt::Debug for Driver<L2, P, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Driver")
            .field("pending_proofs", &self.pending_proofs.len())
            .field("poll_interval", &self.poll_interval)
            .field("last_scanned", &self.last_scanned)
            .finish_non_exhaustive()
    }
}

impl<L2: L2Provider, P: ZkProofProvider, T: TxManager> Driver<L2, P, T> {
    /// Creates a new driver with the given components.
    pub fn new(
        config: DriverConfig,
        scanner: GameScanner,
        validator: OutputValidator<L2>,
        zk_prover: Arc<P>,
        submitter: ChallengeSubmitter<T>,
        verifier_client: Arc<dyn AggregateVerifierClient>,
    ) -> Self {
        Self {
            scanner,
            validator,
            zk_prover,
            submitter,
            verifier_client,
            pending_proofs: HashMap::new(),
            poll_interval: config.poll_interval,
            cancel: config.cancel,
            last_scanned: None,
        }
    }

    /// Runs the main driver loop until the cancellation token is fired.
    pub async fn run(mut self) {
        info!("challenger driver starting");
        loop {
            if self.cancel.is_cancelled() {
                info!("challenger driver shutting down");
                break;
            }

            if let Err(e) = self.step().await {
                warn!(error = %e, "driver step failed");
            }

            metrics::gauge!(ChallengerMetrics::PENDING_PROOFS)
                .set(self.pending_proofs.len() as f64);

            select! {
                biased;
                () = self.cancel.cancelled() => {
                    info!("challenger driver shutting down");
                    break;
                }
                () = tokio::time::sleep(self.poll_interval) => {}
            }
        }
    }

    /// Executes a single scan-validate-prove-submit cycle.
    ///
    /// First polls any in-flight proof sessions that are not in the current
    /// scan batch, then scans for new candidates and processes them.
    async fn step(&mut self) -> eyre::Result<()> {
        // Poll in-flight proof sessions before scanning for new candidates.
        self.poll_pending_proofs().await;

        let (candidates, new_last_scanned) = self.scanner.scan(self.last_scanned).await?;
        self.last_scanned = new_last_scanned;

        for candidate in candidates {
            let index = candidate.index;
            if let Err(e) = self.process_candidate(candidate).await {
                warn!(error = %e, game_index = index, "failed to process candidate");
            }
        }

        Ok(())
    }

    /// Polls all in-flight proof sessions for completion or retries submission.
    async fn poll_pending_proofs(&mut self) {
        let addresses: Vec<Address> = self.pending_proofs.keys().copied().collect();

        for game_address in addresses {
            if let Err(e) = self.poll_or_submit(game_address).await {
                warn!(
                    error = %e,
                    game = %game_address,
                    "failed to poll/submit pending proof"
                );
            }
        }
    }

    /// Processes a single candidate game: validate, prove if invalid, submit.
    async fn process_candidate(&mut self, candidate: CandidateGame) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;

        // If this game already has an in-flight proof session, skip it.
        // Pending proofs are polled separately in `poll_pending_proofs`.
        if self.pending_proofs.contains_key(&game_address) {
            debug!(game = %game_address, "skipping game with pending proof session");
            return Ok(());
        }

        let intermediate_roots =
            self.verifier_client.intermediate_output_roots(game_address).await?;

        let params = IntermediateValidationParams {
            game_address,
            starting_block_number: candidate.starting_block_number,
            l2_block_number: candidate.info.l2_block_number,
            intermediate_block_interval: candidate.intermediate_block_interval,
            claimed_root: candidate.info.root_claim,
            intermediate_roots,
        };

        let result = match self.validator.validate_intermediate_roots(params).await {
            Ok(r) => r,
            Err(e) => {
                match &e {
                    ValidatorError::BlockNotAvailable { .. } => {
                        debug!(
                            game = %game_address,
                            error = %e,
                            "block not yet available, skipping game"
                        );
                    }
                    _ => {
                        warn!(
                            game = %game_address,
                            error = %e,
                            "validation error, skipping game"
                        );
                    }
                }
                return Ok(());
            }
        };

        if result.is_valid {
            debug!(game = %game_address, "game output roots are valid");
            return Ok(());
        }

        let invalid_index = result
            .invalid_intermediate_index
            .ok_or_else(|| eyre::eyre!("invalid result missing invalid_intermediate_index"))?;
        let expected_root = result.expected_root;

        info!(
            game = %game_address,
            invalid_index = invalid_index,
            expected_root = %expected_root,
            "invalid intermediate root detected, requesting proof"
        );

        self.initiate_proof(candidate, invalid_index, expected_root).await
    }

    /// Requests a ZK proof, stores the session, and polls for the result.
    async fn initiate_proof(
        &mut self,
        candidate: CandidateGame,
        invalid_index: usize,
        expected_root: B256,
    ) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;

        let multiplier = u64::try_from(invalid_index)
            .ok()
            .and_then(|i| i.checked_add(1))
            .ok_or_else(|| eyre::eyre!("invalid_index overflow"))?;
        let number_of_blocks_to_prove = candidate
            .intermediate_block_interval
            .checked_mul(multiplier)
            .ok_or_else(|| eyre::eyre!("number_of_blocks_to_prove overflow"))?;

        let request = ProveBlockRequest {
            start_block_number: candidate.starting_block_number,
            number_of_blocks_to_prove,
            sequence_window: None,
            proof_type: ProofType::GenericZkvmClusterCompressed as i32,
        };

        let prove_response = self.zk_prover.prove_block(request).await?;
        let session_id = prove_response.session_id;

        info!(
            game = %game_address,
            session_id = %session_id,
            "proof job initiated"
        );

        let pending = PendingProof {
            phase: ProofPhase::AwaitingProof { session_id },
            invalid_index,
            expected_root,
        };
        self.pending_proofs.insert(game_address, pending);

        self.poll_or_submit(game_address).await
    }

    /// Advances a pending proof through its lifecycle.
    ///
    /// - **`AwaitingProof`** — polls the ZK service:
    ///   - `Succeeded` → transitions to `ReadyToSubmit` and falls through to
    ///     submission.
    ///   - `Failed` → removes the entry so a fresh `prove_block` is issued
    ///     next cycle.
    ///   - Intermediate (`Created`/`Pending`/`Running`) → returns early.
    /// - **`ReadyToSubmit`** — submits the nullification tx:
    ///   - On success → removes the entry.
    ///   - On failure → leaves the entry so it is retried next tick.
    async fn poll_or_submit(&mut self, game_address: Address) -> eyre::Result<()> {
        let pending = match self.pending_proofs.get(&game_address) {
            Some(p) => p.clone(),
            None => return Ok(()),
        };

        // Check if the game is still challengeable before doing any work.
        let (status, zk_prover) = tokio::try_join!(
            self.verifier_client.status(game_address),
            self.verifier_client.zk_prover(game_address),
        )?;
        if status != GameScanner::STATUS_IN_PROGRESS {
            debug!(game = %game_address, status = status, "game no longer in progress, dropping pending proof");
            self.pending_proofs.remove(&game_address);
            return Ok(());
        }
        if zk_prover != Address::ZERO {
            debug!(game = %game_address, zk_prover = %zk_prover, "game already challenged, dropping pending proof");
            self.pending_proofs.remove(&game_address);
            return Ok(());
        }

        // Resolve the proof bytes to submit — either by polling the ZK
        // service or by extracting them from an already-obtained proof.
        let proof_bytes = match pending.phase {
            ProofPhase::AwaitingProof { ref session_id } => {
                let get_proof_request = GetProofRequest {
                    session_id: session_id.clone(),
                    receipt_type: Some(ReceiptType::Snark as i32),
                };

                let proof_response = self.zk_prover.get_proof(get_proof_request).await?;
                let status = ProofJobStatus::try_from(proof_response.status)
                    .unwrap_or(ProofJobStatus::Unspecified);

                match status {
                    ProofJobStatus::Succeeded => {
                        let mut raw = Vec::with_capacity(1 + proof_response.receipt.len());
                        raw.push(ZK_PROOF_TYPE_BYTE);
                        raw.extend_from_slice(&proof_response.receipt);
                        let proof_bytes = Bytes::from(raw);

                        info!(
                            game = %game_address,
                            session_id = %session_id,
                            proof_len = proof_bytes.len(),
                            "proof succeeded, submitting nullification"
                        );

                        // Transition to ReadyToSubmit (keeps entry in the map).
                        self.pending_proofs.insert(
                            game_address,
                            PendingProof {
                                phase: ProofPhase::ReadyToSubmit {
                                    proof_bytes: proof_bytes.clone(),
                                },
                                invalid_index: pending.invalid_index,
                                expected_root: pending.expected_root,
                            },
                        );

                        proof_bytes
                    }
                    ProofJobStatus::Failed => {
                        self.pending_proofs.remove(&game_address);
                        warn!(
                            game = %game_address,
                            session_id = %session_id,
                            "proof job failed, will retry next tick"
                        );
                        return Ok(());
                    }
                    other => {
                        debug!(
                            game = %game_address,
                            session_id = %session_id,
                            status = ?other,
                            "proof not ready, will retry next tick"
                        );
                        return Ok(());
                    }
                }
            }
            ProofPhase::ReadyToSubmit { proof_bytes } => proof_bytes,
        };

        // ── Submit nullification ─────────────────────────────────────────
        match self
            .submitter
            .submit_nullification(
                game_address,
                proof_bytes,
                u64::try_from(pending.invalid_index)
                    .map_err(|_| eyre::eyre!("invalid_index overflow"))?,
                pending.expected_root,
            )
            .await
        {
            Ok(_) => {
                self.pending_proofs.remove(&game_address);
            }
            Err(e) => {
                warn!(
                    error = %e,
                    game = %game_address,
                    "nullification tx failed, will retry next tick"
                );
                // Leave entry as ReadyToSubmit for retry.
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use alloy_primitives::{Address, B256};
    use base_enclave::output_root_v0;
    use base_proof_contracts::GameAtIndex;

    use super::*;
    use crate::{
        ChallengeSubmitter, ScannerConfig,
        test_utils::{
            MockAggregateVerifier, MockDisputeGameFactory, MockGameState, MockL2Provider,
            MockTxManager, MockZkProofProvider, addr, build_test_header_and_account, factory_game,
            mock_state, receipt_with_status,
        },
    };

    /// Builds a test driver with the given mocks.
    fn test_driver(
        factory: Arc<MockDisputeGameFactory>,
        verifier: Arc<MockAggregateVerifier>,
        l2_provider: Arc<MockL2Provider>,
        zk_prover: Arc<MockZkProofProvider>,
        tx_manager: MockTxManager,
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
        };

        Driver::new(
            config,
            scanner,
            validator,
            zk_prover,
            submitter,
            verifier as Arc<dyn AggregateVerifierClient>,
        )
    }

    fn default_zk_prover() -> Arc<MockZkProofProvider> {
        Arc::new(MockZkProofProvider {
            session_id: "test-session".to_string(),
            proof_status: Mutex::new(ProofJobStatus::Created as i32),
            receipt: Mutex::new(vec![]),
        })
    }

    fn default_tx_manager() -> MockTxManager {
        MockTxManager::new(Ok(receipt_with_status(true, B256::repeat_byte(0xAA))))
    }

    /// Builds the common L2, factory, and verifier mocks for an invalid-game
    /// scenario: starting=10, `l2_block=20`, interval=5, checkpoints at 15 and
    /// 20 with a correct root at 15 and a bogus root at 20 (invalid index 1).
    fn invalid_game_mocks()
    -> (Arc<MockL2Provider>, Arc<MockDisputeGameFactory>, Arc<MockAggregateVerifier>) {
        let storage_hash = B256::repeat_byte(0xBB);
        let (header_15, account_15) = build_test_header_and_account(15, storage_hash);
        let root_15 = output_root_v0(&header_15, storage_hash);
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

    #[tokio::test]
    async fn test_step_no_candidates() {
        let factory = Arc::new(MockDisputeGameFactory { games: vec![] });
        let verifier = Arc::new(MockAggregateVerifier { games: HashMap::new() });
        let l2 = Arc::new(MockL2Provider::new());

        let mut driver =
            test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());

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
            proof_status: Mutex::new(ProofJobStatus::Created as i32),
            receipt: Mutex::new(vec![]),
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
            proof_status: Mutex::new(ProofJobStatus::Created as i32),
            receipt: Mutex::new(vec![]),
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
        // ZK prover returns Failed → no nullification submitted.
        let (l2, factory, verifier) = invalid_game_mocks();

        let zk = Arc::new(MockZkProofProvider {
            session_id: "proof-fail".to_string(),
            proof_status: Mutex::new(ProofJobStatus::Failed as i32),
            receipt: Mutex::new(vec![]),
        });

        // tx_manager should NOT be called (proof failed → no submission)
        let tx_manager = default_tx_manager();

        let mut driver = test_driver(factory, verifier, l2, zk, tx_manager);

        // step succeeds — proof failure is logged but not an error
        driver.step().await.unwrap();
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

        let mut driver =
            test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());

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
            async fn game_count(&self) -> Result<u64, base_proof_contracts::ContractError> {
                Err(base_proof_contracts::ContractError::Validation(
                    "simulated game_count failure".into(),
                ))
            }

            async fn game_at_index(
                &self,
                _index: u64,
            ) -> Result<GameAtIndex, base_proof_contracts::ContractError> {
                unreachable!()
            }

            async fn init_bonds(
                &self,
                _game_type: u32,
            ) -> Result<alloy_primitives::U256, base_proof_contracts::ContractError> {
                unreachable!()
            }

            async fn game_impls(
                &self,
                _game_type: u32,
            ) -> Result<Address, base_proof_contracts::ContractError> {
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
        };

        let mut driver = Driver::new(
            config,
            scanner,
            validator,
            default_zk_prover(),
            submitter,
            verifier as Arc<dyn AggregateVerifierClient>,
        );

        let result = driver.step().await;
        assert!(result.is_err(), "scan error should propagate");
    }

    #[tokio::test]
    async fn test_step_pending_proof_skips_prove_block() {
        // First step: proof initiated (status=Created, not ready).
        // Second step: same game re-discovered → polls existing session,
        // proof succeeds, nullification submitted.
        let (l2, factory, verifier) = invalid_game_mocks();

        let zk = Arc::new(MockZkProofProvider {
            session_id: "pending-session".to_string(),
            proof_status: Mutex::new(ProofJobStatus::Created as i32),
            receipt: Mutex::new(vec![0xBE, 0xEF]),
        });

        let tx_hash = B256::repeat_byte(0xDD);
        let tx_manager = MockTxManager::new(Ok(receipt_with_status(true, tx_hash)));

        let mut driver = test_driver(factory, verifier, l2, Arc::clone(&zk), tx_manager);

        // Step 1: proof is initiated but not ready (Created) → session stored.
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
        });

        // First tx call fails (NonceTooLow), second succeeds.
        let tx_manager = crate::test_utils::MockTxManager::with_responses(vec![
            Err(base_tx_manager::TxManagerError::NonceTooLow),
            Ok(receipt_with_status(true, B256::repeat_byte(0xCC))),
        ]);

        let mut driver = test_driver(factory, verifier, l2, zk, tx_manager);

        // Step 1: proof succeeds, but nullification tx fails.
        // The error is swallowed by `step` (logged as a warning) because
        // `poll_or_submit` returns Ok — only poll_pending_proofs logs it.
        // But initiate_proof calls poll_or_submit directly, so its error
        // propagates up through process_candidate → step logs it.
        driver.step().await.unwrap();

        // Entry must still be in pending_proofs as ReadyToSubmit.
        let entry = driver.pending_proofs.get(&addr(0)).expect("proof should be preserved");
        assert!(
            matches!(entry.phase, ProofPhase::ReadyToSubmit { .. }),
            "phase should be ReadyToSubmit after tx failure"
        );

        // Step 2: poll_pending_proofs re-submits, now the tx succeeds.
        driver.step().await.unwrap();
        assert!(
            !driver.pending_proofs.contains_key(&addr(0)),
            "entry should be removed after successful submission"
        );
    }

    /// Builds a driver with a single pending `ReadyToSubmit` proof at `addr(0)`
    /// whose verifier reports the given `game_state`.
    fn driver_with_ready_proof(
        game_state: MockGameState,
    ) -> Driver<MockL2Provider, MockZkProofProvider, MockTxManager> {
        let (l2, factory, _verifier) = invalid_game_mocks();
        let verifier_games = HashMap::from([(addr(0), game_state)]);
        let verifier = Arc::new(MockAggregateVerifier { games: verifier_games });
        let mut driver =
            test_driver(factory, verifier, l2, default_zk_prover(), default_tx_manager());
        driver.pending_proofs.insert(
            addr(0),
            PendingProof {
                phase: ProofPhase::ReadyToSubmit {
                    proof_bytes: Bytes::from_static(&[0x01, 0xDE, 0xAD]),
                },
                invalid_index: 1,
                expected_root: B256::repeat_byte(0xEE),
            },
        );
        driver
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
        };

        let driver = Driver::new(
            config,
            scanner,
            validator,
            default_zk_prover(),
            submitter,
            verifier as Arc<dyn AggregateVerifierClient>,
        );

        // Cancel immediately
        cancel.cancel();

        // run() should return promptly
        tokio::time::timeout(Duration::from_secs(2), driver.run())
            .await
            .expect("driver.run() should exit promptly after cancellation");
    }
}
