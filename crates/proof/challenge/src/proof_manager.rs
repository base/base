//! Dispute-proof lifecycle management.
//!
//! [`DisputeProofManager`] owns in-flight proof sessions from initiation
//! through proof polling, retries, TEE-to-ZK fallback, and coordination of
//! onchain dispute submission. The [`Driver`](crate::Driver) remains
//! responsible for scanning candidate games.

use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256};
use base_proof_contracts::{AggregateVerifierClient, GameStatus};
use base_proof_primitives::ProofRequest as TeeProofRequest;
use base_proof_rpc::{L1Provider, L2Provider};
use base_proof_submission::KnownRevert;
use base_prover_service_client::ProofRequesterProvider;
use base_prover_service_protocol::{SnarkPlonkProofRequest, ZkBackend, ZkProofRequest, ZkVm};
use base_tx_manager::{TxManager, TxManagerError};
use tracing::{debug, info, warn};

use crate::{
    CandidateGame, ChallengeSubmitError, ChallengeSubmitter, ChallengerMetrics,
    ChallengerProofAdapter, DisputeIntent, OutputValidator, PendingProof, PendingProofs, ProofKind,
    ProofPhase, ProofUpdate,
};

/// Manages the lifecycle of proofs used to dispute invalid games.
pub struct DisputeProofManager<L2: L2Provider, P: ProofRequesterProvider> {
    /// Validates output roots and constructs TEE proof commitments.
    validator: OutputValidator<L2>,
    /// Prover-service requester used to generate and poll fault proofs.
    proof_requester: Arc<P>,
    /// L1 provider used to construct TEE proof requests.
    l1_provider: Arc<dyn L1Provider>,
    verifier_client: Arc<dyn AggregateVerifierClient>,
    /// In-flight proof sessions keyed by game address.
    pending_proofs: PendingProofs,
    ignored_games: HashSet<Address>,
    ignored_game_order: VecDeque<Address>,
    max_proof_duration: Duration,
    tee_submit_retry_limit: u32,
}

impl<L2: L2Provider, P: ProofRequesterProvider> std::fmt::Debug for DisputeProofManager<L2, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisputeProofManager")
            .field("pending_proofs", &self.pending_proofs.len())
            .field("tee_submit_retry_limit", &self.tee_submit_retry_limit)
            .finish_non_exhaustive()
    }
}

impl<L2: L2Provider, P: ProofRequesterProvider> DisputeProofManager<L2, P> {
    /// Maximum number of times a failed proof job will be retried before being dropped.
    pub const MAX_PROOF_RETRIES: u32 = 3;

    /// Maximum number of terminally ignored games retained to avoid rediscovery churn.
    ///
    /// Evicted games may be rediscovered by a later scan, then re-ignored after
    /// one check.
    pub const MAX_IGNORED_GAMES: usize = 10_000;

    /// Creates a proof manager from its validator, proof client, and contract clients.
    pub fn new(
        validator: OutputValidator<L2>,
        proof_requester: Arc<P>,
        l1_provider: Arc<dyn L1Provider>,
        verifier_client: Arc<dyn AggregateVerifierClient>,
        max_proof_duration: Duration,
        tee_submit_retry_limit: u32,
    ) -> Self {
        Self {
            validator,
            proof_requester,
            l1_provider,
            verifier_client,
            pending_proofs: PendingProofs::new(),
            ignored_games: HashSet::new(),
            ignored_game_order: VecDeque::new(),
            max_proof_duration,
            tee_submit_retry_limit,
        }
    }

    /// Returns whether a game is terminally ignored.
    pub fn is_ignored(&self, game_address: Address) -> bool {
        self.ignored_games.contains(&game_address)
    }

    /// Returns whether a game has an in-flight proof session.
    pub fn has_pending_proof(&self, game_address: Address) -> bool {
        self.pending_proofs.contains_key(&game_address)
    }

    /// Returns the number of in-flight proof sessions.
    pub fn pending_proofs_len(&self) -> usize {
        self.pending_proofs.len()
    }

