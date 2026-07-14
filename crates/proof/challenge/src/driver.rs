//! Main driver loop for the challenger service.
//!
//! The [`Driver`] ties together all challenger components — scanning for
//! invalid dispute games, validating output roots, requesting proofs, and
//! submitting dispute transactions — into a single polling loop.
//!
//! Four dispute paths are supported:
//!
//! 1. **Wrong TEE proof** — nullify with a TEE proof (`nullify()`) or
//!    challenge with a ZK proof (`challenge()`).
//! 2. **Correct TEE proof challenged with a wrong ZK proof** — nullify
//!    the fraudulent ZK challenge with a ZK proof (`nullify()`).
//! 3. **Wrong ZK proposal** — nullify with a ZK proof (`nullify()`).
//! 4. **Wrong dual proposal (TEE + ZK, no challenge)** — nullify with a
//!    TEE proof first (`nullify()`), falling back to ZK `nullify()`.
//!    After the TEE proof is nullified, the game is re-scanned as Path 3.

use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256};
use base_proof_contracts::{AggregateVerifierClient, GameStatus};
use base_proof_primitives::ProofRequest as TeeProofRequest;
use base_proof_rpc::L2Provider;
use base_proof_submission::KnownRevert;
use base_prover_service_client::ProofRequesterProvider;
use base_prover_service_protocol::{
    SnarkPlonkProofRequest, TeeKind, ZkBackend, ZkProofRequest, ZkVm,
};
use base_runtime::{Clock, TokioRuntime};
use base_tx_manager::{TxManager, TxManagerError};
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    AnchorUpdater, BondManager, CandidateGame, ChallengeSubmitError, ChallengeSubmitter,
    ChallengerMetrics, DisputeIntent, GameCategory, GameScanner, IntermediateValidationParams,
    L1HeadProvider, OutputValidator, PendingProof, PendingProofs, ProofKind, ProofPhase,
    ProofUpdate, ValidatorError,
};

/// Configuration for the challenger [`Driver`].
#[derive(Debug)]
pub struct DriverConfig {
    /// How often the driver polls for new games.
    pub poll_interval: Duration,
    /// Maximum wall-clock time to wait for a ZK proof session before treating it as failed.
    pub max_proof_duration: Duration,
    /// Retryable TEE submission failures to tolerate before falling back to ZK.
    pub tee_submit_retry_limit: u32,
    /// Cancellation token for graceful shutdown.
    pub cancel: CancellationToken,
}

/// TEE proof configuration, bundling the provider and L1 head provider.
#[derive(Debug)]
pub struct TeeConfig {
    /// L1 head provider for fetching the finalized head hash.
    pub l1_head_provider: Arc<dyn L1HeadProvider>,
}

/// Service-layer dependencies injected into the [`Driver`].
pub struct DriverComponents<
    L2: L2Provider,
    P: ProofRequesterProvider,
    T: TxManager,
    C: Clock = TokioRuntime,
> {
    /// Scans for new dispute games on L1.
    pub scanner: GameScanner,
    /// Validates L2 output roots against the local node.
    pub validator: OutputValidator<L2>,
    /// Prover-service requester used to generate and poll ZK fault proofs.
    pub proof_requester: Arc<P>,
    /// Submits challenge transactions to L1.
    pub submitter: ChallengeSubmitter<T>,
    /// Optional TEE proof configuration (provider + L1 RPC client).
    pub tee: Option<TeeConfig>,
    /// Client for the aggregate verifier contract.
    pub verifier_client: Arc<dyn AggregateVerifierClient>,
    /// Bond lifecycle manager (optional; enabled when claim addresses are configured).
    pub bond_manager: Option<BondManager<C>>,
    /// Best-effort anchor state updater.
    pub anchor_updater: AnchorUpdater,
}

impl<L2: L2Provider, P: ProofRequesterProvider, T: TxManager, C: Clock> std::fmt::Debug
    for DriverComponents<L2, P, T, C>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverComponents")
            .field("scanner", &self.scanner)
            .field("tee", &self.tee.as_ref().map(|_| ".."))
            .field("bond_manager", &self.bond_manager.as_ref().map(|_| ".."))
            .finish_non_exhaustive()
    }
}

/// Orchestrates the challenger pipeline: scan, validate, prove, submit.
pub struct Driver<L2, P, T, C: Clock = TokioRuntime>
where
    L2: L2Provider,
    P: ProofRequesterProvider,
    T: TxManager,
{
    /// Scans for new dispute games on L1.
    pub scanner: GameScanner,
    /// Validates L2 output roots against the local node.
    pub validator: OutputValidator<L2>,
    /// Prover-service requester used to generate and poll ZK fault proofs.
    pub proof_requester: Arc<P>,
    /// Submits challenge transactions to L1.
    pub submitter: ChallengeSubmitter<T>,
    /// Optional TEE proof configuration (provider + L1 RPC client).
    pub tee: Option<TeeConfig>,
    /// Client for the aggregate verifier contract.
    pub verifier_client: Arc<dyn AggregateVerifierClient>,
    /// In-flight proof sessions keyed by game address.
    pub pending_proofs: PendingProofs,
    /// Games that hit terminal contract reverts and should not be rediscovered.
    ignored_games: HashSet<Address>,
    ignored_game_order: VecDeque<Address>,
    /// Bond lifecycle manager (optional; enabled when claim addresses are configured).
    pub bond_manager: Option<BondManager<C>>,
    /// Best-effort anchor state updater.
    pub anchor_updater: AnchorUpdater,
    /// Interval between polling cycles.
    pub poll_interval: Duration,
    /// Maximum wall-clock time to wait for a ZK proof session before treating it as failed.
    pub max_proof_duration: Duration,
    /// Retryable TEE submission failures to tolerate before falling back to ZK.
    pub tee_submit_retry_limit: u32,
    /// Token used to signal graceful shutdown.
    pub cancel: CancellationToken,
}

