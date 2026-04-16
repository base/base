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
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_primitives::{Address, B256, Bytes};
use base_proof_contracts::AggregateVerifierClient;
use base_proof_primitives::{
    ProofEncoder, ProofRequest as TeeProofRequest, ProofResult, ProverClient,
};
use base_proof_rpc::L2Provider;
use base_runtime::{Clock, TokioRuntime};
use base_tx_manager::TxManager;
use base_zk_client::{ProofType, ProveBlockRequest, ZkProofProvider};
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
    /// Cancellation token for graceful shutdown.
    pub cancel: CancellationToken,
    /// Shared flag flipped to `true` after the first successful driver step.
    pub ready: Arc<AtomicBool>,
}

/// TEE proof configuration, bundling the provider and L1 head provider.
#[derive(Debug)]
pub struct TeeConfig {
    /// TEE proof provider.
    pub provider: Arc<dyn ProverClient>,
    /// L1 head provider for fetching the finalized head hash.
    pub l1_head_provider: Arc<dyn L1HeadProvider>,
    /// Timeout for individual TEE proof requests.
    pub request_timeout: Duration,
}

/// Service-layer dependencies injected into the [`Driver`].
pub struct DriverComponents<
    L2: L2Provider,
    P: ZkProofProvider,
    T: TxManager,
    C: Clock = TokioRuntime,
> {
    /// Scans for new dispute games on L1.
    pub scanner: GameScanner,
    /// Validates L2 output roots against the local node.
    pub validator: OutputValidator<L2>,
    /// ZK proof provider used to generate fault proofs.
    pub zk_prover: Arc<P>,
    /// Submits challenge transactions to L1.
    pub submitter: ChallengeSubmitter<T>,
    /// Optional TEE proof configuration (provider + L1 RPC client).
    pub tee: Option<TeeConfig>,
    /// Client for the aggregate verifier contract.
    pub verifier_client: Arc<dyn AggregateVerifierClient>,
    /// Bond lifecycle manager (optional; enabled when claim addresses are configured).
    pub bond_manager: Option<BondManager<C>>,
}

impl<L2: L2Provider, P: ZkProofProvider, T: TxManager, C: Clock> std::fmt::Debug
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
    P: ZkProofProvider,
    T: TxManager,
{
    /// Scans for new dispute games on L1.
    pub scanner: GameScanner,
    /// Validates L2 output roots against the local node.
    pub validator: OutputValidator<L2>,
    /// ZK proof provider used to generate fault proofs.
    pub zk_prover: Arc<P>,
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
    /// Token used to signal graceful shutdown.
    pub cancel: CancellationToken,
    /// Indicates whether the driver has completed its first scan.
    pub ready: Arc<AtomicBool>,
}

impl<L2: L2Provider, P: ZkProofProvider, T: TxManager, C: Clock> std::fmt::Debug
    for Driver<L2, P, T, C>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Driver")
            .field("pending_proofs", &self.pending_proofs.len())
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}

impl<L2: L2Provider, P: ZkProofProvider, T: TxManager, C: Clock> Driver<L2, P, T, C> {
    /// Maximum number of times a failed proof job will be retried before being dropped.
    pub const MAX_PROOF_RETRIES: u32 = 3;

    /// Creates a new driver with the given components.
    pub fn new(config: DriverConfig, components: DriverComponents<L2, P, T, C>) -> Self {
        Self {
            scanner: components.scanner,
            validator: components.validator,
            zk_prover: components.zk_prover,
            submitter: components.submitter,
            tee: components.tee,
            verifier_client: components.verifier_client,
            pending_proofs: PendingProofs::new(),
            bond_manager: components.bond_manager,
            poll_interval: config.poll_interval,
            cancel: config.cancel,
            ready: config.ready,
        }
    }

