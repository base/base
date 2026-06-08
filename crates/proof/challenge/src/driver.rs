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
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256};
use base_proof_contracts::{AggregateVerifierClient, GameStatus};
use base_proof_primitives::ProofRequest as TeeProofRequest;
use base_proof_rpc::L2Provider;
use base_prover_service_client::ProofRequesterProvider;
use base_prover_service_protocol::{SnarkGroth16ProofRequest, TeeKind, ZkProofRequest, ZkVm};
use base_runtime::{Clock, TokioRuntime};
use base_tx_manager::TxManager;
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    BondManager, CandidateGame, ChallengeSubmitter, ChallengerMetrics, DisputeIntent, GameCategory,
    GameScanner, IntermediateValidationParams, L1HeadProvider, OutputValidator, PendingProof,
    PendingProofs, ProofKind, ProofPhase, ProofUpdate, ValidatorError,
};

/// Configuration for the challenger [`Driver`].
#[derive(Debug)]
pub struct DriverConfig {
    /// How often the driver polls for new games.
    pub poll_interval: Duration,
    /// Maximum wall-clock time to wait for a ZK proof session before treating it as failed.
    pub max_proof_duration: Duration,
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
}

impl<L2: L2Provider, P: ProofRequesterProvider, T: TxManager, C: Clock> std::fmt::Debug
    for DriverComponents<L2, P, T, C>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverComponents")
            .field("scanner", &self.scanner)
            .field("tee", &self.tee.as_ref().map(|_| ".."))
            .field("bond_manager", &self.bond_manager)
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
    /// Bond lifecycle manager (optional; enabled when claim addresses are configured).
    pub bond_manager: Option<BondManager<C>>,
    /// Interval between polling cycles.
    pub poll_interval: Duration,
    /// Maximum wall-clock time to wait for a ZK proof session before treating it as failed.
    pub max_proof_duration: Duration,
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
            .finish_non_exhaustive()
    }
}

impl<L2: L2Provider, P: ProofRequesterProvider, T: TxManager, C: Clock> Driver<L2, P, T, C> {
    /// Maximum number of times a failed proof job will be retried before being dropped.
    pub const MAX_PROOF_RETRIES: u32 = 3;

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
            bond_manager: components.bond_manager,
            poll_interval: config.poll_interval,
            max_proof_duration: config.max_proof_duration,
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
    /// scan batch, then discovers claimable bonds, advances bond lifecycle
    /// claims, and finally scans for new candidates and processes them.
    pub async fn step(&mut self) -> eyre::Result<()> {
        self.poll_pending_proofs().await;
        self.discover_claimable_bonds().await;
        self.poll_bond_claims().await;

        let candidates = self.scanner.scan().await?;

        for candidate in candidates {
            let index = candidate.index;
            if let Err(e) = self.process_candidate(candidate).await {
                warn!(error = %e, game_index = index, "failed to process candidate");
            }
        }

        Ok(())
    }

    /// Discovers new claimable games via incremental and periodic full
    /// rescanning. Runs before [`poll_bond_claims`](Self::poll_bond_claims)
    /// so that newly discovered games are immediately eligible for
    /// advancement in the same tick.
    async fn discover_claimable_bonds(&mut self) {
        if let Some(ref mut bond_manager) = self.bond_manager
            && let Err(e) = bond_manager.discover_claimable_games(&*self.verifier_client).await
        {
            warn!(error = %e, "bond discovery scan failed");
        }
    }

    /// Polls the bond manager to advance tracked games through the bond
    /// lifecycle (resolve → unlock → delay → withdraw).
    async fn poll_bond_claims(&mut self) {
        if let Some(ref mut bond_manager) = self.bond_manager {
            bond_manager.poll(&*self.verifier_client, &self.submitter).await;
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
                    // mismatch between the cached interval and on-chain
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

        let expected_root = match self.validator.compute_output_root(checkpoint_block).await {
            Ok(root) => root,
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
        if on_chain_root != expected_root {
            debug!(
                game = %game_address,
                challenged_index = challenged_index,
                on_chain = %on_chain_root,
                expected = %expected_root,
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

        self.initiate_zk_proof(candidate, challenged_index, on_chain_root, DisputeIntent::Nullify)
            .await
    }

    /// Attempts TEE-first proof sourcing with ZK fallback.
    ///
    /// The `intent` determines the on-chain action for the ZK fallback path.
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
        // journal whose `l1OriginHash` matches what the on-chain `nullify()`
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

    /// Builds a [`SnarkGroth16ProofRequest`] for the given candidate and invalid index.
    fn build_zk_request(
        &self,
        candidate: &CandidateGame,
        invalid_index: u64,
    ) -> eyre::Result<SnarkGroth16ProofRequest> {
        let start_block_number = candidate.checkpoint_start_block(invalid_index)?;

        Ok(SnarkGroth16ProofRequest {
            proof: ZkProofRequest {
                start_block_number,
                number_of_blocks_to_prove: candidate.intermediate_block_interval,
                sequence_window: None,
                l1_head: Some(candidate.l1_head),
                intermediate_root_interval: Some(candidate.intermediate_block_interval),
                zk_vm: ZkVm::Sp1,
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
        let request = crate::ChallengerProofAdapter::snark_groth16_prove_block_range_request(
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

                // After a successful challenge(), register the game for bond
                // tracking so the BondManager can resolve and claim the bond.
                if intent == DisputeIntent::Challenge
                    && let Some(ref mut bond_manager) = self.bond_manager
                {
                    let sender = self.submitter.sender_address();
                    if !bond_manager.track_game(game_address, sender) {
                        warn!(
                            game = %game_address,
                            sender = %sender,
                            "bond will not be tracked — sender address is not \
                             in --bond-claim-addresses; bond may go unclaimed"
                        );
                    }
                }
            }
            Err(e) => {
                if targets_tee && let Some(p) = self.pending_proofs.get_mut(&game_address) {
                    warn!(
                        error = %e,
                        game = %game_address,
                        "TEE dispute tx failed, falling back to ZK"
                    );
                    // Don't retry the failed TEE submission — switch to the ZK
                    // fallback so the next retry uses the pre-built ZK request.
                    p.phase = ProofPhase::NeedsRetry;
                    return self.handle_proof_retry(game_address).await;
                }
                warn!(
                    error = %e,
                    game = %game_address,
                    "dispute tx failed, will retry next tick"
                );
                // Leave entry as ReadyToSubmit for retry.
            }
        }

        Ok(())
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
                }

                fallback_request
            }
            ProofKind::Zk { prove_request } => prove_request.clone(),
        };

        ChallengerMetrics::proof_retries_total().increment(1);

        let prove_request = crate::ChallengerProofAdapter::snark_groth16_prove_block_range_request(
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