impl<L2: L2Provider, P: ProofRequesterProvider, T: TxManager, C: Clock> std::fmt::Debug
    for Driver<L2, P, T, C>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Driver")
            .field("pending_proofs", &self.pending_proofs.len())
            .field("poll_interval", &self.poll_interval)
            .field("tee_submit_retry_limit", &self.tee_submit_retry_limit)
            .finish_non_exhaustive()
    }
}

impl<L2: L2Provider, P: ProofRequesterProvider, T: TxManager, C: Clock> Driver<L2, P, T, C> {
    /// Maximum number of times a failed proof job will be retried before being dropped.
    pub const MAX_PROOF_RETRIES: u32 = 3;

    /// Maximum number of terminally ignored games retained to avoid rediscovery churn.
    ///
    /// Evicted games may be rediscovered by a later scan, then re-ignored after
    /// one check.
    pub const MAX_IGNORED_GAMES: usize = 10_000;

    /// Creates a new driver with the given components.
    pub fn new(config: DriverConfig, components: DriverComponents<L2, P, T, C>) -> Self {
        Self {
            scanner: components.scanner,
            validator: components.validator,
            proof_requester: components.proof_requester,
            submitter: components.submitter,
            tee: components.tee,
            verifier_client: components.verifier_client,
            pending_proofs: PendingProofs::new(),
            ignored_games: HashSet::new(),
            ignored_game_order: VecDeque::new(),
            bond_manager: components.bond_manager,
            anchor_updater: components.anchor_updater,
            poll_interval: config.poll_interval,
            max_proof_duration: config.max_proof_duration,
            tee_submit_retry_limit: config.tee_submit_retry_limit,
            cancel: config.cancel,
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

            ChallengerMetrics::pending_proofs().set(self.pending_proofs.len() as f64);
            ChallengerMetrics::ignored_games().set(self.ignored_games.len() as f64);

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
    /// scan batch, then scans recent games for claimable bonds, polls anchor
    /// updates, and finally scans for new candidates and processes them.
    pub async fn step(&mut self) -> eyre::Result<()> {
        self.poll_pending_proofs().await;
        self.discover_claimable_bonds().await;
        self.anchor_updater.poll(&*self.verifier_client, &self.submitter).await;

        let candidates = self.scanner.scan().await?;

        for candidate in candidates {
            let index = candidate.index;
            if let Err(e) = self.process_candidate(candidate).await {
                warn!(error = %e, game_index = index, "failed to process candidate");
            }
        }

        Ok(())
    }

    /// Scans recent games from scratch and advances ready bond claims.
    async fn discover_claimable_bonds(&mut self) {
        let Some(ref mut bond_manager) = self.bond_manager else {
            return;
        };

        match bond_manager.discover_claimable_games(&*self.verifier_client, &self.submitter).await {
            Ok(_) => {}
            Err(e) => warn!(error = %e, "bond discovery scan failed"),
        }
    }

    /// Polls all in-flight proof sessions for completion or retries submission.
    async fn poll_pending_proofs(&mut self) {
        let addresses = self.pending_proofs.addresses();

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

    /// Processes a single candidate game by dispatching to the appropriate
    /// handler based on the game's [`GameCategory`].
    async fn process_candidate(&mut self, candidate: CandidateGame) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;

        if self.ignored_games.contains(&game_address) {
            debug!(game = %game_address, "skipping ignored game");
            return Ok(());
        }

        // If this game already has an in-flight proof session, skip it.
        // Pending proofs are polled separately in `poll_pending_proofs`.
        if self.pending_proofs.contains_key(&game_address) {
            debug!(game = %game_address, "skipping game with pending proof session");
            return Ok(());
        }

        match candidate.category {
            GameCategory::InvalidTeeProposal => {
                self.process_invalid_proposal(candidate, DisputeIntent::Challenge).await
            }
            GameCategory::FraudulentZkChallenge { challenged_index } => {
                self.process_fraudulent_zk_challenge(candidate, challenged_index).await
            }
            GameCategory::InvalidZkProposal | GameCategory::InvalidDualProposal => {
                self.process_invalid_proposal(candidate, DisputeIntent::Nullify).await
            }
        }
    }

    /// Fetches intermediate roots and validates them against the local L2 node.
    ///
    /// Returns `Ok(Some((result, roots)))` when validation completes, or
    /// `Ok(None)` when a transient error (e.g. block not yet available) means
    /// the game should be skipped this tick. Permanent errors are propagated.
    async fn validate_game(
        &self,
        candidate: &CandidateGame,
    ) -> eyre::Result<Option<(crate::ValidationResult, Vec<B256>)>> {
        let game_address = candidate.factory.proxy;

        let intermediate_roots =
            self.verifier_client.intermediate_output_roots(game_address).await?;

        let params = IntermediateValidationParams {
            game_address,
            starting_block_number: candidate.starting_block_number,
            l2_block_number: candidate.info.l2_block_number,
            intermediate_block_interval: candidate.intermediate_block_interval,
            claimed_root: candidate.info.root_claim,
            intermediate_roots: &intermediate_roots,
        };

        match self.validator.validate_intermediate_roots(params).await {
            Ok(result) => Ok(Some((result, intermediate_roots))),
            Err(e) => {
                match &e {
                    // Transient: the L2 node has not produced this block yet.
                    // Safe to skip — the next scan tick will retry.
                    ValidatorError::BlockNotAvailable { .. } => {
                        debug!(
                            game = %game_address,
                            error = %e,
                            "block not yet available, skipping game"
                        );
                        Ok(None)
                    }

                    // Persistent configuration errors: these indicate a
                    // mismatch between the cached interval and onchain
                    // state (e.g. after a governance `setImplementation`
                    // that changed `INTERMEDIATE_BLOCK_INTERVAL`).
                    // Propagate so the caller logs the error at game level
                    // and operators are alerted. No log here — the caller
                    // in `step()` already logs at warn level.
                    ValidatorError::CheckpointCountMismatch { .. }
                    | ValidatorError::InvalidInterval
                    | ValidatorError::InvalidBlockRange { .. } => Err(e.into()),

                    // Other errors (RPC, header hash mismatch, account
                    // proof failure, arithmetic overflow) are potentially
                    // transient — skip and retry on the next tick.
                    _ => {
                        warn!(
                            game = %game_address,
                            error = %e,
                            "transient validation error, skipping game"
                        );
                        Ok(None)
                    }
                }
            }
        }
    }

    /// Processes a game whose proposal may contain an invalid intermediate
    /// root (Path 1: wrong TEE proof, Path 3: wrong ZK proof, Path 4:
    /// wrong dual proposal).
    ///
    /// Validates the intermediate roots against the local L2 node. If a
    /// mismatch is found, initiates a proof with the given `intent`.
    async fn process_invalid_proposal(
        &mut self,
        candidate: CandidateGame,
        intent: DisputeIntent,
    ) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;

        let result = match self.validate_game(&candidate).await? {
            Some((result, _)) => result,
            None => return Ok(()),
        };

        if result.is_valid {
            debug!(game = %game_address, "game output roots are valid");
            return Ok(());
        }

        let invalid_index =
            u64::try_from(result.invalid_intermediate_index.ok_or_else(|| {
                eyre::eyre!("invalid result missing invalid_intermediate_index")
            })?)?;
        let expected_root = result.expected_root;

        info!(
            game = %game_address,
            invalid_index = invalid_index,
            expected_root = %expected_root,
            intent = ?intent,
            "invalid intermediate root detected, requesting proof"
        );

        let try_tee_first = match candidate.category {
            GameCategory::InvalidTeeProposal => {
                ChallengerMetrics::invalid_tee_proposal_detected_total().increment(1);
                true
            }
            GameCategory::InvalidZkProposal => {
                ChallengerMetrics::invalid_zk_proposal_detected_total().increment(1);
                false
            }
            GameCategory::InvalidDualProposal => {
                ChallengerMetrics::invalid_dual_proposal_detected_total().increment(1);
                true
            }
            GameCategory::FraudulentZkChallenge { .. } => {
                error!(
                    category = ?candidate.category,
                    game = %game_address,
                    "unexpected category in process_invalid_proposal"
                );
                debug_assert!(
                    false,
                    "unexpected category in process_invalid_proposal: {:?}",
                    candidate.category
                );
                return Err(eyre::eyre!(
                    "unexpected category in process_invalid_proposal: {:?}",
                    candidate.category
                ));
            }
        };

        self.initiate_proof(candidate, invalid_index, expected_root, intent, try_tee_first).await
    }

    /// Processes a game whose correct TEE proposal has been challenged with
    /// a potentially fraudulent ZK proof (Path 2).
    ///
    /// Validates the originally proposed root at the challenged index. If the
    /// original root is correct, the ZK challenge was fraudulent and a ZK
    /// proof is submitted via `nullify()` to refute it.
    async fn process_fraudulent_zk_challenge(
        &mut self,
        candidate: CandidateGame,
        challenged_index: u64,
    ) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;

        // Fetch only the challenged onchain intermediate root (not all roots).
        let on_chain_root =
            self.verifier_client.intermediate_output_root(game_address, challenged_index).await?;

        // Validate only the challenged root — not all intermediate roots.
        // The checkpoint block for index `i` is:
        //   starting_block + interval * (i + 1)
        let checkpoint_block = candidate.checkpoint_start_block(challenged_index + 1)?;

        let validation = match self
            .validator
            .validate_claimed_root_at_block(game_address, checkpoint_block, on_chain_root)
            .await
        {
            Ok(result) => result,
            Err(ValidatorError::BlockNotAvailable { .. }) => {
                debug!(
                    game = %game_address,
                    block = checkpoint_block,
                    "block not yet available, skipping game"
                );
                return Ok(());
            }
            Err(e) => {
                warn!(
                    game = %game_address,
                    block = checkpoint_block,
                    error = %e,
                    "output root computation failed, skipping game"
                );
                return Ok(());
            }
        };

        // If the onchain root at the challenged index does not match the
        // locally computed root, the ZK challenge was legitimate — skip.
        if !validation.is_valid {
            debug!(
                game = %game_address,
                challenged_index = challenged_index,
                on_chain = %on_chain_root,
                expected = %validation.expected_root,
                "ZK challenge is legitimate (challenged root was wrong), skipping"
            );
            return Ok(());
        }

        info!(
            game = %game_address,
            challenged_index = challenged_index,
            on_chain_root = %on_chain_root,
            "fraudulent ZK challenge detected, nullifying with ZK proof"
        );

        ChallengerMetrics::fraudulent_zk_challenge_detected_total().increment(1);

        self.initiate_zk_proof(
            candidate,
            challenged_index,
            validation.expected_root,
            DisputeIntent::Nullify,
        )
        .await
    }

