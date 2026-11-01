//! Dispute-proof lifecycle management.
//!
//! [`DisputeProofManager`] owns dispute attempts from initiation
//! through proof polling, retries, TEE-to-ZK fallback, and coordination of
//! onchain dispute submission. The [`Driver`](crate::Driver) remains
//! responsible for scanning candidate games.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256};
use base_proof_contracts::AggregateVerifierClient;
use base_proof_primitives::ProofRequest as TeeProofRequest;
use base_proof_rpc::{L1Provider, L2Provider};
use base_prover_service_client::ProofRequesterProvider;
use base_prover_service_protocol::{
    GetProofRequest, SnarkPlonkProofRequest, ZkBackend, ZkProofRequest, ZkVm,
};
use base_tx_manager::TxManager;
use tracing::{info, warn};

use crate::{
    Action, AttemptLimits, AttemptReporter, CandidateGame, ChallengeSubmitter, ChallengerMetrics,
    ChallengerProofAdapter, DisputeAttempt, DisputeIntent, DisputeTarget, Event, Need,
    OutputValidator, ZkFallback,
};

/// Manages dispute attempts for invalid games.
pub struct DisputeProofManager<L2: L2Provider, P: ProofRequesterProvider> {
    /// Validates output roots and constructs TEE proof commitments.
    validator: OutputValidator<L2>,
    /// Prover-service requester used to generate and poll fault proofs.
    proof_requester: Arc<P>,
    /// L1 provider used to construct TEE proof requests.
    l1_provider: Arc<dyn L1Provider>,
    verifier_client: Arc<dyn AggregateVerifierClient>,
    /// Dispute attempts keyed by game address.
    attempts: HashMap<Address, DisputeAttempt>,
    ignored_games: HashSet<Address>,
    ignored_game_order: VecDeque<Address>,
    max_proof_duration: Duration,
    tee_submit_retry_limit: u32,
}