    /// Runs the main driver loop until the cancellation token is fired.
    pub async fn run(mut self) {
        info!("challenger driver starting");
        let mut signalled_ready = false;
        loop {
            if self.cancel.is_cancelled() {
                info!("challenger driver shutting down");
                break;
            }

            match self.step().await {
                Ok(()) => {
                    if !signalled_ready {
                        signalled_ready = true;
                        self.ready.store(true, Ordering::SeqCst);
                        info!("service is ready");
                    }
                }
                Err(e) => {
                    warn!(error = %e, "driver step failed");
                }
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
                Ok(None)
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
            let tee_fut = self.attempt_tee_proof(&candidate, invalid_index, expected_root, tee);
            match tokio::time::timeout(tee.request_timeout, tee_fut).await {
                Err(_elapsed) => {
                    warn!(
                        game = %game_address,
                        timeout = ?tee.request_timeout,
                        "TEE proof request timed out, falling back to ZK"
                    );
                }
                Ok(Ok(proof_bytes)) => {
                    info!(game = %game_address, path = "tee", "TEE proof obtained");
                    self.pending_proofs.insert(
                        game_address,
                        PendingProof::ready_tee(proof_bytes, invalid_index, expected_root),
                    );
                    if let Err(e) = self.poll_or_submit(game_address).await {
                        warn!(error = %e, game = %game_address, "initial TEE submission failed, will retry next tick");
                    }
                    ChallengerMetrics::tee_proof_obtained_total().increment(1);
                    return Ok(());
                }
                Ok(Err(e)) => {
                    warn!(
                        error = %e,
                        game = %game_address,
                        "TEE proof failed, falling back to ZK"
                    );
                }
            }
            ChallengerMetrics::tee_proof_fallback_total().increment(1);
        }

        // ZK fallback (or direct ZK if no TEE prover / intent is Nullify).
        self.initiate_zk_proof(candidate, invalid_index, expected_root, intent).await
    }

    /// Attempts to obtain a TEE proof for the given candidate game.
    async fn attempt_tee_proof(
        &self,
        candidate: &CandidateGame,
        invalid_index: u64,
        expected_root: B256,
        tee: &TeeConfig,
    ) -> eyre::Result<Bytes> {
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

        let request = TeeProofRequest {
            l1_head,
            agreed_l2_head_hash,
            agreed_l2_output_root,
            claimed_l2_output_root: expected_root,
            claimed_l2_block_number,
            proposer: self.submitter.sender_address(),
            intermediate_block_interval: candidate.intermediate_block_interval,
            l1_head_number,
            ..Default::default()
        };

        let result = tee.provider.prove(request).await.map_err(|e| eyre::eyre!(e))?;

        // Validate that the TEE computed the expected output root and encode
        // the proof in compact format (type + signature only, no L1 origin
        // data — the contract already has it in CWIA).
        match &result {
            ProofResult::Tee { aggregate_proposal, .. } => {
                if aggregate_proposal.output_root != expected_root {
                    return Err(eyre::eyre!(
                        "TEE computed unexpected output root: expected {expected_root}, got {}",
                        aggregate_proposal.output_root
                    ));
                }
                ProofEncoder::encode_dispute_proof_bytes(&aggregate_proposal.signature)
                    .map_err(|e| eyre::eyre!("TEE proof encoding failed: {e}"))
            }
            ProofResult::Zk { .. } => Err(eyre::eyre!("TEE provider returned ZK result")),
        }
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
        let start_block_number = candidate.checkpoint_start_block(invalid_index)?;

        let session_id = PendingProof::derive_session_id(game_address, invalid_index);
        let prover_address = format!("{:#x}", self.submitter.sender_address());
        let request = ProveBlockRequest {
            start_block_number,
            number_of_blocks_to_prove: candidate.intermediate_block_interval,
            sequence_window: None,
            proof_type: ProofType::GenericZkvmClusterSnarkGroth16.into(),
            session_id: Some(session_id),
            prover_address: Some(prover_address),
            l1_head: Some(format!("{:#x}", candidate.l1_head)),
        };

        let prove_response = self.zk_prover.prove_block(request.clone()).await?;

        info!(
            game = %game_address,
            session_id = %prove_response.session_id,
            "proof job initiated"
        );

        let pending = PendingProof::awaiting(
            prove_response.session_id,
            invalid_index,
            expected_root,
            request,
            intent,
        );
        self.pending_proofs.insert(game_address, pending);

        Ok(())
    }

    /// Advances a pending proof through its lifecycle.
    ///
    /// - **`AwaitingProof`** — polls the ZK service:
    ///   - `Succeeded` → transitions to `ReadyToSubmit` and falls through to
    ///     submission.
    ///   - `Failed` → transitions to `NeedsRetry` so `prove_block` is
    ///     re-initiated.
    ///   - Intermediate (`Created`/`Pending`/`Running`) → returns early
    ///     without any contract calls.
    /// - **`ReadyToSubmit`** — submits the dispute tx based on the entry's
    ///   [`DisputeIntent`]:
    ///   - [`DisputeIntent::Nullify`] → calls `nullify()`.
    ///   - [`DisputeIntent::Challenge`] → calls `challenge()`.
    ///   - On success → removes the entry.
    ///   - On failure → leaves the entry so it is retried next tick.
    /// - **`NeedsRetry`** — re-initiates `prove_block`:
    ///   - If `retry_count > MAX_PROOF_RETRIES` → drops the entry.
    ///   - Otherwise → calls `prove_block` and transitions to `AwaitingProof`.
    async fn poll_or_submit(&mut self, game_address: Address) -> eyre::Result<()> {
        let (invalid_index, expected_root, intent, targets_tee) =
            match self.pending_proofs.get(&game_address) {
                Some(p) => (p.invalid_index, p.expected_root, p.intent, p.kind.is_tee()),
                None => return Ok(()),
            };

        // Poll proof status first — if still pending, skip the contract
        // calls that check game liveness. This avoids 3 RPC round-trips
        // per tick for proofs that are not yet ready.
        let proof_update = self.pending_proofs.poll(game_address, &*self.zk_prover).await?;
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

        if status != GameScanner::STATUS_IN_PROGRESS {
            debug!(game = %game_address, status = status, "game no longer in progress, dropping pending proof");
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
    /// If retries are exhausted the entry is dropped; otherwise `prove_block`
    /// is called and the phase transitions back to `AwaitingProof`.
    async fn handle_proof_retry(&mut self, game_address: Address) -> eyre::Result<()> {
        let pending = match self.pending_proofs.get(&game_address) {
            Some(p) => p,
            None => return Ok(()),
        };

        let retry_count = pending.retry_count;

        if retry_count > Self::MAX_PROOF_RETRIES {
            warn!(
                game = %game_address,
                retry_count = retry_count,
                "proof retries exhausted, dropping entry"
            );
            self.pending_proofs.remove(&game_address);
            return Ok(());
        }

        let request = match &pending.kind {
            ProofKind::Tee => {
                // TEE proofs have no ZK session to re-initiate — drop the entry.
                debug!(game = %game_address, "TEE proof has no ZK request, dropping entry");
                self.pending_proofs.remove(&game_address);
                return Ok(());
            }
            ProofKind::Zk { prove_request } => prove_request.clone(),
        };

        ChallengerMetrics::proof_retries_total().increment(1);

        match self.zk_prover.prove_block(request).await {
            Ok(response) => {
                info!(
                    game = %game_address,
                    session_id = %response.session_id,
                    retry_count = retry_count,
                    "proof job re-initiated"
                );
                if let Some(p) = self.pending_proofs.get_mut(&game_address) {
                    p.phase = ProofPhase::AwaitingProof { session_id: response.session_id };
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
                    "prove_block failed on retry, will retry next tick"
                );
                // Leave as NeedsRetry for next tick.
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;

    use crate::PendingProof;

    #[test]
    fn session_id_is_deterministic() {
        let addr = Address::repeat_byte(0xAA);
        assert_eq!(
            PendingProof::derive_session_id(addr, 42),
            PendingProof::derive_session_id(addr, 42),
        );
    }

    #[test]
    fn session_id_differs_for_different_inputs() {
        let addr = Address::repeat_byte(0xAA);
        assert_ne!(
            PendingProof::derive_session_id(addr, 1),
            PendingProof::derive_session_id(addr, 2),
        );
        assert_ne!(
            PendingProof::derive_session_id(Address::repeat_byte(0xBB), 1),
            PendingProof::derive_session_id(Address::repeat_byte(0xCC), 1),
        );
    }
}