    /// Returns in-flight proof sessions for test inspection.
    #[cfg(any(test, feature = "test-utils"))]
    pub const fn pending_proofs(&self) -> &PendingProofs {
        &self.pending_proofs
    }

    /// Returns in-flight proof sessions for test setup.
    #[cfg(any(test, feature = "test-utils"))]
    pub const fn pending_proofs_mut(&mut self) -> &mut PendingProofs {
        &mut self.pending_proofs
    }

    /// Returns the number of terminally ignored games retained in memory.
    pub fn ignored_games_len(&self) -> usize {
        self.ignored_games.len()
    }

    /// Polls all in-flight proof sessions for completion or retries submission.
    pub async fn poll_pending_proofs<T: TxManager>(&mut self, submitter: &ChallengeSubmitter<T>) {
        let addresses = self.pending_proofs.addresses();

        for game_address in addresses {
            if let Err(e) = self.poll_or_submit(game_address, submitter).await {
                warn!(
                    error = %e,
                    game = %game_address,
                    "failed to poll/submit pending proof"
                );
            }
        }
    }

    /// Attempts TEE-first proof sourcing with ZK fallback.
    ///
    /// The `intent` determines the onchain action for the ZK fallback path.
    /// TEE proofs always use `nullify()` regardless of `intent`.
    ///
    /// When `try_tee_first` is `true` and the game has a non-zero TEE prover,
    /// a synchronous TEE proof is attempted before falling back to ZK.
    #[tracing::instrument(
        name = "challenger.initiate_proof",
        skip_all,
        fields(game = %candidate.factory.proxy, intent = ?intent)
    )]
    pub async fn initiate_proof(
        &mut self,
        prover_address: Address,
        candidate: CandidateGame,
        invalid_index: u64,
        expected_root: B256,
        intent: DisputeIntent,
        try_tee_first: bool,
    ) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;

        if candidate.tee_prover != Address::ZERO && try_tee_first {
            ChallengerMetrics::tee_proof_attempts_total().increment(1);
            match self
                .build_tee_request(
                    prover_address,
                    &candidate,
                    invalid_index,
                    expected_root,
                    self.l1_provider.as_ref(),
                )
                .await
            {
                Ok(tee_request) => {
                    let zk_fallback =
                        match self.build_zk_request(prover_address, &candidate, invalid_index) {
                            Ok(request) => Some((request, intent)),
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    game = %game_address,
                                    "failed to build ZK fallback request; \
                                     TEE proof will have no ZK fallback"
                                );
                                None
                            }
                        };

                    let request = ChallengerProofAdapter::tee_prove_block_range_request(
                        game_address,
                        invalid_index,
                        tee_request,
                    );
                    match self.proof_requester.prove_block_range(request).await {
                        Ok(response) => {
                            info!(
                                game = %game_address,
                                session_id = %response.session_id,
                                path = "tee",
                                "TEE proof job initiated"
                            );
                            self.pending_proofs.insert(
                                game_address,
                                PendingProof::awaiting_tee(
                                    response.session_id,
                                    invalid_index,
                                    expected_root,
                                    zk_fallback,
                                ),
                            );
                            return Ok(());
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                game = %game_address,
                                "TEE proof request failed, falling back to ZK"
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        game = %game_address,
                        "failed to build TEE proof request, falling back to ZK"
                    );
                }
            }
            ChallengerMetrics::tee_proof_fallback_total().increment(1);
        }

        self.initiate_zk_proof(prover_address, candidate, invalid_index, expected_root, intent)
            .await
    }

    /// Requests a ZK proof and stores its session for later polling.
    pub async fn initiate_zk_proof(
        &mut self,
        prover_address: Address,
        candidate: CandidateGame,
        invalid_index: u64,
        expected_root: B256,
        intent: DisputeIntent,
    ) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;

        let proof_request = self.build_zk_request(prover_address, &candidate, invalid_index)?;
        let request = ChallengerProofAdapter::snark_plonk_prove_block_range_request(
            game_address,
            invalid_index,
            proof_request.clone(),
        );

        let prove_response = self.proof_requester.prove_block_range(request).await?;

        info!(
            game = %game_address,
            session_id = %prove_response.session_id,
            "proof job initiated"
        );

        self.pending_proofs.insert(
            game_address,
            PendingProof::awaiting(
                prove_response.session_id,
                invalid_index,
                expected_root,
                proof_request,
                intent,
            ),
        );

        Ok(())
    }

    async fn build_tee_request(
        &self,
        proposer: Address,
        candidate: &CandidateGame,
        invalid_index: u64,
        expected_root: B256,
        l1_provider: &dyn L1Provider,
    ) -> eyre::Result<TeeProofRequest> {
        let start_block_number = candidate.checkpoint_start_block(invalid_index)?;

        let claimed_l2_block_number = start_block_number
            .checked_add(candidate.intermediate_block_interval)
            .ok_or_else(|| eyre::eyre!("claimed_l2_block_number overflow"))?;

        let l1_head = candidate.l1_head;
        let (l1_header_result, output_root_result) = tokio::join!(
            l1_provider.header_by_hash(l1_head),
            self.validator.compute_output_root_with_hash(start_block_number),
        );
        let l1_head_number = l1_header_result?.number;
        let (agreed_l2_head_hash, agreed_l2_output_root) = output_root_result?;

        Ok(TeeProofRequest {
            l1_head,
            agreed_l2_head_hash,
            agreed_l2_output_root,
            claimed_l2_output_root: expected_root,
            claimed_l2_block_number,
            proposer,
            intermediate_block_interval: candidate.intermediate_block_interval,
            l1_head_number,
            schedule_l2_block_number: Some(candidate.info.l2_block_number),
        })
    }

    fn build_zk_request(
        &self,
        prover_address: Address,
        candidate: &CandidateGame,
        invalid_index: u64,
    ) -> eyre::Result<SnarkPlonkProofRequest> {
        let start_block_number = candidate.checkpoint_start_block(invalid_index)?;

        Ok(SnarkPlonkProofRequest {
            proof: ZkProofRequest {
                start_block_number,
                number_of_blocks_to_prove: candidate.intermediate_block_interval,
                sequence_window: None,
                l1_head: Some(candidate.l1_head),
                intermediate_root_interval: Some(candidate.intermediate_block_interval),
                schedule_l2_block_number: Some(candidate.info.l2_block_number),
                zk_vm: ZkVm::Sp1,
                zk_backend: ZkBackend::Cluster,
            },
            prover_address,
        })
    }

    async fn poll_or_submit<T: TxManager>(
        &mut self,
        game_address: Address,
        submitter: &ChallengeSubmitter<T>,
    ) -> eyre::Result<()> {
        let (invalid_index, expected_root, intent, targets_tee, was_awaiting) =
            match self.pending_proofs.get(&game_address) {
                Some(p) => (
                    p.invalid_index,
                    p.expected_root,
                    p.intent,
                    p.kind.is_tee(),
                    matches!(p.phase, ProofPhase::AwaitingProof { .. }),
                ),
                None => return Ok(()),
            };

        let proof_update = self
            .pending_proofs
            .poll(game_address, &*self.proof_requester, self.max_proof_duration)
            .await?;
        match &proof_update {
            Some(ProofUpdate::Pending) => {
                debug!(game = %game_address, "proof not ready, will retry next tick");
                return Ok(());
            }
            None => return Ok(()),
            _ => {}
        }

        let (status, tee_prover, zk_prover) = tokio::try_join!(
            self.verifier_client.status(game_address),
            self.verifier_client.tee_prover(game_address),
            self.verifier_client.zk_prover(game_address),
        )?;

        if status != GameStatus::InProgress {
            debug!(game = %game_address, status = ?status, "game no longer in progress, dropping pending proof");
            self.pending_proofs.remove(&game_address);
            return Ok(());
        }

        let already_resolved = match intent {
            DisputeIntent::Challenge => zk_prover != Address::ZERO || tee_prover == Address::ZERO,
            DisputeIntent::Nullify => {
                if targets_tee {
                    tee_prover == Address::ZERO
                } else {
                    zk_prover == Address::ZERO
                }
            }
        };

        if already_resolved {
            debug!(
                game = %game_address,
                intent = ?intent,
                tee_prover = %tee_prover,
                zk_prover = %zk_prover,
                "game already resolved, dropping pending proof"
            );
            self.pending_proofs.remove(&game_address);
            return Ok(());
        }

        let proof_bytes = match proof_update {
            Some(ProofUpdate::Ready(proof_bytes)) => {
                info!(
                    game = %game_address,
                    proof_len = proof_bytes.len(),
                    action = intent.label(),
                    "proof ready, submitting dispute transaction"
                );
                if targets_tee && was_awaiting {
                    ChallengerMetrics::tee_proof_obtained_total().increment(1);
                }
                proof_bytes
            }
            Some(ProofUpdate::NeedsRetry) => {
                return self.handle_proof_retry(game_address).await;
            }
            Some(ProofUpdate::Pending) | None => unreachable!("handled above"),
        };

        let result = submitter
            .submit_dispute(game_address, proof_bytes, invalid_index, expected_root, intent)
            .await;
        match result {
            Ok(_) => {
                self.pending_proofs.remove(&game_address);
            }
            Err(e) => {
                match &e {
                    ChallengeSubmitError::KnownRevert(
                        revert @ (KnownRevert::GameAlreadyExists
                        | KnownRevert::ProofAlreadyVerified),
                    ) => {
                        info!(
                            error = %e,
                            game = %game_address,
                            revert = ?revert,
                            "dispute already resolved, dropping pending proof"
                        );
                        self.pending_proofs.remove(&game_address);
                        return Ok(());
                    }
                    ChallengeSubmitError::KnownRevert(
                        revert @ (KnownRevert::InvalidParentGame | KnownRevert::L1OriginTooOld),
                    ) => {
                        warn!(
                            error = %e,
                            game = %game_address,
                            revert = ?revert,
                            "dispute cannot be resolved, ignoring game"
                        );
                        self.ignore_game(game_address);
                        return Ok(());
                    }
                    ChallengeSubmitError::KnownRevert(KnownRevert::InvalidSigner)
                        if !targets_tee =>
                    {
                        warn!(
                            error = %e,
                            game = %game_address,
                            "dispute proof signer is invalid onchain, dropping pending proof"
                        );
                        self.pending_proofs.remove(&game_address);
                        return Ok(());
                    }
                    _ if targets_tee && Self::should_fallback_from_tee_submit(&e) => {
                        warn!(
                            error = %e,
                            game = %game_address,
                            "TEE dispute tx failed, falling back to ZK"
                        );
                    }
                    _ if targets_tee => {
                        let Some(pending) = self.pending_proofs.get_mut(&game_address) else {
                            return Ok(());
                        };
                        let has_zk_fallback =
                            matches!(&pending.kind, ProofKind::Tee { zk_fallback: Some(_) });

                        if pending.tee_submit_retry_count >= self.tee_submit_retry_limit {
                            warn!(
                                error = %e,
                                game = %game_address,
                                retry_count = pending.tee_submit_retry_count,
                                retry_limit = self.tee_submit_retry_limit,
                                has_zk_fallback,
                                "TEE dispute tx retry limit reached"
                            );
                            pending.phase = ProofPhase::NeedsRetry;
                            return self.handle_proof_retry(game_address).await;
                        }

                        pending.tee_submit_retry_count =
                            pending.tee_submit_retry_count.saturating_add(1);
                        warn!(
                            error = %e,
                            game = %game_address,
                            retry_count = pending.tee_submit_retry_count,
                            retry_limit = self.tee_submit_retry_limit,
                            has_zk_fallback,
                            "TEE dispute tx failed, will retry next tick"
                        );
                        return Ok(());
                    }
                    _ => {
                        warn!(
                            error = %e,
                            game = %game_address,
                            "dispute tx failed, will retry next tick"
                        );
                        return Ok(());
                    }
                }

                if let Some(pending) = self.pending_proofs.get_mut(&game_address) {
                    pending.phase = ProofPhase::NeedsRetry;
                    return self.handle_proof_retry(game_address).await;
                }
            }
        }

        Ok(())
    }

    const fn should_fallback_from_tee_submit(error: &ChallengeSubmitError) -> bool {
        matches!(
            error,
            ChallengeSubmitError::KnownRevert(KnownRevert::InvalidSigner)
                | ChallengeSubmitError::TxReverted { .. }
                | ChallengeSubmitError::TxManager(TxManagerError::ExecutionReverted { .. })
        )
    }

    fn ignore_game(&mut self, game_address: Address) {
        self.pending_proofs.remove(&game_address);
        if self.ignored_games.insert(game_address) {
            self.ignored_game_order.push_back(game_address);
        }
        while self.ignored_games.len() > Self::MAX_IGNORED_GAMES {
            if let Some(game_address) = self.ignored_game_order.pop_front() {
                self.ignored_games.remove(&game_address);
            }
        }
        ChallengerMetrics::ignored_games().set(self.ignored_games.len() as f64);
    }

    async fn handle_proof_retry(&mut self, game_address: Address) -> eyre::Result<()> {
        let pending = match self.pending_proofs.get(&game_address) {
            Some(p) => p,
            None => return Ok(()),
        };

        let retry_count = pending.retry_count;
        let invalid_index = pending.invalid_index;

        if retry_count > Self::MAX_PROOF_RETRIES {
            warn!(
                game = %game_address,
                retry_count = retry_count,
                "proof retries exhausted, dropping entry"
            );
            ChallengerMetrics::proof_retries_exhausted_total().increment(1);
            self.pending_proofs.remove(&game_address);
            return Ok(());
        }

        let request = match &pending.kind {
            ProofKind::Tee { zk_fallback } => {
                let Some((fallback_request, fallback_intent)) = zk_fallback.clone() else {
                    debug!(
                        game = %game_address,
                        "TEE proof has no ZK fallback request, dropping entry"
                    );
                    self.pending_proofs.remove(&game_address);
                    return Ok(());
                };

                debug!(game = %game_address, "TEE proof needs retry, falling back to ZK");
                ChallengerMetrics::tee_proof_fallback_total().increment(1);

                if let Some(pending) = self.pending_proofs.get_mut(&game_address) {
                    pending.kind = ProofKind::Zk { prove_request: fallback_request.clone() };
                    pending.intent = fallback_intent;
                    pending.retry_count = 0;
                    pending.tee_submit_retry_count = 0;
                }

                fallback_request
            }
            ProofKind::Zk { prove_request } => prove_request.clone(),
        };

        ChallengerMetrics::proof_retries_total().increment(1);

        let prove_request = ChallengerProofAdapter::snark_plonk_prove_block_range_request(
            game_address,
            invalid_index,
            request,
        );

        match self.proof_requester.prove_block_range(prove_request).await {
            Ok(response) => {
                info!(
                    game = %game_address,
                    session_id = %response.session_id,
                    retry_count = retry_count,
                    "proof job re-initiated"
                );
                if let Some(pending) = self.pending_proofs.get_mut(&game_address) {
                    pending.phase = ProofPhase::AwaitingProof {
                        session_id: response.session_id,
                        started_at: Instant::now(),
                    };
                }
            }
            Err(e) => {
                if let Some(pending) = self.pending_proofs.get_mut(&game_address) {
                    pending.retry_count += 1;
                }
                warn!(
                    error = %e,
                    game = %game_address,
                    retry_count = retry_count,
                    "proveBlockRange failed on retry, will retry next tick"
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, time::Duration};

    use alloy_primitives::{Address, B256, Bytes};
    use base_proof_contracts::{AggregateVerifierClient, GameStatus, l1_origin_too_old_selector};
    use base_proof_rpc::L1Provider;
    use base_prover_service_protocol::{SnarkPlonkProofRequest, ZkProofRequest, ZkVm};
    use base_tx_manager::TxManagerError;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockL1, MockL2Provider, MockTxManager, MockZkProofProvider, addr,
        mock_state, receipt_with_status,
    };

    type TestManager = DisputeProofManager<MockL2Provider, MockZkProofProvider>;
    type TestSubmitter = ChallengeSubmitter<MockTxManager>;

    fn proof_request() -> SnarkPlonkProofRequest {
        SnarkPlonkProofRequest {
            proof: ZkProofRequest {
                start_block_number: 100,
                number_of_blocks_to_prove: 10,
                sequence_window: None,
                l1_head: Some(B256::repeat_byte(0xAA)),
                intermediate_root_interval: Some(10),
                schedule_l2_block_number: None,
                zk_vm: ZkVm::Sp1,
                zk_backend: ZkBackend::Cluster,
            },
            prover_address: Address::repeat_byte(0xCC),
        }
    }

    fn manager_with_tx_error(
        error: TxManagerError,
    ) -> (TestManager, TestSubmitter, Arc<MockZkProofProvider>) {
        manager_with_tx_manager(MockTxManager::new(Err(error)))
    }

    fn manager_with_tx_manager(
        tx_manager: MockTxManager,
    ) -> (TestManager, TestSubmitter, Arc<MockZkProofProvider>) {
        let game_address = addr(0);
        let mut verifier_games = HashMap::new();
        verifier_games.insert(game_address, mock_state(GameStatus::InProgress, Address::ZERO, 100));
        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));
        let proof_requester = Arc::new(MockZkProofProvider::default());
        let manager = DisputeProofManager::new(
            OutputValidator::new(Arc::new(MockL2Provider::default())),
            Arc::clone(&proof_requester),
            Arc::new(MockL1::failure("unused")) as Arc<dyn L1Provider>,
            verifier as Arc<dyn AggregateVerifierClient>,
            Duration::from_secs(60),
            3,
        );

        (manager, ChallengeSubmitter::new(tx_manager), proof_requester)
    }

    fn insert_ready_proof(manager: &mut TestManager) {
        manager.pending_proofs.insert(
            addr(0),
            PendingProof::ready(
                Bytes::from(vec![0x01, 0xAA]),
                0,
                B256::repeat_byte(0x22),
                proof_request(),
                DisputeIntent::Challenge,
            ),
        );
    }

    fn insert_ready_tee_proof(manager: &mut TestManager, with_zk_fallback: bool) {
        manager.pending_proofs.insert(
            addr(0),
            PendingProof {
                phase: ProofPhase::ReadyToSubmit { proof_bytes: Bytes::from(vec![0x00, 0xAA]) },
                kind: ProofKind::Tee {
                    zk_fallback: with_zk_fallback
                        .then(|| (proof_request(), DisputeIntent::Nullify)),
                },
                invalid_index: 0,
                expected_root: B256::repeat_byte(0x22),
                retry_count: 0,
                tee_submit_retry_count: 0,
                intent: DisputeIntent::Nullify,
            },
        );
    }

    fn assert_ready_tee_proof(manager: &TestManager) {
        let pending = manager.pending_proofs.get(&addr(0)).expect("pending proof should remain");
        assert!(matches!(pending.phase, ProofPhase::ReadyToSubmit { .. }));
        assert!(pending.kind.is_tee());
    }

    fn assert_zk_fallback_requested(manager: &TestManager, proof_requester: &MockZkProofProvider) {
        let pending = manager.pending_proofs.get(&addr(0)).expect("pending proof should remain");
        assert!(matches!(pending.phase, ProofPhase::AwaitingProof { .. }));
        assert!(!pending.kind.is_tee());
        assert_eq!(proof_requester.state.lock().unwrap().prove_block_range_log.len(), 1);
    }

    #[tokio::test]
    async fn proof_already_verified_revert_drops_pending_proof() {
        let (mut manager, submitter, _) =
            manager_with_tx_error(TxManagerError::ExecutionReverted {
                reason: Some("AlreadyProven(1)".to_string()),
                data: None,
            });
        insert_ready_proof(&mut manager);

        manager.poll_or_submit(addr(0), &submitter).await.unwrap();

        assert!(!manager.pending_proofs.contains_key(&addr(0)));
    }

    #[tokio::test]
    async fn game_already_exists_revert_drops_pending_proof() {
        let (mut manager, submitter, _) =
            manager_with_tx_error(TxManagerError::ExecutionReverted {
                reason: Some("GameAlreadyExists(0x00)".to_string()),
                data: None,
            });
        insert_ready_proof(&mut manager);

        manager.poll_or_submit(addr(0), &submitter).await.unwrap();

        assert!(!manager.pending_proofs.contains_key(&addr(0)));
    }

    #[tokio::test]
    async fn challenge_success_does_not_track_anchor_update() {
        let tx_hash = B256::repeat_byte(0x44);
        let (mut manager, submitter, _) =
            manager_with_tx_manager(MockTxManager::new(Ok(receipt_with_status(true, tx_hash))));
        insert_ready_proof(&mut manager);

        manager.poll_or_submit(addr(0), &submitter).await.unwrap();

        assert!(!manager.pending_proofs.contains_key(&addr(0)));
    }

    #[tokio::test]
    async fn stale_l1_origin_revert_drops_pending_zk_proof() {
        let (mut manager, submitter, proof_requester) =
            manager_with_tx_error(TxManagerError::ExecutionReverted {
                reason: None,
                data: Some(Bytes::from(l1_origin_too_old_selector().to_vec())),
            });
        insert_ready_proof(&mut manager);

        manager.poll_or_submit(addr(0), &submitter).await.unwrap();

        assert!(!manager.pending_proofs.contains_key(&addr(0)));
        assert!(manager.is_ignored(addr(0)));
        assert!(proof_requester.state.lock().unwrap().prove_block_range_log.is_empty());
    }

    #[tokio::test]
    async fn stale_l1_origin_revert_drops_pending_tee_proof_without_requesting_zk() {
        let (mut manager, submitter, proof_requester) =
            manager_with_tx_error(TxManagerError::ExecutionReverted {
                reason: None,
                data: Some(Bytes::from(l1_origin_too_old_selector().to_vec())),
            });
        insert_ready_tee_proof(&mut manager, true);

        manager.poll_or_submit(addr(0), &submitter).await.unwrap();

        assert!(!manager.pending_proofs.contains_key(&addr(0)));
        assert!(manager.is_ignored(addr(0)));
        assert!(proof_requester.state.lock().unwrap().prove_block_range_log.is_empty());
    }

    #[test]
    fn ignored_games_are_bounded() {
        let (mut manager, _, _) =
            manager_with_tx_manager(MockTxManager::new(Ok(receipt_with_status(true, B256::ZERO))));

        for i in 0..=TestManager::MAX_IGNORED_GAMES {
            manager.ignore_game(addr(i as u64));
        }

        assert_eq!(manager.ignored_games_len(), TestManager::MAX_IGNORED_GAMES);
        assert!(!manager.is_ignored(addr(0)));
        assert!(manager.is_ignored(addr(TestManager::MAX_IGNORED_GAMES as u64)));
    }

    #[tokio::test]
    async fn tee_submit_nonce_too_low_keeps_ready_proof_without_requesting_zk() {
        let (mut manager, submitter, proof_requester) =
            manager_with_tx_error(TxManagerError::NonceTooLow);
        insert_ready_tee_proof(&mut manager, true);

        manager.poll_or_submit(addr(0), &submitter).await.unwrap();

        assert_ready_tee_proof(&manager);
        let pending = manager.pending_proofs.get(&addr(0)).expect("pending proof should remain");
        assert_eq!(pending.tee_submit_retry_count, 1);
        assert!(proof_requester.state.lock().unwrap().prove_block_range_log.is_empty());
    }

    #[tokio::test]
    async fn tee_submit_retry_limit_falls_back_to_zk() {
        let (mut manager, submitter, proof_requester) =
            manager_with_tx_manager(MockTxManager::with_responses(vec![
                Err(TxManagerError::NonceTooLow),
                Err(TxManagerError::NonceTooLow),
            ]));
        manager.tee_submit_retry_limit = 1;
        insert_ready_tee_proof(&mut manager, true);

        manager.poll_or_submit(addr(0), &submitter).await.unwrap();

        assert_ready_tee_proof(&manager);
        assert!(proof_requester.state.lock().unwrap().prove_block_range_log.is_empty());

        manager.poll_or_submit(addr(0), &submitter).await.unwrap();

        assert_zk_fallback_requested(&manager, &proof_requester);
    }

    #[tokio::test]
    async fn tee_submit_retry_limit_drops_proof_without_zk_fallback() {
        let (mut manager, submitter, proof_requester) =
            manager_with_tx_manager(MockTxManager::with_responses(vec![
                Err(TxManagerError::NonceTooLow),
                Err(TxManagerError::NonceTooLow),
            ]));
        manager.tee_submit_retry_limit = 1;
        insert_ready_tee_proof(&mut manager, false);

        manager.poll_or_submit(addr(0), &submitter).await.unwrap();

        assert_ready_tee_proof(&manager);

        manager.poll_or_submit(addr(0), &submitter).await.unwrap();

        assert!(!manager.pending_proofs.contains_key(&addr(0)));
        assert!(proof_requester.state.lock().unwrap().prove_block_range_log.is_empty());
    }

    #[tokio::test]
    async fn tee_invalid_signer_revert_falls_back_to_zk() {
        let (mut manager, submitter, proof_requester) =
            manager_with_tx_error(TxManagerError::ExecutionReverted {
                reason: Some("InvalidSigner()".to_string()),
                data: None,
            });
        insert_ready_tee_proof(&mut manager, true);

        manager.poll_or_submit(addr(0), &submitter).await.unwrap();

        assert_zk_fallback_requested(&manager, &proof_requester);
    }

    #[tokio::test]
    async fn tee_mined_tx_revert_falls_back_to_zk() {
        let tx_hash = B256::repeat_byte(0x44);
        let (mut manager, submitter, proof_requester) =
            manager_with_tx_manager(MockTxManager::new(Ok(receipt_with_status(false, tx_hash))));
        insert_ready_tee_proof(&mut manager, true);

        manager.poll_or_submit(addr(0), &submitter).await.unwrap();

        assert_zk_fallback_requested(&manager, &proof_requester);
    }

    #[cfg(feature = "metrics")]
    mod metrics_emission {
        use metrics_util::{
            MetricKind,
            debugging::{DebugValue, DebuggingRecorder},
        };

        use super::*;

        #[test]
        fn proof_exhaustion_emits_metric() {
            let recorder = DebuggingRecorder::new();
            let snapshotter = recorder.snapshotter();
            metrics::with_local_recorder(&recorder, || {
                let runtime =
                    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                runtime.block_on(async {
                    let (mut manager, _, _) = manager_with_tx_manager(MockTxManager::new(Ok(
                        receipt_with_status(true, B256::ZERO),
                    )));
                    insert_ready_proof(&mut manager);

                    let entry = manager
                        .pending_proofs
                        .get_mut(&addr(0))
                        .expect("entry should exist after insertion");
                    entry.retry_count = TestManager::MAX_PROOF_RETRIES + 1;
                    entry.phase = ProofPhase::NeedsRetry;

                    manager.handle_proof_retry(addr(0)).await.unwrap();
                });

                let snapshot = snapshotter.snapshot().into_vec();
                let count = snapshot.iter().find_map(|(key, _, _, value)| {
                    if key.kind() != MetricKind::Counter
                        || key.key().name() != "base_challenger.proof_retries_exhausted_total"
                    {
                        return None;
                    }
                    match value {
                        DebugValue::Counter(value) => Some(*value),
                        _ => None,
                    }
                });

                assert_eq!(count, Some(1));
            });
        }
    }
}