impl<L2: L2Provider, P: ProofRequesterProvider> std::fmt::Debug for DisputeProofManager<L2, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisputeProofManager")
            .field("pending_proofs", &self.attempts.len())
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
            attempts: HashMap::new(),
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

    /// Returns whether a game has an active dispute attempt.
    pub fn has_pending_proof(&self, game_address: Address) -> bool {
        self.attempts.contains_key(&game_address)
    }

    /// Returns the number of active dispute attempts.
    pub fn pending_proofs_len(&self) -> usize {
        self.attempts.len()
    }

    /// Returns the number of terminally ignored games retained in memory.
    pub fn ignored_games_len(&self) -> usize {
        self.ignored_games.len()
    }

    /// Polls all active dispute attempts for completion or retries submission.
    pub async fn poll_pending_proofs<T: TxManager>(&mut self, submitter: &ChallengeSubmitter<T>) {
        let addresses: Vec<_> = self.attempts.keys().copied().collect();

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
                            self.attempts.insert(
                                game_address,
                                DisputeAttempt::awaiting_tee(
                                    response.session_id,
                                    DisputeTarget { invalid_index, expected_root },
                                    zk_fallback
                                        .map(|(request, intent)| ZkFallback { request, intent }),
                                    self.attempt_limits(),
                                    Instant::now(),
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

        self.attempts.insert(
            game_address,
            DisputeAttempt::awaiting_zk(
                prove_response.session_id,
                DisputeTarget { invalid_index, expected_root },
                proof_request,
                intent,
                self.attempt_limits(),
                Instant::now(),
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
        let intermediate_block_interval = candidate.intermediate_block_interval;

        let claimed_l2_block_number = start_block_number
            .checked_add(intermediate_block_interval.get())
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
            l1_head_number,
            schedule_l2_block_number: Some(candidate.info.l2_block_number),
            intermediate_block_interval,
        })
    }

    fn build_zk_request(
        &self,
        prover_address: Address,
        candidate: &CandidateGame,
        invalid_index: u64,
    ) -> eyre::Result<SnarkPlonkProofRequest> {
        let start_block_number = candidate.checkpoint_start_block(invalid_index)?;
        let intermediate_block_interval = candidate.intermediate_block_interval;

        // A dispute proves the one segment ending at the disputed root.
        Ok(SnarkPlonkProofRequest {
            proof: ZkProofRequest {
                start_block_number,
                number_of_blocks_to_prove: intermediate_block_interval.get(),
                sequence_window: None,
                l1_head: Some(candidate.l1_head),
                schedule_l2_block_number: Some(candidate.info.l2_block_number),
                intermediate_block_interval,
                zk_vm: ZkVm::Sp1,
                zk_backend: ZkBackend::Cluster,
            },
            prover_address,
        })
    }

    const fn attempt_limits(&self) -> AttemptLimits {
        AttemptLimits {
            max_proof_duration: self.max_proof_duration,
            tee_submit_retry_limit: self.tee_submit_retry_limit,
            max_proof_retries: Self::MAX_PROOF_RETRIES,
        }
    }

    async fn poll_or_submit<T: TxManager>(
        &mut self,
        game_address: Address,
        submitter: &ChallengeSubmitter<T>,
    ) -> eyre::Result<()> {
        let now = Instant::now();
        loop {
            let Some(attempt) = self.attempts.get_mut(&game_address) else { return Ok(()) };
            let event = match attempt.need(now) {
                Need::Idle => return Ok(()),
                Need::PollProver(session_id) => Event::ProverStatus(Box::new(
                    self.proof_requester.get_proof(GetProofRequest { session_id }).await?,
                )),
                Need::CheckChain { timeout } => {
                    if let Some(reason) = timeout {
                        AttemptReporter::retry(game_address, &reason);
                    }
                    let (status, tee_prover, zk_prover) = tokio::try_join!(
                        self.verifier_client.status(game_address),
                        self.verifier_client.tee_prover(game_address),
                        self.verifier_client.zk_prover(game_address),
                    )?;
                    Event::ChainState { status, tee_prover, zk_prover }
                }
            };
            let mut action = attempt.apply(event, now);
            loop {
                AttemptReporter::action(game_address, &action);
                let event = match action {
                    Action::Wait(_) => break,
                    Action::Submit { proof_bytes, target, intent, .. } => Event::Submitted(
                        submitter
                            .submit_dispute(
                                game_address,
                                proof_bytes,
                                target.invalid_index,
                                target.expected_root,
                                intent,
                            )
                            .await
                            .map(|_| ()),
                    ),
                    Action::Reprove(reprove) => {
                        let request = ChallengerProofAdapter::snark_plonk_prove_block_range_request(
                            game_address,
                            reprove.invalid_index,
                            reprove.request,
                        );
                        Event::ReproveStarted(
                            self.proof_requester
                                .prove_block_range(request)
                                .await
                                .map(|response| response.session_id)
                                .map_err(Into::into),
                        )
                    }
                    Action::Drop(_) => {
                        self.attempts.remove(&game_address);
                        return Ok(());
                    }
                    Action::Ignore(_) => {
                        self.ignore_game(game_address);
                        return Ok(());
                    }
                };
                action = self
                    .attempts
                    .get_mut(&game_address)
                    .expect("active attempt")
                    .apply(event, Instant::now());
            }
        }
    }

    fn ignore_game(&mut self, game_address: Address) {
        self.attempts.remove(&game_address);
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
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, time::Duration};

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockL1, MockL2Provider, MockZkProofProvider, addr,
    };

    type TestManager = DisputeProofManager<MockL2Provider, MockZkProofProvider>;

    #[test]
    fn ignored_games_are_bounded() {
        let mut manager = TestManager::new(
            OutputValidator::new(Arc::new(MockL2Provider::default())),
            Arc::new(MockZkProofProvider::default()),
            Arc::new(MockL1::failure("unused")),
            Arc::new(MockAggregateVerifier::new(HashMap::new())),
            Duration::from_secs(60),
            3,
        );

        for i in 0..=TestManager::MAX_IGNORED_GAMES {
            manager.ignore_game(addr(i as u64));
        }

        assert_eq!(manager.ignored_games_len(), TestManager::MAX_IGNORED_GAMES);
        assert!(!manager.is_ignored(addr(0)));
        assert!(manager.is_ignored(addr(TestManager::MAX_IGNORED_GAMES as u64)));
    }

    #[cfg(feature = "metrics")]
    mod metrics_tests {
        use std::{num::NonZeroU64, sync::Mutex};

        use alloy_primitives::Bytes;
        use base_proof_contracts::invalid_proof_selector;
        use base_proof_primitives::Proposal;
        use base_proof_submission::test_utils::SnarkReceiptFixture;
        use base_prover_service_protocol::{ProofResult, ProofStatus, TeeKind, TeeProofResult};
        use base_tx_manager::TxManagerError;
        use metrics_util::{
            MetricKind,
            debugging::{DebugValue, DebuggingRecorder},
        };
        use tracing::{Event as TracingEvent, Level, field::Field};
        use tracing_subscriber::{Layer, Registry, layer::Context, prelude::*};

        use super::*;
        use crate::test_utils::{MockGameState, MockTxManager, MockZkProofState};

        #[derive(Debug)]
        struct CapturedEvent {
            level: Level,
            fields: HashMap<String, String>,
        }

        struct CaptureEvents(Arc<Mutex<Vec<CapturedEvent>>>);

        impl Layer<Registry> for CaptureEvents {
            fn on_event(&self, event: &TracingEvent<'_>, _context: Context<'_, Registry>) {
                let mut fields = HashMap::new();
                event.record(&mut |field: &Field, value: &dyn std::fmt::Debug| {
                    fields.insert(field.name().to_owned(), format!("{value:?}"));
                });
                self.0
                    .lock()
                    .unwrap()
                    .push(CapturedEvent { level: *event.metadata().level(), fields });
            }
        }

        #[test]
        fn poll_zk_invalid_proof_logs_error_counts_and_ignores_game() {
            let game = addr(42);
            let events = Arc::new(Mutex::new(Vec::new()));
            let subscriber = Registry::default().with(CaptureEvents(Arc::clone(&events)));
            let _subscriber = tracing::subscriber::set_default(subscriber);
            let recorder = DebuggingRecorder::new();
            let snapshotter = recorder.snapshotter();
            metrics::with_local_recorder(&recorder, || {
                let runtime =
                    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                runtime.block_on(async {
                    let mut manager = TestManager::new(
                        OutputValidator::new(Arc::new(MockL2Provider::default())),
                        Arc::new(MockZkProofProvider {
                            state: Mutex::new(MockZkProofState {
                                proof_status: ProofStatus::Succeeded,
                                proof: SnarkReceiptFixture::plonk_receipt_bytes(
                                    [0x5a, 0x09, 0x3a, 0x2f],
                                    "dead",
                                ),
                                ..Default::default()
                            }),
                        }),
                        Arc::new(MockL1::failure("unused")),
                        Arc::new(MockAggregateVerifier::new(HashMap::from([(
                            game,
                            MockGameState { tee_prover: addr(1), ..Default::default() },
                        )]))),
                        Duration::from_secs(60),
                        3,
                    );
                    manager.attempts.insert(
                        game,
                        DisputeAttempt::awaiting_zk(
                            "session".to_owned(),
                            DisputeTarget {
                                invalid_index: 7,
                                expected_root: B256::repeat_byte(0xaa),
                            },
                            SnarkPlonkProofRequest {
                                proof: ZkProofRequest {
                                    start_block_number: 0,
                                    number_of_blocks_to_prove: 1,
                                    sequence_window: None,
                                    l1_head: None,
                                    schedule_l2_block_number: None,
                                    intermediate_block_interval: NonZeroU64::MIN,
                                    zk_vm: ZkVm::Sp1,
                                    zk_backend: ZkBackend::Cluster,
                                },
                                prover_address: Address::ZERO,
                            },
                            DisputeIntent::Challenge,
                            manager.attempt_limits(),
                            Instant::now(),
                        ),
                    );
                    let submitter = ChallengeSubmitter::new(MockTxManager::new(Err(
                        TxManagerError::ExecutionReverted {
                            reason: None,
                            data: Some(Bytes::from(invalid_proof_selector().to_vec())),
                        },
                    )));

                    manager.poll_pending_proofs(&submitter).await;

                    assert!(manager.is_ignored(game));
                });
            });

            let events = events.lock().unwrap();
            let errors: Vec<_> =
                events.iter().filter(|event| event.level == Level::ERROR).collect();
            assert_eq!(errors.len(), 1, "expected one ERROR event, got {events:?}");
            assert_eq!(
                errors[0].fields["message"],
                "onchain verifier rejected the ZK proof (InvalidProof); the prover \
                 likely runs a program other than the game's ZK hashes. Ignoring the \
                 game until restart; it remains unchallenged"
            );
            assert_eq!(errors[0].fields["game"], game.to_string());
            assert_eq!(errors[0].fields["invalid_index"], "7");
            let count =
                snapshotter.snapshot().into_vec().into_iter().find_map(|(key, _, _, value)| {
                    if key.kind() == MetricKind::Counter
                        && key.key().name() == "base_challenger.zk_invalid_proof_total"
                        && let DebugValue::Counter(count) = value
                    {
                        return Some(count);
                    }
                    None
                });
            assert_eq!(count, Some(1), "ZK InvalidProof must increment its counter once");
        }

        #[test]
        fn poll_emits_failure_metrics() {
            let expected_root = B256::repeat_byte(0xaa);
            let bad_proposal = Proposal {
                output_root: B256::repeat_byte(0xbb),
                signature: Bytes::from_static(&[0; 65]),
                l1_origin_hash: B256::ZERO,
                l1_origin_number: 0,
                l2_block_number: 0,
                prev_output_root: B256::ZERO,
                config_hash: B256::ZERO,
                schedule_id: B256::ZERO,
            };

            for (state, max_proof_duration, expected_reason) in [
                (
                    MockZkProofState::default(),
                    Duration::ZERO,
                    ChallengerMetrics::PROOF_FAILURE_TIMEOUT,
                ),
                (
                    MockZkProofState { proof_status: ProofStatus::Failed, ..Default::default() },
                    Duration::from_secs(3600),
                    ChallengerMetrics::PROOF_FAILURE_FAILED,
                ),
                (
                    MockZkProofState {
                        proof_status: ProofStatus::Succeeded,
                        omit_result_on_success: true,
                        ..Default::default()
                    },
                    Duration::from_secs(3600),
                    ChallengerMetrics::PROOF_FAILURE_MALFORMED,
                ),
                (
                    MockZkProofState {
                        proof_status: ProofStatus::Succeeded,
                        result: Some(ProofResult::Tee(TeeProofResult {
                            intermediate_roots: vec![bad_proposal.output_root],
                            aggregate_proposal: bad_proposal,
                            tee_kind: TeeKind::AwsNitro,
                            tee_signer: Address::repeat_byte(0x11),
                        })),
                        ..Default::default()
                    },
                    Duration::from_secs(3600),
                    ChallengerMetrics::PROOF_FAILURE_TEE_VALIDATION,
                ),
            ] {
                let recorder = DebuggingRecorder::new();
                let snapshotter = recorder.snapshotter();
                metrics::with_local_recorder(&recorder, || {
                    let runtime =
                        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    runtime.block_on(async {
                        let mut manager = TestManager::new(
                            OutputValidator::new(Arc::new(MockL2Provider::default())),
                            Arc::new(MockZkProofProvider { state: Mutex::new(state) }),
                            Arc::new(MockL1::failure("unused")),
                            Arc::new(MockAggregateVerifier::new(HashMap::from([(
                                addr(0),
                                MockGameState::default(),
                            )]))),
                            max_proof_duration,
                            3,
                        );
                        manager.attempts.insert(
                            addr(0),
                            DisputeAttempt::awaiting_tee(
                                "session".to_owned(),
                                DisputeTarget { invalid_index: 0, expected_root },
                                None,
                                manager.attempt_limits(),
                                Instant::now() - Duration::from_secs(1),
                            ),
                        );
                        let submitter =
                            ChallengeSubmitter::new(MockTxManager::with_responses(vec![]));
                        manager.poll_pending_proofs(&submitter).await;
                    });
                });
                let count =
                    snapshotter.snapshot().into_vec().into_iter().find_map(|(key, _, _, value)| {
                        if key.kind() == MetricKind::Counter
                            && key.key().name() == "base_challenger.proof_session_failures_total"
                            && key.key().labels().any(|label| {
                                label.key() == "reason" && label.value() == expected_reason
                            })
                            && let DebugValue::Counter(count) = value
                        {
                            return Some(count);
                        }
                        None
                    });
                assert_eq!(count, Some(1), "{expected_reason}");
            }
        }
    }
}