    /// Attempts TEE-first proof sourcing with ZK fallback.
    ///
    /// The `intent` determines the onchain action for the ZK fallback path.
    /// TEE proofs always use `nullify()` regardless of `intent`.
    ///
    /// When `try_tee_first` is `true` and the game has a non-zero TEE prover,
    /// a synchronous TEE proof is attempted before falling back to ZK.
    /// Path 1 (`InvalidTeeProposal`) sets `try_tee_first = true` with
    /// `intent = Challenge`; Path 4 (`InvalidDualProposal`) sets
    /// `try_tee_first = true` with `intent = Nullify` so the ZK fallback
    /// also calls `nullify()`.
    async fn initiate_proof(
        &mut self,
        candidate: CandidateGame,
        invalid_index: u64,
        expected_root: B256,
        intent: DisputeIntent,
        try_tee_first: bool,
    ) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;

        // TEE-first: try if the game has a TEE prover, we have a TEE config,
        // and the caller opted in. Path 1 (InvalidTeeProposal) and Path 4
        // (InvalidDualProposal) set `try_tee_first = true`.
        if candidate.tee_prover != Address::ZERO
            && try_tee_first
            && let Some(tee) = &self.tee
        {
            ChallengerMetrics::tee_proof_attempts_total().increment(1);
            match self.build_tee_request(&candidate, invalid_index, expected_root, tee).await {
                Ok(tee_request) => {
                    let (zk_fallback_request, zk_fallback_intent) =
                        match self.build_zk_request(&candidate, invalid_index) {
                            Ok(req) => (Some(req), Some(intent)),
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    game = %game_address,
                                    "failed to build ZK fallback request; \
                                     TEE proof will have no ZK fallback"
                                );
                                (None, None)
                            }
                        };

                    let request = crate::ChallengerProofAdapter::tee_prove_block_range_request(
                        game_address,
                        invalid_index,
                        tee_request,
                        TeeKind::AwsNitro,
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
                                    zk_fallback_request,
                                    zk_fallback_intent,
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

        // ZK fallback (or direct ZK if no TEE prover / intent is Nullify).
        self.initiate_zk_proof(candidate, invalid_index, expected_root, intent).await
    }

    /// Builds a TEE proof request for the given candidate game.
    async fn build_tee_request(
        &self,
        candidate: &CandidateGame,
        invalid_index: u64,
        expected_root: B256,
        tee: &TeeConfig,
    ) -> eyre::Result<TeeProofRequest> {
        let start_block_number = candidate.checkpoint_start_block(invalid_index)?;

        let claimed_l2_block_number = start_block_number
            .checked_add(candidate.intermediate_block_interval)
            .ok_or_else(|| eyre::eyre!("claimed_l2_block_number overflow"))?;

        // Use the game's stored L1 head (from CWIA) so the enclave signs a
        // journal whose `l1OriginHash` matches what the onchain `nullify()`
        // will use for verification. Look up its block number concurrently
        // with the agreed L2 state computation.
        let l1_head = candidate.l1_head;
        let (l1_head_number_result, output_root_result) = tokio::join!(
            tee.l1_head_provider.block_number_by_hash(l1_head),
            self.validator.compute_output_root_with_hash(start_block_number),
        );
        let l1_head_number = l1_head_number_result?;
        let (agreed_l2_head_hash, agreed_l2_output_root) = output_root_result?;

        Ok(TeeProofRequest {
            l1_head,
            agreed_l2_head_hash,
            agreed_l2_output_root,
            claimed_l2_output_root: expected_root,
            claimed_l2_block_number,
            proposer: self.submitter.sender_address(),
            intermediate_block_interval: candidate.intermediate_block_interval,
            l1_head_number,
            ..Default::default()
        })
    }

    /// Builds a [`SnarkPlonkProofRequest`] for the given candidate and invalid index.
    fn build_zk_request(
        &self,
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
                zk_vm: ZkVm::Sp1,
                zk_backend: ZkBackend::Cluster,
            },
            prover_address: self.submitter.sender_address(),
        })
    }

    /// Requests a ZK proof, stores the session, and polls for the result.
    async fn initiate_zk_proof(
        &mut self,
        candidate: CandidateGame,
        invalid_index: u64,
        expected_root: B256,
        intent: DisputeIntent,
    ) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;

        // The prior intermediate root (or the game's starting root when
        // invalid_index == 0) is a trusted anchor, so the ZK proof only
        // needs to cover the single interval that contains the invalid
        // checkpoint: [prior_checkpoint .. invalid_checkpoint].
        let proof_request = self.build_zk_request(&candidate, invalid_index)?;
        let request = crate::ChallengerProofAdapter::snark_plonk_prove_block_range_request(
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

        let pending = PendingProof::awaiting(
            prove_response.session_id,
            invalid_index,
            expected_root,
            proof_request,
            intent,
        );
        self.pending_proofs.insert(game_address, pending);

        Ok(())
    }

    /// Advances a pending proof through its lifecycle.
    ///
    /// - **`AwaitingProof`** — polls prover service:
    ///   - `Succeeded` → transitions to `ReadyToSubmit` and falls through to
    ///     submission.
    ///   - `Failed` → transitions to `NeedsRetry` so `proveBlockRange` is
    ///     re-initiated.
    ///   - Intermediate (`Queued`/`Running`) → returns early
    ///     without any contract calls.
    /// - **`ReadyToSubmit`** — submits the dispute tx based on the entry's
    ///   [`DisputeIntent`]:
    ///   - [`DisputeIntent::Nullify`] → calls `nullify()`.
    ///   - [`DisputeIntent::Challenge`] → calls `challenge()`.
    ///   - On success → removes the entry.
    ///   - On failure → leaves the entry so it is retried next tick.
    /// - **`NeedsRetry`** — re-initiates `proveBlockRange`:
    ///   - If `retry_count > MAX_PROOF_RETRIES` → drops the entry.
    ///   - Otherwise → calls `proveBlockRange` and transitions to `AwaitingProof`.
    async fn poll_or_submit(&mut self, game_address: Address) -> eyre::Result<()> {
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

        // Poll proof status first — if still pending, skip the contract
        // calls that check game liveness. This avoids 3 RPC round-trips
        // per tick for proofs that are not yet ready.
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

        // The proof is ready or needs retry — verify the game is still
        // actionable before doing any work.
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

        // Nullification zeroes only the targeted prover (TEE or ZK) but
        // does NOT change the game status (it stays IN_PROGRESS), so
        // checking status alone would cause infinite retries. Check the
        // relevant prover slot to detect prior resolution.
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

        // Dispatch based on proof status.
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
            // Pending and None already handled above.
            Some(ProofUpdate::Pending) | None => unreachable!("handled above"),
        };

        let result = self
            .submitter
            .submit_dispute(game_address, proof_bytes, invalid_index, expected_root, intent)
            .await;
        match result {
            Ok(_) => {
                self.pending_proofs.remove(&game_address);
            }
            Err(e) => {
                let known_revert = match &e {
                    ChallengeSubmitError::KnownRevert(revert) => Some(*revert),
                    ChallengeSubmitError::TxReverted { .. }
                    | ChallengeSubmitError::TxManager(_) => None,
                };

                match known_revert {
                    Some(KnownRevert::GameAlreadyExists) => {
                        info!(
                            error = %e,
                            game = %game_address,
                            "dispute game already exists, dropping pending proof"
                        );
                        self.pending_proofs.remove(&game_address);
                        return Ok(());
                    }
                    Some(KnownRevert::ProofAlreadyVerified) => {
                        info!(
                            error = %e,
                            game = %game_address,
                            "dispute proof already verified onchain, dropping pending proof"
                        );
                        self.pending_proofs.remove(&game_address);
                        return Ok(());
                    }
                    Some(KnownRevert::InvalidParentGame) => {
                        warn!(
                            error = %e,
                            game = %game_address,
                            "dispute game parent is invalid onchain, ignoring game"
                        );
                        self.ignore_game(game_address);
                        return Ok(());
                    }
                    Some(KnownRevert::L1OriginTooOld) => {
                        warn!(
                            error = %e,
                            game = %game_address,
                            "dispute proof L1 origin is too old, ignoring game"
                        );
                        self.ignore_game(game_address);
                        return Ok(());
                    }
                    Some(KnownRevert::InvalidSigner) if !targets_tee => {
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
                        let has_zk_fallback = pending.kind.has_zk_fallback();

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

                if let Some(p) = self.pending_proofs.get_mut(&game_address) {
                    // Don't retry the failed TEE submission — switch to the ZK
                    // fallback so the next retry uses the pre-built ZK request.
                    p.phase = ProofPhase::NeedsRetry;
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

    /// Handles a proof that needs retrying after failure.
    ///
    /// If retries are exhausted the entry is dropped; otherwise `proveBlockRange`
    /// is called and the phase transitions back to `AwaitingProof`.
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

        // If this was a TEE proof, eagerly transition to ZK *before*
        // calling `proveBlockRange` so that subsequent retries take the ZK branch
        // and the fallback metric is emitted exactly once per transition.
        let request = match &pending.kind {
            ProofKind::Tee { zk_fallback_request, zk_fallback_intent } => {
                let (Some(fallback_request), Some(fallback_intent)) =
                    (zk_fallback_request.clone(), *zk_fallback_intent)
                else {
                    // No ZK fallback available — nothing more we can do.
                    debug!(
                        game = %game_address,
                        "TEE proof has no ZK fallback request, dropping entry"
                    );
                    self.pending_proofs.remove(&game_address);
                    return Ok(());
                };

                debug!(game = %game_address, "TEE proof needs retry, falling back to ZK");
                ChallengerMetrics::tee_proof_fallback_total().increment(1);

                // Transition eagerly so retries use the ZK path.
                if let Some(p) = self.pending_proofs.get_mut(&game_address) {
                    p.kind = ProofKind::Zk { prove_request: fallback_request.clone() };
                    p.intent = fallback_intent;
                    p.retry_count = 0;
                    p.tee_submit_retry_count = 0;
                }

                fallback_request
            }
            ProofKind::Zk { prove_request } => prove_request.clone(),
        };

        ChallengerMetrics::proof_retries_total().increment(1);

        let prove_request = crate::ChallengerProofAdapter::snark_plonk_prove_block_range_request(
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
                if let Some(p) = self.pending_proofs.get_mut(&game_address) {
                    p.phase = ProofPhase::AwaitingProof {
                        session_id: response.session_id,
                        started_at: Instant::now(),
                    };
                }
            }
            Err(e) => {
                if let Some(p) = self.pending_proofs.get_mut(&game_address) {
                    p.retry_count += 1;
                }
                warn!(
                    error = %e,
                    game = %game_address,
                    retry_count = retry_count,
                    "proveBlockRange failed on retry, will retry next tick"
                );
                // Leave as NeedsRetry for next tick.
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, time::Duration};

    use alloy_primitives::{Address, B256, Bytes};
    use base_proof_contracts::l1_origin_too_old_selector;
    use base_prover_service_protocol::{SnarkPlonkProofRequest, ZkProofRequest, ZkVm};
    use base_tx_manager::TxManagerError;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::test_utils::{
        MockAggregateVerifier, MockDisputeGameFactory, MockL2Provider, MockTxManager,
        MockZkProofProvider, addr, factory_game, mock_anchor_registry, mock_state,
        receipt_with_status,
    };

    fn proof_request() -> SnarkPlonkProofRequest {
        SnarkPlonkProofRequest {
            proof: ZkProofRequest {
                start_block_number: 100,
                number_of_blocks_to_prove: 10,
                sequence_window: None,
                l1_head: Some(B256::repeat_byte(0xAA)),
                intermediate_root_interval: Some(10),
                zk_vm: ZkVm::Sp1,
                zk_backend: ZkBackend::Cluster,
            },
            prover_address: Address::repeat_byte(0xCC),
        }
    }

    fn driver_with_tx_error(
        err: TxManagerError,
    ) -> (Driver<MockL2Provider, MockZkProofProvider, MockTxManager>, Arc<MockZkProofProvider>)
    {
        driver_with_tx_manager(MockTxManager::new(Err(err)))
    }

    fn driver_with_tx_manager(
        tx_manager: MockTxManager,
    ) -> (Driver<MockL2Provider, MockZkProofProvider, MockTxManager>, Arc<MockZkProofProvider>)
    {
        let game_address = addr(0);
        let mut verifier_games = HashMap::new();
        verifier_games.insert(game_address, mock_state(GameStatus::InProgress, Address::ZERO, 100));
        let verifier = Arc::new(MockAggregateVerifier::new(verifier_games));
        let factory = Arc::new(MockDisputeGameFactory::new(vec![]));
        let anchor_registry = mock_anchor_registry(Address::ZERO);
        let scanner = GameScanner::new(
            Arc::clone(&factory) as Arc<dyn base_proof_contracts::DisputeGameFactoryClient>,
            Arc::clone(&verifier) as Arc<dyn AggregateVerifierClient>,
            Arc::clone(&anchor_registry),
        );
        let proof_requester = Arc::new(MockZkProofProvider::default());
        let l2_provider = Arc::new(MockL2Provider::new());
        let components = DriverComponents {
            scanner,
            validator: OutputValidator::new(Arc::clone(&l2_provider)),
            proof_requester: Arc::clone(&proof_requester),
            submitter: ChallengeSubmitter::new(tx_manager),
            tee: None,
            verifier_client: verifier,
            bond_manager: None,
            anchor_updater: AnchorUpdater::new(
                factory,
                anchor_registry,
                l2_provider,
                Address::repeat_byte(0xAA),
                1,
                100,
                100,
            ),
        };
        let driver = Driver::new(
            DriverConfig {
                poll_interval: Duration::from_millis(10),
                max_proof_duration: Duration::from_secs(60),
                tee_submit_retry_limit: 3,
                cancel: CancellationToken::new(),
            },
            components,
        );

        (driver, proof_requester)
    }

    fn insert_ready_proof(driver: &mut Driver<MockL2Provider, MockZkProofProvider, MockTxManager>) {
        let game_address = addr(0);
        driver.pending_proofs.insert(
            game_address,
            PendingProof::ready(
                Bytes::from(vec![0x01, 0xAA]),
                0,
                B256::repeat_byte(0x22),
                proof_request(),
                DisputeIntent::Challenge,
            ),
        );
    }

    fn insert_ready_tee_proof(
        driver: &mut Driver<MockL2Provider, MockZkProofProvider, MockTxManager>,
    ) {
        let game_address = addr(0);
        driver.pending_proofs.insert(
            game_address,
            PendingProof {
                phase: ProofPhase::ReadyToSubmit { proof_bytes: Bytes::from(vec![0x00, 0xAA]) },
                kind: ProofKind::Tee {
                    zk_fallback_request: Some(proof_request()),
                    zk_fallback_intent: Some(DisputeIntent::Nullify),
                },
                invalid_index: 0,
                expected_root: B256::repeat_byte(0x22),
                retry_count: 0,
                tee_submit_retry_count: 0,
                intent: DisputeIntent::Nullify,
            },
        );
    }

    fn insert_ready_tee_proof_without_fallback(
        driver: &mut Driver<MockL2Provider, MockZkProofProvider, MockTxManager>,
    ) {
        let game_address = addr(0);
        driver.pending_proofs.insert(
            game_address,
            PendingProof {
                phase: ProofPhase::ReadyToSubmit { proof_bytes: Bytes::from(vec![0x00, 0xAA]) },
                kind: ProofKind::Tee { zk_fallback_request: None, zk_fallback_intent: None },
                invalid_index: 0,
                expected_root: B256::repeat_byte(0x22),
                retry_count: 0,
                tee_submit_retry_count: 0,
                intent: DisputeIntent::Nullify,
            },
        );
    }

    fn candidate() -> CandidateGame {
        let state = mock_state(GameStatus::InProgress, Address::ZERO, 100);
        CandidateGame {
            index: 0,
            factory: factory_game(0, 1),
            info: state.game_info,
            starting_block_number: state.starting_block_number,
            intermediate_block_interval: 10,
            l1_head: state.l1_head,
            tee_prover: state.tee_prover,
            category: GameCategory::InvalidZkProposal,
        }
    }

    fn assert_ready_tee_proof(driver: &Driver<MockL2Provider, MockZkProofProvider, MockTxManager>) {
        let pending = driver.pending_proofs.get(&addr(0)).expect("pending proof should remain");
        assert!(matches!(pending.phase, ProofPhase::ReadyToSubmit { .. }));
        assert!(pending.kind.is_tee());
    }

    fn assert_zk_fallback_requested(
        driver: &Driver<MockL2Provider, MockZkProofProvider, MockTxManager>,
        proof_requester: &MockZkProofProvider,
    ) {
        let pending = driver.pending_proofs.get(&addr(0)).expect("pending proof should remain");
        assert!(matches!(pending.phase, ProofPhase::AwaitingProof { .. }));
        assert!(!pending.kind.is_tee());
        let state = proof_requester.state.lock().unwrap();
        assert_eq!(state.prove_block_range_log.len(), 1);
    }

    #[tokio::test]
    async fn proof_already_verified_revert_drops_pending_proof() {
        let (mut driver, _proof_requester) =
            driver_with_tx_error(TxManagerError::ExecutionReverted {
                reason: Some("AlreadyProven(1)".to_string()),
                data: None,
            });
        insert_ready_proof(&mut driver);

        driver.poll_or_submit(addr(0)).await.unwrap();

        assert!(!driver.pending_proofs.contains_key(&addr(0)));
    }

    #[tokio::test]
    async fn game_already_exists_revert_drops_pending_proof() {
        let (mut driver, _proof_requester) =
            driver_with_tx_error(TxManagerError::ExecutionReverted {
                reason: Some("GameAlreadyExists(0x00)".to_string()),
                data: None,
            });
        insert_ready_proof(&mut driver);

        driver.poll_or_submit(addr(0)).await.unwrap();

        assert!(!driver.pending_proofs.contains_key(&addr(0)));
    }

    #[tokio::test]
    async fn challenge_success_does_not_track_anchor_update() {
        let tx_hash = B256::repeat_byte(0x44);
        let (mut driver, _proof_requester) =
            driver_with_tx_manager(MockTxManager::new(Ok(receipt_with_status(true, tx_hash))));
        insert_ready_proof(&mut driver);

        driver.poll_or_submit(addr(0)).await.unwrap();

        assert!(!driver.pending_proofs.contains_key(&addr(0)));
    }

    #[tokio::test]
    async fn stale_l1_origin_revert_drops_pending_zk_proof() {
        let (mut driver, proof_requester) =
            driver_with_tx_error(TxManagerError::ExecutionReverted {
                reason: None,
                data: Some(Bytes::from(l1_origin_too_old_selector().to_vec())),
            });
        insert_ready_proof(&mut driver);

        driver.poll_or_submit(addr(0)).await.unwrap();

        assert!(!driver.pending_proofs.contains_key(&addr(0)));
        assert!(driver.ignored_games.contains(&addr(0)));
        let state = proof_requester.state.lock().unwrap();
        assert!(state.prove_block_range_log.is_empty());
    }

    #[tokio::test]
    async fn stale_l1_origin_revert_drops_pending_tee_proof_without_requesting_zk() {
        let (mut driver, proof_requester) =
            driver_with_tx_error(TxManagerError::ExecutionReverted {
                reason: None,
                data: Some(Bytes::from(l1_origin_too_old_selector().to_vec())),
            });
        insert_ready_tee_proof(&mut driver);

        driver.poll_or_submit(addr(0)).await.unwrap();

        assert!(!driver.pending_proofs.contains_key(&addr(0)));
        assert!(driver.ignored_games.contains(&addr(0)));
        let state = proof_requester.state.lock().unwrap();
        assert!(state.prove_block_range_log.is_empty());
    }

    #[test]
    fn ignored_games_are_bounded() {
        let (mut driver, _proof_requester) =
            driver_with_tx_manager(MockTxManager::new(Ok(receipt_with_status(true, B256::ZERO))));
        let max = Driver::<MockL2Provider, MockZkProofProvider, MockTxManager>::MAX_IGNORED_GAMES;

        for i in 0..=max {
            driver.ignore_game(addr(i as u64));
        }

        assert_eq!(driver.ignored_games.len(), max);
        assert!(!driver.ignored_games.contains(&addr(0)));
        assert!(driver.ignored_games.contains(&addr(max as u64)));
    }

    #[tokio::test]
    async fn tee_submit_nonce_too_low_keeps_ready_proof_without_requesting_zk() {
        let (mut driver, proof_requester) = driver_with_tx_error(TxManagerError::NonceTooLow);
        insert_ready_tee_proof(&mut driver);

        driver.poll_or_submit(addr(0)).await.unwrap();

        assert_ready_tee_proof(&driver);
        let pending = driver.pending_proofs.get(&addr(0)).expect("pending proof should remain");
        assert_eq!(pending.tee_submit_retry_count, 1);
        let state = proof_requester.state.lock().unwrap();
        assert!(state.prove_block_range_log.is_empty());
    }

    #[tokio::test]
    async fn tee_submit_retry_limit_falls_back_to_zk() {
        let (mut driver, proof_requester) =
            driver_with_tx_manager(MockTxManager::with_responses(vec![
                Err(TxManagerError::NonceTooLow),
                Err(TxManagerError::NonceTooLow),
            ]));
        driver.tee_submit_retry_limit = 1;
        insert_ready_tee_proof(&mut driver);

        driver.poll_or_submit(addr(0)).await.unwrap();

        assert_ready_tee_proof(&driver);
        {
            let state = proof_requester.state.lock().unwrap();
            assert!(state.prove_block_range_log.is_empty());
        }

        driver.poll_or_submit(addr(0)).await.unwrap();

        assert_zk_fallback_requested(&driver, &proof_requester);
    }

    #[tokio::test]
    async fn tee_submit_retry_limit_drops_proof_without_zk_fallback() {
        let (mut driver, proof_requester) =
            driver_with_tx_manager(MockTxManager::with_responses(vec![
                Err(TxManagerError::NonceTooLow),
                Err(TxManagerError::NonceTooLow),
            ]));
        driver.tee_submit_retry_limit = 1;
        insert_ready_tee_proof_without_fallback(&mut driver);

        driver.poll_or_submit(addr(0)).await.unwrap();

        assert_ready_tee_proof(&driver);

        driver.poll_or_submit(addr(0)).await.unwrap();

        assert!(!driver.pending_proofs.contains_key(&addr(0)));
        let state = proof_requester.state.lock().unwrap();
        assert!(state.prove_block_range_log.is_empty());
    }

    #[tokio::test]
    async fn tee_invalid_signer_revert_falls_back_to_zk() {
        let (mut driver, proof_requester) =
            driver_with_tx_error(TxManagerError::ExecutionReverted {
                reason: Some("InvalidSigner()".to_string()),
                data: None,
            });
        insert_ready_tee_proof(&mut driver);

        driver.poll_or_submit(addr(0)).await.unwrap();

        assert_zk_fallback_requested(&driver, &proof_requester);
    }

    #[tokio::test]
    async fn tee_mined_tx_revert_falls_back_to_zk() {
        let tx_hash = B256::repeat_byte(0x44);
        let (mut driver, proof_requester) =
            driver_with_tx_manager(MockTxManager::new(Ok(receipt_with_status(false, tx_hash))));
        insert_ready_tee_proof(&mut driver);

        driver.poll_or_submit(addr(0)).await.unwrap();

        assert_zk_fallback_requested(&driver, &proof_requester);
    }

    #[tokio::test]
    async fn ignored_game_is_not_reprocessed_from_scan() {
        let (mut driver, proof_requester) =
            driver_with_tx_error(TxManagerError::ExecutionReverted {
                reason: None,
                data: Some(Bytes::from(l1_origin_too_old_selector().to_vec())),
            });
        insert_ready_proof(&mut driver);

        driver.poll_or_submit(addr(0)).await.unwrap();
        driver.process_candidate(candidate()).await.unwrap();

        let state = proof_requester.state.lock().unwrap();
        assert!(state.prove_block_range_log.is_empty());
    }
}
