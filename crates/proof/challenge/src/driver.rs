//! Main driver loop for the challenger service.
//!
//! The [`Driver`] schedules game scanning, output validation, proof lifecycle
//! work, bond discovery, and anchor maintenance.

use std::{sync::Arc, time::Duration};

use base_proof_contracts::AggregateVerifierClient;
use base_proof_rpc::{L1Provider, L2Provider};
use base_prover_service_client::ProofRequesterProvider;
use base_runtime::TokioRuntime;
use base_tx_manager::TxManager;
use tokio::select;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    AnchorUpdater, BondManager, CandidateGame, ChallengeSubmitter, ChallengerMetrics,
    DisputeIntent, DisputeProofManager, GameCategory, GameScanner, IntermediateValidationParams,
    OutputValidator, ValidatorError,
};

/// Dependencies and runtime settings injected into the [`Driver`].
pub struct DriverComponents<L2: L2Provider, P: ProofRequesterProvider, T: TxManager> {
    /// Scans for new dispute games on L1.
    pub scanner: GameScanner,
    /// Validates L2 output roots against the local node.
    pub validator: OutputValidator<L2>,
    /// Prover-service requester used to generate and poll ZK fault proofs.
    pub proof_requester: Arc<P>,
    /// Submits challenge transactions to L1.
    pub submitter: ChallengeSubmitter<T>,
    /// L1 provider used to construct TEE proof requests.
    pub l1_provider: Arc<dyn L1Provider>,
    /// Client for the aggregate verifier contract.
    pub verifier_client: Arc<dyn AggregateVerifierClient>,
    /// Bond lifecycle manager (optional; enabled when claim addresses are configured).
    pub bond_manager: Option<BondManager<TokioRuntime>>,
    /// Best-effort anchor state updater.
    pub anchor_updater: AnchorUpdater,
    /// How often the driver polls for new games.
    pub poll_interval: Duration,
    /// Maximum wall-clock time to wait for a ZK proof session before treating it as failed.
    pub max_proof_duration: Duration,
    /// Retryable TEE submission failures to tolerate before falling back to ZK.
    pub tee_submit_retry_limit: u32,
    /// Cancellation token for graceful shutdown.
    pub cancel: CancellationToken,
}

impl<L2: L2Provider, P: ProofRequesterProvider, T: TxManager> std::fmt::Debug
    for DriverComponents<L2, P, T>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverComponents").finish_non_exhaustive()
    }
}

/// Orchestrates challenger scan, validation, proof, bond, and anchor work.
pub struct Driver<L2, P, T>
where
    L2: L2Provider,
    P: ProofRequesterProvider,
    T: TxManager,
{
    /// Scans for new dispute games on L1.
    pub scanner: GameScanner,
    /// Submits challenge transactions to L1 and bond/anchor maintenance transactions.
    pub submitter: ChallengeSubmitter<T>,
    /// Client for the aggregate verifier contract.
    pub verifier_client: Arc<dyn AggregateVerifierClient>,
    /// Validates L2 output roots against the local node.
    pub validator: OutputValidator<L2>,
    /// Manages proof sessions, retries, submissions, and TEE-to-ZK fallback.
    pub proof_manager: DisputeProofManager<L2, P>,
    /// Bond lifecycle manager (optional; enabled when claim addresses are configured).
    pub bond_manager: Option<BondManager<TokioRuntime>>,
    /// Best-effort anchor state updater.
    pub anchor_updater: AnchorUpdater,
    /// Interval between polling cycles.
    pub poll_interval: Duration,
    /// Token used to signal graceful shutdown.
    pub cancel: CancellationToken,
}

impl<L2: L2Provider, P: ProofRequesterProvider, T: TxManager> std::fmt::Debug for Driver<L2, P, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Driver").finish_non_exhaustive()
    }
}

impl<L2: L2Provider, P: ProofRequesterProvider, T: TxManager> Driver<L2, P, T> {
    /// Creates a new driver with the given components.
    pub fn new(components: DriverComponents<L2, P, T>) -> Self {
        let validator = components.validator;
        Self {
            scanner: components.scanner,
            submitter: components.submitter,
            proof_manager: DisputeProofManager::new(
                validator.clone(),
                components.proof_requester,
                components.l1_provider,
                Arc::clone(&components.verifier_client),
                components.max_proof_duration,
                components.tee_submit_retry_limit,
            ),
            verifier_client: components.verifier_client,
            validator,
            bond_manager: components.bond_manager,
            anchor_updater: components.anchor_updater,
            poll_interval: components.poll_interval,
            cancel: components.cancel,
        }
    }

    /// Runs the main driver loop until the cancellation token is fired.
    pub async fn run(mut self) {
        info!("challenger driver starting");
        while !self.cancel.is_cancelled() {
            if let Err(e) = self.step().await {
                warn!(error = %e, "driver step failed");
            }

            ChallengerMetrics::pending_proofs().set(self.proof_manager.pending_proofs_len() as f64);
            ChallengerMetrics::ignored_games().set(self.proof_manager.ignored_games_len() as f64);

            select! {
                () = self.cancel.cancelled() => break,
                () = tokio::time::sleep(self.poll_interval) => {}
            }
        }
        info!("challenger driver shutting down");
    }

    /// Executes a single scan-validate-prove-submit cycle.
    pub async fn step(&mut self) -> eyre::Result<()> {
        self.proof_manager.poll_pending_proofs(&self.submitter).await;
        if let Some(bond_manager) = &mut self.bond_manager
            && let Err(e) =
                bond_manager.discover_claimable_games(&*self.verifier_client, &self.submitter).await
        {
            warn!(error = %e, "bond discovery scan failed");
        }
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

    /// Processes a candidate game according to its [`GameCategory`].
    async fn process_candidate(&mut self, candidate: CandidateGame) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;

        if self.proof_manager.is_ignored(game_address) {
            debug!(game = %game_address, "skipping ignored game");
            return Ok(());
        }

        if self.proof_manager.has_pending_proof(game_address) {
            debug!(game = %game_address, "skipping game with pending proof session");
            return Ok(());
        }

        match candidate.category {
            GameCategory::InvalidTeeProposal => {
                self.process_invalid_proposal(candidate, DisputeIntent::Challenge, true).await
            }
            GameCategory::FraudulentZkChallenge { challenged_index } => {
                self.process_fraudulent_zk_challenge(candidate, challenged_index).await
            }
            GameCategory::InvalidZkProposal => {
                self.process_invalid_proposal(candidate, DisputeIntent::Nullify, false).await
            }
            GameCategory::InvalidDualProposal => {
                self.process_invalid_proposal(candidate, DisputeIntent::Nullify, true).await
            }
        }
    }

    /// Fetches intermediate roots and validates them against the local L2 node.
    ///
    /// Returns `Ok(Some(result))` when validation completes, or `Ok(None)`
    /// when a transient error means the game should be retried next tick.
    async fn validate_game(
        &self,
        candidate: &CandidateGame,
    ) -> eyre::Result<Option<crate::ValidationResult>> {
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
            Ok(result) => Ok(Some(result)),
            Err(e) => match &e {
                ValidatorError::BlockNotAvailable { .. } => {
                    debug!(
                        game = %game_address,
                        error = %e,
                        "block not yet available, skipping game"
                    );
                    Ok(None)
                }
                ValidatorError::CheckpointCountMismatch { .. }
                | ValidatorError::InvalidInterval
                | ValidatorError::InvalidBlockRange { .. } => Err(e.into()),
                _ => {
                    warn!(
                        game = %game_address,
                        error = %e,
                        "transient validation error, skipping game"
                    );
                    Ok(None)
                }
            },
        }
    }

    /// Validates an invalid proposal and starts its proof lifecycle.
    #[tracing::instrument(
        name = "challenger.process_invalid_proposal",
        skip_all,
        fields(game = %candidate.factory.proxy, intent = ?intent)
    )]
    async fn process_invalid_proposal(
        &mut self,
        candidate: CandidateGame,
        intent: DisputeIntent,
        try_tee_first: bool,
    ) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;

        let result = match self.validate_game(&candidate).await? {
            Some(result) => result,
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

        let metric = match &candidate.category {
            GameCategory::InvalidTeeProposal => {
                ChallengerMetrics::invalid_tee_proposal_detected_total()
            }
            GameCategory::InvalidZkProposal => {
                ChallengerMetrics::invalid_zk_proposal_detected_total()
            }
            GameCategory::InvalidDualProposal => {
                ChallengerMetrics::invalid_dual_proposal_detected_total()
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
        metric.increment(1);

        self.proof_manager
            .initiate_proof(
                self.submitter.sender_address(),
                candidate,
                invalid_index,
                expected_root,
                intent,
                try_tee_first,
            )
            .await
    }

    /// Validates a challenged TEE root and nullifies a fraudulent ZK challenge.
    #[tracing::instrument(
        name = "challenger.process_fraudulent_zk_challenge",
        skip_all,
        fields(game = %candidate.factory.proxy, challenged_index = challenged_index)
    )]
    async fn process_fraudulent_zk_challenge(
        &mut self,
        candidate: CandidateGame,
        challenged_index: u64,
    ) -> eyre::Result<()> {
        let game_address = candidate.factory.proxy;
        let on_chain_root =
            self.verifier_client.intermediate_output_root(game_address, challenged_index).await?;
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

        self.proof_manager
            .initiate_zk_proof(
                self.submitter.sender_address(),
                candidate,
                challenged_index,
                validation.expected_root,
                DisputeIntent::Nullify,
            )
            .await
    }
}
